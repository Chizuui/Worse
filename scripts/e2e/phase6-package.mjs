#!/usr/bin/env node
// Phase 6 packaging gate (Windows): build -> silent per-user install -> launch without node on
// PATH -> Host starts on the bundled runtime -> graceful exit -> silent uninstall -> no leftovers.
// Prints PHASE6_PACKAGE_OK on success. Spec: Worse/docs/specs/harness/phase-6-packaging.md (A2, A4,
// A6, A7).
//
//   node Worse/scripts/e2e/phase6-package.mjs
//   HARNESS_PHASE6_INSTALLER=<setup.exe>  reuse an installer instead of building one
//   HARNESS_PHASE6_VERSION=<semver>       version to build (default 0.6.0)
//
// Writes only under Worse/.work/tmp (install dir, isolated home, WebView2 profile) plus what a
// per-user NSIS install writes to HKCU, which the gate records and requires the uninstaller to
// remove. It refuses to run when a real Harness install is registered. It never touches processes
// it did not start (process checks are scoped to the temporary install directory).
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { createWriteStream, existsSync, watch } from "node:fs";
import { mkdir, readdir, rm, stat, writeFile } from "node:fs/promises";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

// 必须在启动任何子进程之前读取真实环境：构建（cargo/rustup/pnpm）需要真实的用户目录。
const realEnv = { ...process.env };
const { installIsolatedHome } = await import("./lib/isolatedHome.mjs");
const { home } = installIsolatedHome("phase6-package");

const worseRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const repoRoot = path.resolve(worseRoot, "..");
const tmpRoot = path.join(worseRoot, ".work/tmp");
const reportDir = path.join(worseRoot, ".work/phase6");
const installDir = path.join(tmpRoot, `phase6-install-${process.pid}`);
const VERSION = process.env.HARNESS_PHASE6_VERSION?.trim() || "0.6.0";
const UNINSTALL_KEY = "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Harness";
// 修复原因（门禁第 4 次运行）：同一厂商键下还有 Phase 6B 更新门禁的 HarnessE2EUpdater 产品键，
// 两个门禁并发时比较整个 HKCU\Software\Harness 会误判；只断言本产品的键。
const PRODUCT_KEY = "HKCU\\Software\\Harness\\Harness";
const SYSTEM32 = path.join(process.env.SystemRoot ?? "C:\\Windows", "System32");
const POWERSHELL = path.join(SYSTEM32, "WindowsPowerShell", "v1.0", "powershell.exe");

const summary = { version: VERSION, installDir, steps: [], hkcuWritten: [], notes: [] };
const started = new Set();
let installed = false;

class GateError extends Error {
  constructor(step, message) {
    super(`${step}: ${message}`);
    this.step = step;
  }
}

function step(name, detail = {}) {
  summary.steps.push({ name, ...detail });
  process.stdout.write(`[phase6-package] ${name}${Object.keys(detail).length ? ` ${JSON.stringify(detail)}` : ""}\n`);
}

function run(command, args, { env = process.env, cwd, input, inherit = false } = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, {
      env,
      cwd,
      windowsHide: true,
      stdio: inherit ? ["ignore", "inherit", "inherit"] : ["pipe", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    child.stdout?.setEncoding("utf8").on("data", (chunk) => (stdout += chunk));
    child.stderr?.setEncoding("utf8").on("data", (chunk) => (stderr += chunk));
    child.once("error", reject);
    child.once("close", (code) => resolve({ code: code ?? 1, stdout, stderr }));
    if (child.stdin) child.stdin.end(input ?? "");
  });
}

/** Windows PowerShell 5.1 with its own module path (PSModulePath from pwsh 7 breaks autoload). */
async function ps(script, input, baseEnv = process.env) {
  const env = { ...baseEnv };
  delete env.PSModulePath;
  const result = await run(POWERSHELL, ["-NoProfile", "-NonInteractive", "-Command", script], { env, input });
  if (result.code !== 0) throw new GateError("POWERSHELL", result.stderr.trim().slice(0, 1000));
  return result.stdout.trim();
}

async function regExists(key) {
  return (await run(path.join(SYSTEM32, "reg.exe"), ["query", key])).code === 0;
}

async function regQuery(key) {
  const result = await run(path.join(SYSTEM32, "reg.exe"), ["query", key, "/s"]);
  return result.code === 0 ? result.stdout.trim() : null;
}

/**
 * Real per-user shortcut paths NSIS would use ($SMPROGRAMS / $DESKTOP; known folders, not env).
 * 只读查询用真实环境运行：known folder 的注册表值含 %USERPROFILE% 等变量，隔离环境下展开结果为空。
 */
async function shortcutPaths() {
  const out = await ps(
    "[Environment]::GetFolderPath('Programs'); [Environment]::GetFolderPath('Desktop'); [Environment]::GetFolderPath('LocalApplicationData'); [Environment]::GetFolderPath('ApplicationData')",
    undefined,
    realEnv,
  );
  if (out.split(/\r?\n/).length < 4) throw new GateError("PRECONDITION", `could not resolve known folders: ${out}`);
  const [programs, desktop, localAppData, appData] = out.split(/\r?\n/);
  return {
    shortcuts: [path.join(programs, "Harness.lnk"), path.join(programs, "Harness", "Harness.lnk"), path.join(desktop, "Harness.lnk")],
    shellData: [path.join(localAppData, "dev.harness.app"), path.join(appData, "dev.harness.app")],
  };
}

/** Processes whose image lives under the temporary install directory. */
async function ownedProcesses() {
  const script =
    "$root = [Console]::In.ReadToEnd().Trim().TrimEnd('\\') + '\\'; " +
    "@(Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith($root, [StringComparison]::OrdinalIgnoreCase) } | " +
    "ForEach-Object { [pscustomobject]@{ pid = $_.ProcessId; parent = $_.ParentProcessId; name = $_.Name; path = $_.ExecutablePath } }) | ConvertTo-Json -Compress";
  const out = await ps(script, installDir);
  if (!out) return [];
  const parsed = JSON.parse(out);
  return Array.isArray(parsed) ? parsed : [parsed];
}

function waitForExit(child, ms) {
  if (child.exitCode !== null || child.signalCode !== null) return Promise.resolve(true);
  return new Promise((resolve) => {
    const timer = setTimeout(() => resolve(false), ms);
    child.once("exit", () => {
      clearTimeout(timer);
      resolve(true);
    });
  });
}

/** Resolves when `predicate()` turns true, re-checking on changes in `dir` (event-driven, bounded). */
async function waitForPath(dir, predicate, ms) {
  const deadline = Date.now() + ms;
  while (!(await predicate())) {
    const left = deadline - Date.now();
    if (left <= 0) return false;
    await new Promise((resolve) => {
      let watcher;
      const done = () => {
        clearTimeout(timer);
        watcher?.close();
        resolve();
      };
      const timer = setTimeout(done, Math.min(left, 2_000));
      try {
        watcher = watch(dir, { recursive: true }, done);
      } catch {
        // 目录不存在时只按超时重查。
      }
    });
  }
  return true;
}

/** PATH without any directory that provides node (node.exe / node.cmd shims). */
function pathWithoutNode(env) {
  const key = Object.keys(env).find((name) => name.toUpperCase() === "PATH") ?? "Path";
  const entries = (env[key] ?? "").split(path.delimiter).filter(Boolean);
  const removed = [];
  const kept = entries.filter((entry) => {
    const hasNode = ["node.exe", "node.cmd", "node.bat", "node"].some((name) => existsSync(path.join(entry, name)));
    if (hasNode) removed.push(entry);
    return !hasNode;
  });
  return { key, value: kept.join(path.delimiter), removed };
}

async function buildInstaller() {
  const explicit = process.env.HARNESS_PHASE6_INSTALLER?.trim();
  if (explicit) {
    if (!existsSync(explicit)) throw new GateError("BUILD", `HARNESS_PHASE6_INSTALLER=${explicit} does not exist`);
    step("build skipped (HARNESS_PHASE6_INSTALLER)", { installer: explicit });
    return explicit;
  }
  const env = { ...realEnv, HARNESS_ALLOW_UNSIGNED: "1" };
  for (const key of Object.keys(env)) {
    if (/^HARNESS_WINDOWS_|^HARNESS_SIGN/.test(key)) delete env[key];
  }
  const started = Date.now();
  const child = spawn(process.execPath, [path.join(worseRoot, "scripts/package/build-installer.mjs"), "--version", VERSION], {
    cwd: repoRoot,
    env,
    stdio: ["ignore", "pipe", "inherit"],
    windowsHide: true,
  });
  let tail = "";
  child.stdout.setEncoding("utf8").on("data", (chunk) => {
    process.stdout.write(chunk);
    tail = (tail + chunk).slice(-20_000);
  });
  const code = await new Promise((resolve) => child.once("close", resolve));
  if (code !== 0) throw new GateError("BUILD", `build-installer.mjs exited with ${code}`);
  const line = tail.split(/\r?\n/).reverse().find((entry) => entry.startsWith("HARNESS_BUNDLE_OK "));
  if (!line) throw new GateError("BUILD", "no HARNESS_BUNDLE_OK line");
  if (!/UNSIGNED BUILD/.test(tail)) throw new GateError("BUILD", "unsigned release build did not announce UNSIGNED BUILD");
  const result = JSON.parse(line.slice("HARNESS_BUNDLE_OK ".length));
  const nsis = result.artifacts.find((artifact) => artifact.bundle === "nsis");
  if (!nsis) throw new GateError("BUILD", "no nsis artifact");
  summary.installerBytes = nsis.bytes;
  step("built installer", { path: nsis.path, bytes: nsis.bytes, seconds: Math.round((Date.now() - started) / 1000) });
  return nsis.path;
}

/**
 * Launches the installed shell, waits for the Host, checks the Node runtime it runs on, then closes
 * the window gracefully. Returns timings and observations.
 */
async function launchAndClose(label, extraEnv, { expectBundled }) {
  const env = { ...process.env, ...extraEnv };
  for (const key of ["HARNESS_NODE_BIN", "HARNESS_HOST_ENTRY", "HARNESS_RUST_HOST", "HARNESS_REPO_ROOT"]) {
    if (!(key in extraEnv)) delete env[key];
  }
  const exe = path.join(installDir, "harness-app.exe");
  const logFile = path.join(reportDir, `package-${label}.log`);
  const log = createWriteStream(logFile);
  const t0 = performance.now();
  const child = spawn(exe, [], { cwd: home, env, stdio: ["ignore", "pipe", "pipe"], windowsHide: false });
  started.add(child);
  let output = "";
  let readyAt = null;
  const ready = new Promise((resolve) => {
    const onData = (chunk) => {
      log.write(chunk);
      output += chunk.toString();
      if (readyAt === null && /Host sidecar ready/.test(output)) {
        readyAt = performance.now();
        resolve(true);
      }
    };
    child.stdout.on("data", onData);
    child.stderr.on("data", onData);
    child.once("exit", () => resolve(false));
    setTimeout(() => resolve(false), 180_000);
  });
  try {
    if (!(await ready)) {
      throw new GateError(`LAUNCH_${label}`, `Host did not report ready (see ${logFile}); tail: ${output.slice(-1500)}`);
    }
    const nodeLine = output.split(/\r?\n/).find((line) => line.includes("using the Node.js runtime")) ?? "";
    const processes = await ownedProcesses();
    const hostNode = processes.find((entry) => entry.name.toLowerCase() === "node.exe");
    const observation = {
      label,
      hostReadyMs: Math.round(readyAt - t0),
      nodeLogLine: nodeLine.trim().slice(0, 400),
      processesUnderInstallDir: processes.map((entry) => `${entry.name} (${path.relative(installDir, entry.path)})`),
    };
    if (expectBundled) {
      if (!hostNode || !hostNode.path.toLowerCase().endsWith("\\host\\runtime\\node\\node.exe")) {
        throw new GateError(`NODE_${label}`, `Host node.exe is not the bundled runtime: ${JSON.stringify(processes)}`);
      }
      if (!/Bundled/.test(nodeLine)) throw new GateError(`NODE_${label}`, `log does not name the bundled source: ${nodeLine}`);
    } else if (hostNode) {
      throw new GateError(`NODE_${label}`, "dev-layout run unexpectedly used the bundled runtime");
    }

    // 优雅退出：向主窗口发送 WM_CLOSE，壳的关闭屏障会先释放 Host 再退出。
    const closed = await ps(`$p = Get-Process -Id ${child.pid} -ErrorAction SilentlyContinue; if ($p) { $p.CloseMainWindow() } else { 'gone' }`);
    const exited = await waitForExit(child, 30_000);
    if (!exited) throw new GateError(`EXIT_${label}`, `shell did not exit within 30 s after CloseMainWindow (${closed})`);
    observation.exitCode = child.exitCode;
    const leftovers = await ownedProcesses();
    if (leftovers.length > 0) throw new GateError(`ORPHANS_${label}`, `processes left under the install dir: ${JSON.stringify(leftovers)}`);
    step(`launch ${label}`, observation);
    return observation;
  } finally {
    log.end();
    started.delete(child);
    if (child.exitCode === null) child.kill();
  }
}

async function uninstall() {
  const uninstaller = path.join(installDir, "uninstall.exe");
  const result = await run(uninstaller, ["/S"], { cwd: tmpRoot });
  // 卸载程序会把自己复制到 %TEMP% 重新启动并立即返回，所以等待安装目录消失。
  const gone = await waitForPath(tmpRoot, async () => !existsSync(installDir), 120_000);
  installed = !gone;
  return { exit: result.code, gone };
}

async function listRemaining(dir) {
  const entries = await readdir(dir, { recursive: true }).catch(() => []);
  return entries.slice(0, 20);
}

async function main() {
  if (process.platform !== "win32") throw new GateError("PLATFORM", "the packaging gate verifies the Windows NSIS installer");
  await mkdir(reportDir, { recursive: true });
  if (await regExists(UNINSTALL_KEY)) {
    throw new GateError("PRECONDITION", `${UNINSTALL_KEY} exists: a real Harness install is registered; uninstall it first`);
  }
  if (await regExists(PRODUCT_KEY)) {
    throw new GateError("PRECONDITION", `${PRODUCT_KEY} exists: a previous Harness install left its install-location key`);
  }
  const { shortcuts, shellData } = await shortcutPaths();
  const shortcutsBefore = shortcuts.filter((file) => existsSync(file));
  const shellDataBefore = shellData.filter((dir) => existsSync(dir));
  step("preconditions", { productKeyBefore: false, shortcutsBefore, shellDataBefore });

  const installer = await buildInstaller();

  // ---- install (per-user, silent, no shortcuts, temp dir) ----
  await rm(installDir, { recursive: true, force: true });
  const t0 = performance.now();
  const install = await run(installer, ["/S", "/NS", `/D=${installDir}`], { cwd: tmpRoot });
  installed = existsSync(installDir);
  if (install.code !== 0) throw new GateError("INSTALL", `installer exited with ${install.code}`);
  for (const file of ["harness-app.exe", "uninstall.exe", "host/runtime/node/node.exe", "host/runtime/node/LICENSE", "host/harness-host-manifest.json"]) {
    if (!existsSync(path.join(installDir, file))) throw new GateError("INSTALL", `missing ${file} in ${installDir}`);
  }
  const uninstallKey = await regQuery(UNINSTALL_KEY);
  const productKey = await regExists(PRODUCT_KEY);
  if (!uninstallKey) throw new GateError("INSTALL", `${UNINSTALL_KEY} was not written`);
  if (!uninstallKey.toLowerCase().includes(installDir.toLowerCase())) {
    throw new GateError("INSTALL", "uninstall entry does not point at the temporary install dir");
  }
  summary.hkcuWritten.push(UNINSTALL_KEY);
  if (productKey) summary.hkcuWritten.push(PRODUCT_KEY);
  const shortcutsAfterInstall = shortcuts.filter((file) => existsSync(file) && !shortcutsBefore.includes(file));
  if (shortcutsAfterInstall.length > 0) throw new GateError("INSTALL", `/NS install created shortcuts: ${shortcutsAfterInstall.join(", ")}`);
  let installBytes = 0;
  for (const entry of await readdir(installDir, { recursive: true, withFileTypes: true })) {
    if (entry.isFile()) installBytes += (await stat(path.join(entry.parentPath, entry.name))).size;
  }
  step("installed", {
    seconds: Math.round((performance.now() - t0) / 100) / 10,
    installMiB: Math.round(installBytes / 104857.6) / 10,
    hkcuWritten: summary.hkcuWritten,
    programFiles: installDir.toLowerCase().includes("program files"),
  });

  // ---- A7: custom data directory from setting.json ----
  const customData = path.join(home, "custom-data");
  await mkdir(path.join(home, ".zcode", "v2"), { recursive: true });
  await mkdir(customData, { recursive: true });
  await writeFile(path.join(home, ".zcode", "v2", "setting.json"), `${JSON.stringify({ dataBaseDir: customData }, null, 2)}\n`);

  const strippedPath = pathWithoutNode(process.env);
  summary.pathEntriesRemoved = strippedPath.removed;
  const baseEnv = {
    [strippedPath.key]: strippedPath.value,
    WEBVIEW2_USER_DATA_FOLDER: path.join(home, "webview2"),
    HARNESS_DEVICE_MID: `phase6-package-${randomUUID()}`,
    HARNESS_LOCALE: "en-US",
    // 不访问真实 ZCode 云端：登录页之外的请求指向本机不可达端口。
    ZCODE_ENDPOINT_ORIGIN: "http://127.0.0.1:9",
  };

  const cold = await launchAndClose("packaged-cold", baseEnv, { expectBundled: true });
  const dataCreated = await waitForPath(customData, async () => existsSync(path.join(customData, ".zcode")), 1_000);
  if (!dataCreated) throw new GateError("DATA_DIR", `Host did not create ${path.join(customData, ".zcode")} from setting.json dataBaseDir`);
  step("custom data dir honoured", { dir: path.join(customData, ".zcode") });
  const warm = await launchAndClose("packaged-warm", baseEnv, { expectBundled: true });
  // Dev layout for comparison: same shell binary, checkout layout, Node from the real PATH.
  const dev = await launchAndClose(
    "dev-layout",
    { ...baseEnv, [strippedPath.key]: process.env[strippedPath.key], HARNESS_REPO_ROOT: repoRoot },
    { expectBundled: false },
  );
  summary.coldStart = { packagedColdMs: cold.hostReadyMs, packagedWarmMs: warm.hostReadyMs, devLayoutMs: dev.hostReadyMs };

  // ---- uninstall ----
  const removed = await uninstall();
  if (!removed.gone) {
    throw new GateError("UNINSTALL", `install dir still exists after 120 s: ${JSON.stringify(await listRemaining(installDir))}`);
  }
  const leftovers = await ownedProcesses();
  if (leftovers.length > 0) throw new GateError("UNINSTALL", `processes left: ${JSON.stringify(leftovers)}`);
  const uninstallKeyAfter = await regExists(UNINSTALL_KEY);
  if (uninstallKeyAfter) throw new GateError("UNINSTALL", `${UNINSTALL_KEY} still exists`);
  if (await regExists(PRODUCT_KEY)) throw new GateError("UNINSTALL", `${PRODUCT_KEY} still exists`);
  const shortcutsAfter = shortcuts.filter((file) => existsSync(file) && !shortcutsBefore.includes(file));
  if (shortcutsAfter.length > 0) throw new GateError("UNINSTALL", `shortcuts left: ${shortcutsAfter.join(", ")}`);
  for (const kept of [path.join(home, ".zcode", "v2", "setting.json"), path.join(customData, ".zcode")]) {
    if (!existsSync(kept)) throw new GateError("UNINSTALL", `user data was removed: ${kept}`);
  }
  summary.shellDataAfter = shellData.filter((dir) => existsSync(dir));
  if (summary.shellDataAfter.some((dir) => !shellDataBefore.includes(dir))) {
    summary.notes.push(
      `shell data created outside the isolated home (Tauri resolves app data/log dirs through known folders): ${summary.shellDataAfter.join(", ")}`,
    );
  }
  step("uninstalled", { registryRemoved: true, userDataKept: true, shellDataDirs: summary.shellDataAfter });
}

try {
  await main();
  await writeFile(path.join(reportDir, "package-gate.json"), `${JSON.stringify(summary, null, 2)}\n`);
  process.stdout.write(`PHASE6_PACKAGE_OK ${JSON.stringify({ installerBytes: summary.installerBytes, coldStart: summary.coldStart, hkcuWritten: summary.hkcuWritten })}\n`);
} catch (error) {
  summary.error = error.message;
  process.stderr.write(`${error.stack}\n`);
  process.stdout.write(`PHASE6_PACKAGE_FAIL ${error.message}\n`);
  process.exitCode = 1;
  for (const child of started) if (child.exitCode === null) child.kill();
  if (installed && existsSync(path.join(installDir, "uninstall.exe"))) {
    const cleanup = await uninstall().catch((cleanupError) => ({ error: cleanupError.message }));
    process.stdout.write(`cleanup uninstall: ${JSON.stringify(cleanup)}\n`);
  }
  await writeFile(path.join(reportDir, "package-gate.json"), `${JSON.stringify(summary, null, 2)}\n`).catch(() => {});
}
