#!/usr/bin/env node
// Phase 6 (agent B) updater gate: signed N -> N+1 update of a per-user NSIS install, end to end.
// Spec: Worse/docs/specs/harness/phase-6-updater.md (acceptance U1-U8).
//
//   node Worse/scripts/e2e/phase6-update.mjs [--reuse]
//
// Windows only. Steps:
//   1. TEST updater keypair (+ an "attacker" keypair) under Worse/.work/tmp/phase6-update/keys.
//   2. Build N (full pipeline through scripts/package/build-installer.mjs, HARNESS_ALLOW_UNSIGNED=1
//      for Authenticode) and N+1 (same command, `build.beforeBuildCommand` removed, so only the
//      shell is recompiled with the bumped version and re-bundled). Both carry a test-only config
//      override: identifier dev.harnesse2e.updater, productName HarnessE2EUpdater, binary
//      harness-e2e-updater (so Tauri's "kill running app by name" never touches a real Harness),
//      the TEST public key, createUpdaterArtifacts, dangerousInsecureTransportProtocol (loopback).
//   3. Loopback HTTP feed (latest.json + artifacts) with switchable modes.
//   4. Silent per-user install of N into Worse/.work/tmp (/S /NS /D=), launch with the feed env,
//      drive the shell through the WebView2 DevTools endpoint (updater_call / updater events).
//   5. Negatives on N: 204, downgrade, same version, wrong platform, tampered bytes, foreign key
//      signature, relabelled version -> refused, still N, never "downloaded".
//   6. Positive: check -> download -> quitAndInstall; Host drained before the installer; the
//      installer relaunches N+1; assert version, release notes, no orphan processes.
//   7. Graceful close, silent uninstall, cleanup of HKCU keys, app dirs, temp installers, keys.
//
// --reuse: skip the builds when artifacts and keys from a kept run exist (PHASE6_UPDATE_KEEP_KEYS=1).
// Prints PHASE6_UPDATE_OK on success.
import { spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { existsSync } from "node:fs";
import { copyFile, mkdir, readFile, readdir, rm, stat, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";
import { installIsolatedHome } from "./lib/isolatedHome.mjs";
import { callAndAwaitEvent, closeGracefully, connectCdp, diagnostics, findAppPage, updater } from "./phase6-update/cdp.mjs";
import { BINARY, IDENTIFIER, PRODUCT, PRODUCT_KEY, PUBLISHER_KEY, UNINSTALL_KEY, VERSION_N, VERSION_N1 } from "./phase6-update/constants.mjs";
import { manifestFor, startFeed } from "./phase6-update/feed.mjs";
import { StepError, freePort, pidAlive, powershellJson, regDelete, regExists, regValues, runSync, sleep, waitUntil } from "./phase6-update/system.mjs";
import { decoyMismatches, plantDecoys, removeDecoys, updaterTempDirs as productTempDirs } from "./phase6-update/tempDirs.mjs";

// 构建工具（cargo/rustup/pnpm/NSIS 缓存）需要真实的用户目录；只在隔离之前读取环境，不启动任何进程。
const buildBaseEnv = { ...process.env };
const { home } = installIsolatedHome("phase6-update");

const moduleDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(moduleDir, "../../..");
const worseRoot = path.join(repoRoot, "Worse");
const appDir = path.join(worseRoot, "apps/harness-app");
const workDir = path.join(worseRoot, ".work/tmp/phase6-update");
const keyDir = path.join(workDir, "keys");
const artifactDir = path.join(workDir, "artifacts");
const installBacking = path.join(workDir, "install");
const backingInstallDir = path.join(installBacking, "HarnessE2EUpdater");
// 修复原因：gate 把 USERPROFILE 改写到隔离 home；此时 ShellExecuteW 对真实用户目录（C:\Users\<user>\...）
// 下的任意 exe 返回 SE_ERR_PATHNOTFOUND(3)（用 notepad 副本探针复现；C:\Users\Public 或 subst 盘符下正常）。
// NSIS 的 RunAsUser 在非管理员下回退到 ShellExecuteW，所以安装器装完 N+1 后无法重启应用（run3/run4 的 RELAUNCH 失败）。
// 因此把 .work/tmp/phase6-update/install 用 subst 映射成临时盘符，安装、运行与重启都走该盘符，cleanup 中 subst /D 移除。
// 真实用户环境（USERPROFILE 未改写）下 ShellExecuteW 正常（rc=42），产品行为不受影响。
let installDrive = null;
let installDir = backingInstallDir;
const logCopyDir = path.join(workDir, "logs");
const requireFromRepo = createRequire(path.join(repoRoot, "package.json"));
const realAppData = buildBaseEnv.APPDATA;
const realLocalAppData = buildBaseEnv.LOCALAPPDATA;
const appDataDirs = [path.join(realAppData, IDENTIFIER), path.join(realLocalAppData, IDENTIFIER)];

const reuse = process.argv.includes("--reuse");
const keepKeys = process.env.PHASE6_UPDATE_KEEP_KEYS === "1";
const report = { versions: { n: VERSION_N, n1: VERSION_N1 }, negatives: [], notes: [], writes: {} };
const startedChildren = [];
/** Updater temp dirs that existed before the run (left alone) and the planted decoys (U9). */
let tempBefore = [];
let decoys = [];

function log(message) {
  process.stdout.write(`[phase6-update] ${message}\n`);
}

/** Processes whose image or command line lives under the gate's install dir. */
function installProcesses() {
  return powershellJson(
    "$roots=@($env:P6_ROOT,$env:P6_ROOT2) | ForEach-Object { [System.IO.Path]::GetFullPath($_).TrimEnd('\\')+'\\' };" +
      "function Hit($s){ if(-not $s){ return $false }; foreach($r in $roots){ if($s.IndexOf($r,[System.StringComparison]::OrdinalIgnoreCase) -ge 0){ return $true } }; return $false };" +
      "Get-CimInstance Win32_Process | Where-Object { (Hit $_.ExecutablePath) -or (Hit $_.CommandLine) } |" +
      " ForEach-Object { [pscustomobject]@{ pid=$_.ProcessId; ppid=$_.ParentProcessId; name=$_.Name; path=$_.ExecutablePath; created=$_.CreationDate.ToUniversalTime().ToString('o') } } | ConvertTo-Json -Compress",
    { P6_ROOT: installDir, P6_ROOT2: backingInstallDir },
  );
}

function shortcutPaths() {
  const [folders] = powershellJson(
    "[pscustomobject]@{ programs=[Environment]::GetFolderPath('Programs'); desktop=[Environment]::GetFolderPath('DesktopDirectory') } | ConvertTo-Json -Compress",
  );
  return [path.join(folders.programs, `${PRODUCT}.lnk`), path.join(folders.desktop, `${PRODUCT}.lnk`)];
}

const updaterTempDirs = () => productTempDirs(PRODUCT);

// ---------------------------------------------------------------- keys and builds

function tauriCli() {
  return requireFromRepo.resolve("@tauri-apps/cli/tauri.js");
}

async function generateKey(name, password) {
  const file = path.join(keyDir, `${name}.key`);
  const result = runSync(process.execPath, [tauriCli(), "signer", "generate", "--ci", "-p", password, "-w", file, "-f"]);
  // 输出里含私钥路径与公钥，不打印 stdout。
  if (result.code !== 0 || !existsSync(`${file}.pub`)) throw new StepError("KEYS", `signer generate ${name} failed (${result.code})`);
  return { file, pub: (await readFile(`${file}.pub`, "utf8")).trim(), password };
}

async function signWith(key, file, version) {
  const result = runSync(process.execPath, [
    tauriCli(), "signer", "sign", "-f", key.file, "-p", key.password, "--app-version", version, file,
  ]);
  if (result.code !== 0) throw new StepError("SIGN", `signer sign failed (${result.code}): ${result.stderr.trim().slice(0, 300)}`);
  return (await readFile(`${file}.sig`, "utf8")).trim();
}

function trustedComment(signature) {
  const text = Buffer.from(signature, "base64").toString("utf8");
  return text.split("\n").find((line) => line.startsWith("trusted comment:"))?.slice("trusted comment:".length).trim() ?? "";
}

async function overrideConfig(pubkey, shellOnly) {
  const config = {
    identifier: IDENTIFIER,
    productName: PRODUCT,
    mainBinaryName: BINARY,
    bundle: { createUpdaterArtifacts: true },
    plugins: {
      updater: {
        pubkey,
        endpoints: [],
        requireSignedVersion: true,
        // 仅测试构建：允许回环 http feed；生产配置不含该项（spec 第 3 节）。
        dangerousInsecureTransportProtocol: true,
        windows: { installMode: "passive" },
      },
    },
  };
  // N+1 只需用新版本号重新编译 shell 并重新打包；前端 dist 与 resources/host 沿用 N 的构建产物。
  // JSON merge patch：null 删除平台配置里的 beforeBuildCommand。
  if (shellOnly) config.build = { beforeBuildCommand: null };
  const file = path.join(workDir, `tauri.e2e.${shellOnly ? "shell" : "full"}.json`);
  await writeFile(file, `${JSON.stringify(config, null, 2)}\n`);
  return file;
}

function runBuild(version, configFile, key) {
  const args = [path.join(worseRoot, "scripts/package/build-installer.mjs"), "--version", version, "--bundles", "nsis", "--config", configFile];
  const env = { ...buildBaseEnv, HARNESS_ALLOW_UNSIGNED: "1", TAURI_SIGNING_PRIVATE_KEY_PATH: "", TAURI_SIGNING_PRIVATE_KEY_PASSWORD: key.password };
  delete env.TAURI_SIGNING_PRIVATE_KEY_PATH;
  return readFile(key.file, "utf8").then(
    (privateKey) =>
      new Promise((resolve, reject) => {
        env.TAURI_SIGNING_PRIVATE_KEY = privateKey.trim();
        log(`building ${version}: node ${args.map((arg) => path.relative(repoRoot, arg) || arg).join(" ")}`);
        const started = Date.now();
        const child = spawn(process.execPath, args, { cwd: appDir, env, windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
        let tail = "";
        let summary = null;
        const onData = (chunk) => {
          const text = chunk.toString();
          tail = (tail + text).slice(-4000);
          for (const line of text.split(/\r?\n/)) {
            if (line.startsWith("HARNESS_BUNDLE_OK ")) summary = JSON.parse(line.slice("HARNESS_BUNDLE_OK ".length));
          }
        };
        child.stdout.on("data", onData);
        child.stderr.on("data", onData);
        child.once("error", reject);
        child.once("close", (code) => {
          if (code !== 0 || !summary) {
            reject(new StepError("BUILD", `build ${version} failed (${String(code)}); tail:\n${tail}`));
          } else {
            resolve({ summary, seconds: Math.round((Date.now() - started) / 1000) });
          }
        });
      }),
  );
}

async function buildRelease(version, configFile, key) {
  // build-installer.mjs 的构建锁已改为原子创建 + token 校验释放（scripts/package/buildLock.mjs），不再重试。
  const { summary, seconds } = await runBuild(version, configFile, key);
  const setup = summary.artifacts.find((artifact) => artifact.path.endsWith("-setup.exe"));
  const sig = summary.artifacts.find((artifact) => artifact.path.endsWith("-setup.exe.sig"));
  if (!setup || !sig) throw new StepError("BUILD", `build ${version} produced no setup.exe + .sig: ${JSON.stringify(summary.artifacts)}`);
  const dir = path.join(artifactDir, version);
  await mkdir(dir, { recursive: true });
  const setupCopy = path.join(dir, path.basename(setup.path));
  await copyFile(setup.path, setupCopy);
  await copyFile(sig.path, `${setupCopy}.sig`);
  return { setup: setupCopy, bytes: (await stat(setupCopy)).size, seconds, signed: summary.signed };
}

async function prepareArtifacts() {
  const statePath = path.join(workDir, "artifacts.json");
  if (reuse && existsSync(statePath) && existsSync(path.join(keyDir, "updater.key"))) {
    const saved = JSON.parse(await readFile(statePath, "utf8"));
    const keys = {
      updater: { file: path.join(keyDir, "updater.key"), pub: (await readFile(path.join(keyDir, "updater.key.pub"), "utf8")).trim(), password: saved.password },
      attacker: { file: path.join(keyDir, "attacker.key"), pub: "", password: saved.password },
    };
    if (existsSync(saved.n.setup) && existsSync(saved.n1.setup)) {
      log("reusing artifacts from a previous run (--reuse)");
      return { keys, n: saved.n, n1: saved.n1, reused: true };
    }
  }
  await rm(keyDir, { recursive: true, force: true });
  await rm(artifactDir, { recursive: true, force: true });
  await mkdir(keyDir, { recursive: true });
  const password = randomBytes(18).toString("base64url");
  const keys = { updater: await generateKey("updater", password), attacker: await generateKey("attacker", password) };
  const n = await buildRelease(VERSION_N, await overrideConfig(keys.updater.pub, false), keys.updater);
  const n1 = await buildRelease(VERSION_N1, await overrideConfig(keys.updater.pub, true), keys.updater);
  await writeFile(statePath, JSON.stringify({ n, n1, password }, null, 2));
  return { keys, n, n1, reused: false };
}

// ---------------------------------------------------------------- loopback feed

// ---------------------------------------------------------------- app driving

function appEnv(cdpPort, feedUrl) {
  return {
    ...process.env,
    HARNESS_UPDATE_FEED_URL: feedUrl,
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${cdpPort}`,
    WEBVIEW2_USER_DATA_FOLDER: path.join(home, "webview2"),
    HARNESS_APP_DATA_DIR: path.join(home, "harness-app-data"),
    RUST_LOG: "info",
  };
}

async function waitForHost(timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const hosts = installProcesses().filter((entry) => /^node(\.exe)?$/i.test(entry.name ?? ""));
    if (hosts.length > 0) return hosts;
    await sleep(1_000);
  }
  throw new StepError("HOST_START", "the Host (node.exe under the install dir) never started");
}

async function readAppLogs() {
  // 修复原因：Tauri 的 app_log_dir 在 Windows 上跟随进程的 LOCALAPPDATA 环境变量（探针确认日志写进隔离 home），
  // 只读真实 %LOCALAPPDATA% 会找不到安装日志；两处都读，隔离 home 优先。
  const dirs = [path.join(process.env.LOCALAPPDATA ?? realLocalAppData, IDENTIFIER, "logs"), path.join(realLocalAppData, IDENTIFIER, "logs")];
  const chunks = [];
  await mkdir(logCopyDir, { recursive: true });
  for (const dir of [...new Set(dirs)]) {
    const names = await readdir(dir).catch(() => []);
    for (const name of names.sort()) {
      chunks.push(await readFile(path.join(dir, name), "utf8").catch(() => ""));
      await copyFile(path.join(dir, name), path.join(logCopyDir, name)).catch(() => {});
    }
  }
  return chunks.join("\n");
}

// ---------------------------------------------------------------- install / uninstall

async function installN(setup) {
  if (regExists(UNINSTALL_KEY) || regExists(PRODUCT_KEY)) {
    throw new StepError("PRECONDITION", `${UNINSTALL_KEY} or ${PRODUCT_KEY} already exists; a previous run was not cleaned up`);
  }
  await rm(backingInstallDir, { recursive: true, force: true });
  await mkdir(installBacking, { recursive: true });
  mapInstallDrive();
  report.writes.publisherKeyExistedBefore = regExists(PUBLISHER_KEY);
  // /D 必须最后且不加引号（NSIS 约定）；/NS 不建开始菜单与桌面快捷方式。
  const result = runSync(setup, ["/S", "/NS", `/D=${installDir}`], { env: process.env });
  if (result.code !== 0) throw new StepError("INSTALL", `silent install exited ${result.code}`);
  const exe = path.join(installDir, `${BINARY}.exe`);
  if (!existsSync(exe)) throw new StepError("INSTALL", `${exe} missing after install`);
  report.writes.afterInstall = {
    uninstallKey: regValues(UNINSTALL_KEY),
    productKey: regValues(PRODUCT_KEY),
    shortcuts: shortcutPaths().filter((file) => existsSync(file)),
  };
  return exe;
}

/** Maps the install backing dir to a free drive letter (see the note at `installBacking`). */
function mapInstallDrive() {
  for (const letter of ["R", "Q", "P", "O", "N", "M", "L", "K"]) {
    if (existsSync(`${letter}:\\`)) continue;
    const result = runSync("subst", [`${letter}:`, installBacking]);
    if (result.code !== 0) continue;
    installDrive = `${letter}:`;
    installDir = path.join(`${installDrive}\\`, "HarnessE2EUpdater");
    report.writes.substDrive = `${installDrive} -> ${path.relative(repoRoot, installBacking)}`;
    return;
  }
  throw new StepError("PRECONDITION", "no free drive letter for subst");
}

function unmapInstallDrive() {
  if (!installDrive) return null;
  const result = runSync("subst", [installDrive, "/D"]);
  const drive = installDrive;
  if (result.code === 0) {
    installDrive = null;
    installDir = backingInstallDir;
  }
  return `subst ${drive} /D exited ${result.code}`;
}

async function uninstall() {
  const uninstaller = path.join(installDir, "uninstall.exe");
  if (!existsSync(uninstaller)) return "no uninstaller";
  const result = runSync(uninstaller, ["/S"], { env: process.env });
  // 卸载程序会把自己复制到 %TEMP% 后重新启动并立即返回；等待注册表项和安装目录消失。
  await waitUntil(
    () => !regExists(UNINSTALL_KEY) && !existsSync(path.join(installDir, `${BINARY}.exe`)),
    90_000,
    "UNINSTALL",
    "uninstall did not remove the uninstall key and the binary within 90 s",
  );
  return `uninstall.exe /S exited ${result.code}`;
}

async function cleanup() {
  const actions = [];
  // 只处理本 gate 安装目录下的进程（都是本 gate 启动的应用及其子进程）。
  for (const entry of (() => {
    try {
      return installProcesses();
    } catch {
      return [];
    }
  })()) {
    runSync("taskkill", ["/PID", String(entry.pid), "/T", "/F"]);
    actions.push(`killed leftover ${entry.name} ${entry.pid}`);
  }
  for (const child of startedChildren) {
    if (child.exitCode === null && child.pid) runSync("taskkill", ["/PID", String(child.pid), "/T", "/F"]);
  }
  try {
    actions.push(await uninstall());
  } catch (error) {
    actions.push(`uninstall: ${error.message}`);
  }
  for (const key of [UNINSTALL_KEY, PRODUCT_KEY]) {
    if (regExists(key)) {
      regDelete(key);
      actions.push(`deleted ${key}`);
    }
  }
  if (!report.writes.publisherKeyExistedBefore && regExists(PUBLISHER_KEY)) {
    const values = runSync("reg", ["query", PUBLISHER_KEY, "/s"]).stdout.trim().split(/\r?\n/).filter(Boolean);
    if (values.length <= 1) {
      regDelete(PUBLISHER_KEY);
      actions.push(`deleted empty ${PUBLISHER_KEY}`);
    }
  }
  for (const file of shortcutPaths()) {
    if (existsSync(file)) {
      await rm(file, { force: true });
      actions.push(`deleted ${file}`);
    }
  }
  await readAppLogs().catch(() => {});
  for (const dir of appDataDirs) {
    if (existsSync(dir)) {
      await rm(dir, { recursive: true, force: true, maxRetries: 6, retryDelay: 250 });
      actions.push(`deleted ${dir}`);
    }
  }
  // 只在失败路径上才会有本次运行的安装包目录残留（成功路径已由 U9 断言为空）；诱饵单独删除。
  for (const name of updaterTempDirs()) {
    if (tempBefore.includes(name) || decoys.some((decoy) => decoy.name === name)) continue;
    await rm(path.join(os.tmpdir(), name), { recursive: true, force: true }).catch(() => {});
    actions.push(`deleted %TEMP%\\${name}`);
  }
  await removeDecoys(decoys);
  const unmapped = unmapInstallDrive();
  if (unmapped) actions.push(unmapped);
  await rm(installBacking, { recursive: true, force: true, maxRetries: 6, retryDelay: 250 }).catch(() => {});
  if (!keepKeys) {
    await rm(keyDir, { recursive: true, force: true });
    actions.push("deleted test keys");
  } else {
    actions.push(`kept test keys in ${path.relative(repoRoot, keyDir)} (PHASE6_UPDATE_KEEP_KEYS=1)`);
  }
  const leftovers = {
    uninstallKey: regExists(UNINSTALL_KEY),
    productKey: regExists(PRODUCT_KEY),
    shortcuts: shortcutPaths().filter((file) => existsSync(file)),
    appDataDirs: appDataDirs.filter((dir) => existsSync(dir)),
    installDir: existsSync(backingInstallDir),
    substDrive: installDrive,
    processes: (() => {
      try {
        return installProcesses().length;
      } catch {
        return -1;
      }
    })(),
  };
  return { actions, leftovers };
}

// ---------------------------------------------------------------- main

async function main() {
  if (process.platform !== "win32") {
    log("SKIP: the updater gate needs Windows (NSIS per-user install)");
    process.stdout.write("PHASE6_UPDATE_SKIPPED non-windows\n");
    return;
  }
  await mkdir(workDir, { recursive: true });
  let result = null;
  let failure = null;
  try {
    tempBefore = updaterTempDirs();
    decoys = await plantDecoys(PRODUCT);
    tempBefore = tempBefore.filter((name) => !decoys.some((decoy) => decoy.name === name));
    result = await runScenario();
  } catch (error) {
    failure = error;
  }
  const cleaned = await cleanup();
  report.cleanup = cleaned;
  await writeFile(path.join(workDir, "report.json"), `${JSON.stringify(report, null, 2)}\n`);
  log(`cleanup: ${cleaned.actions.join("; ")}`);
  const leftover = cleaned.leftovers;
  const dirty =
    leftover.uninstallKey || leftover.productKey || leftover.shortcuts.length > 0 || leftover.appDataDirs.length > 0 || leftover.installDir || leftover.substDrive !== null || leftover.processes !== 0;
  if (failure) {
    process.stdout.write(`PHASE6_UPDATE_FAIL ${failure.step ?? "ERROR"}: ${failure.message}\n`);
    process.exitCode = 1;
    return;
  }
  if (dirty) {
    process.stdout.write(`PHASE6_UPDATE_FAIL CLEANUP: leftovers ${JSON.stringify(leftover)}\n`);
    process.exitCode = 1;
    return;
  }
  process.stdout.write(`${JSON.stringify(result, null, 2)}\n`);
  process.stdout.write("PHASE6_UPDATE_OK\n");
}

async function runScenario() {
  const { chromium } = requireFromRepo("playwright-core");
  const built = await prepareArtifacts();
  report.build = { reused: built.reused, n: built.n, n1: built.n1 };
  const n1Signature = (await readFile(`${built.n1.setup}.sig`, "utf8")).trim();
  const n1Comment = trustedComment(n1Signature);
  if (!n1Comment.includes(`version:${VERSION_N1}`)) {
    throw new StepError("SIGNATURE", `N+1 updater signature does not bind the version: '${n1Comment}'`);
  }
  report.build.n1TrustedComment = n1Comment;

  // 攻击者密钥签名同一安装包（版本也绑定为 N+1），只有公钥不同。
  const foreignCopy = path.join(workDir, "foreign-setup.exe");
  await copyFile(built.n1.setup, foreignCopy);
  const attacker = built.keys.attacker.pub
    ? built.keys.attacker
    : { ...built.keys.attacker, pub: (await readFile(`${built.keys.attacker.file}.pub`, "utf8")).trim() };
  const attackerSignature = await signWith(attacker, foreignCopy, VERSION_N1);
  const n1Bytes = await readFile(built.n1.setup);
  const tampered = Buffer.from(n1Bytes);
  tampered[tampered.length - 17] ^= 0xff;
  const feed = await startFeed({ "/n1-setup.exe": n1Bytes, "/tampered-setup.exe": tampered });
  feed.manifest = () => manifestFor(feed, feed.mode, { n1: n1Signature, attacker: attackerSignature });
  const feedUrl = `http://127.0.0.1:${feed.port}/latest.json`;

  try {
    const exe = await installN(built.n.setup);
    log(`installed ${VERSION_N} into ${path.relative(repoRoot, installDir)}`);
    const cdpPort = await freePort();
    const child = spawn(exe, [], { cwd: installDir, env: appEnv(cdpPort, feedUrl), windowsHide: false, stdio: ["ignore", "pipe", "pipe"] });
    startedChildren.push(child);
    let stdoutTail = "";
    const capture = (chunk) => {
      stdoutTail = (stdoutTail + chunk.toString()).slice(-20000);
    };
    child.stdout.on("data", capture);
    child.stderr.on("data", capture);
    const childExit = new Promise((resolve) => child.once("exit", (code) => resolve(code)));

    let browser = await connectCdp(chromium, cdpPort, () => child.exitCode === null, 90_000);
    let page = await findAppPage(browser, 60_000);
    const first = await diagnostics(page);
    if (first.currentVersion !== VERSION_N || !first.enabled) {
      throw new StepError("APP_N", `expected enabled updater on ${VERSION_N}, got ${JSON.stringify(first)}`);
    }
    report.startupState = first;
    const hosts = await waitForHost(90_000).catch(async (error) => {
      await writeFile(path.join(workDir, "app-stdout.log"), stdoutTail);
      throw error;
    });
    log(`N running, Host pid(s) ${hosts.map((entry) => entry.pid).join(",")}`);

    // ---------------- negatives (U5)
    const negatives = [
      { mode: "none", expect: "up-to-date" },
      { mode: "downgrade", expect: "up-to-date" },
      { mode: "same", expect: "up-to-date" },
      { mode: "wrong-platform", expect: "error" },
      { mode: "tampered", expect: "download-error" },
      { mode: "foreign-key", expect: "download-error" },
      { mode: "relabel", expect: "download-error" },
    ];
    for (const negative of negatives) {
      feed.mode = negative.mode;
      const check = await callAndAwaitEvent(
        page,
        "check",
        { manual: true },
        [{ event: "harness://updater/check-result", kinds: ["up-to-date", "available", "error", "ready", "dev-skipped"] }],
        30_000,
      );
      const entry = { mode: negative.mode, check: check.payload?.kind ?? check.event };
      if (check.payload?.kind === "error") entry.checkError = check.payload.message;
      if (check.payload?.kind === "available") {
        const download = await callAndAwaitEvent(
          page,
          "download",
          {},
          [
            { event: "harness://updater/ready", kinds: ["version"] },
            { event: "harness://updater/check-result", kinds: ["error"] },
          ],
          120_000,
        );
        entry.download = download.event === "harness://updater/ready" ? "READY (unexpected)" : download.payload?.message ?? download.event;
      }
      const after = await diagnostics(page);
      entry.phase = after.phase;
      entry.version = after.currentVersion;
      report.negatives.push(entry);
      const ok =
        after.currentVersion === VERSION_N &&
        after.phase !== "update-downloaded" &&
        child.exitCode === null &&
        (negative.expect === "up-to-date"
          ? entry.check === "up-to-date"
          : negative.expect === "error"
            ? entry.check === "error"
            : entry.check === "available" && typeof entry.download === "string" && !entry.download.startsWith("READY") && entry.download !== "timeout");
      log(`negative ${negative.mode}: ${JSON.stringify(entry)}`);
      if (!ok) throw new StepError("NEGATIVE", `feed mode ${negative.mode} was not refused as expected: ${JSON.stringify(entry)}`);
    }

    // ---------------- positive update (U1-U4)
    feed.mode = "good";
    const servedBefore = feed.bytesServed["/n1-setup.exe"] ?? 0;
    const check = await callAndAwaitEvent(page, "check", { manual: true }, [{ event: "harness://updater/check-result", kinds: ["available", "error", "up-to-date"] }], 30_000);
    if (check.payload?.kind !== "available" || check.payload.version !== VERSION_N1) {
      throw new StepError("CHECK", `expected ${VERSION_N1} available, got ${JSON.stringify(check)}`);
    }
    const downloadStarted = Date.now();
    const ready = await callAndAwaitEvent(
      page,
      "download",
      {},
      [
        { event: "harness://updater/ready", kinds: ["version"] },
        { event: "harness://updater/check-result", kinds: ["error"] },
      ],
      300_000,
    );
    if (ready.event !== "harness://updater/ready" || ready.payload !== VERSION_N1) {
      throw new StepError("DOWNLOAD", `download did not become ready: ${JSON.stringify(ready)}`);
    }
    report.download = {
      bytes: (feed.bytesServed["/n1-setup.exe"] ?? 0) - servedBefore,
      artifactBytes: n1Bytes.length,
      seconds: (Date.now() - downloadStarted) / 1000,
    };
    const state = await updater(page, "getState");
    if (state.kind !== "update-downloaded" || state.version !== VERSION_N1 || state.releaseNotes?.title !== `Harness ${VERSION_N1}`) {
      throw new StepError("READY_STATE", `unexpected state ${JSON.stringify(state)}`);
    }
    const hostsBefore = installProcesses().filter((entry) => entry.pid !== child.pid);
    const oldShellPid = child.pid;
    const installStarted = Date.now();
    // Windows：安装器启动后 shell 在该调用内退出，调用不会返回。
    await page.evaluate(() => {
      void window.__TAURI_INTERNALS__.invoke("updater_call", { method: "quitAndInstall", params: {} }).catch(() => {});
    });
    const exitCode = await Promise.race([childExit, sleep(120_000).then(() => "timeout")]);
    if (exitCode === "timeout") throw new StepError("INSTALL_EXIT", "the shell did not exit within 120 s after quitAndInstall");
    const shellExitedAt = Date.now();
    const survivors = hostsBefore.filter((entry) => pidAlive(entry.pid));
    report.install = {
      shellExitCode: exitCode,
      shellExitSeconds: (shellExitedAt - installStarted) / 1000,
      childProcessesBefore: hostsBefore.map((entry) => `${entry.name}:${entry.pid}`),
      aliveAtShellExit: survivors.map((entry) => `${entry.name}:${entry.pid}`),
    };
    await browser.close().catch(() => {});
    if (survivors.length > 0) {
      throw new StepError("DRAIN", `Host/agent processes still alive when the shell exited: ${report.install.aliveAtShellExit.join(", ")}`);
    }

    // 安装器（/UPDATE /R）完成后以 RunAsUser 重新启动新版本；它继承本 shell 的环境（含 CDP 端口）。
    let relaunched = null;
    await waitUntil(
      () => {
        const shells = installProcesses().filter((entry) => entry.name?.toLowerCase() === `${BINARY}.exe` && entry.pid !== oldShellPid);
        relaunched = shells[0] ?? null;
        return relaunched !== null;
      },
      180_000,
      "RELAUNCH",
      "the installer did not relaunch the app within 180 s",
    ).catch((error) => {
      const installers = powershellJson(
        "Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -and $_.CommandLine -match 'HarnessE2EUpdater' } | ForEach-Object { [pscustomobject]@{ pid=$_.ProcessId; name=$_.Name; cmd=$_.CommandLine } } | ConvertTo-Json -Compress",
      );
      error.message += `; DisplayVersion=${regValues(UNINSTALL_KEY)?.DisplayVersion}; processes=${JSON.stringify(installers)}; temp=${updaterTempDirs().join(",")}`;
      throw error;
    });
    log(`relaunched pid ${relaunched.pid}`);
    browser = await connectCdp(chromium, cdpPort, () => pidAlive(relaunched.pid), 90_000);
    page = await findAppPage(browser, 60_000);
    const afterUpdate = await diagnostics(page);
    report.install.updateSeconds = (Date.now() - installStarted) / 1000;
    report.afterUpdate = afterUpdate;
    if (afterUpdate.currentVersion !== VERSION_N1) {
      throw new StepError("VERSION", `expected ${VERSION_N1} after the update, got ${afterUpdate.currentVersion}`);
    }
    const uninstallValues = regValues(UNINSTALL_KEY);
    report.writes.afterUpdate = {
      uninstallKey: uninstallValues,
      productKey: regValues(PRODUCT_KEY),
      shortcuts: shortcutPaths().filter((file) => existsSync(file)),
    };
    if (uninstallValues?.DisplayVersion !== VERSION_N1) {
      throw new StepError("REGISTRY", `DisplayVersion after update is ${uninstallValues?.DisplayVersion}`);
    }
    // 升级后的发布说明（U4）：取到 N+1 的说明，确认后清除。
    const notes = await updater(page, "takeReleaseNotes");
    if (notes?.version !== VERSION_N1) throw new StepError("RELEASE_NOTES", `post-update notes missing: ${JSON.stringify(notes)}`);
    await updater(page, "ackReleaseNotes", { version: VERSION_N1 });
    const notesAfterAck = await updater(page, "takeReleaseNotes");
    if (notesAfterAck !== null) throw new StepError("RELEASE_NOTES", "release notes still pending after ack");
    report.releaseNotes = { title: notes.title, acknowledged: true };
    const hostsAfter = await waitForHost(90_000);
    report.install.hostsAfterRelaunch = hostsAfter.map((entry) => `${entry.name}:${entry.pid}`);

    // 同版本 feed 不再提示（防止升级后循环）。
    const recheck = await callAndAwaitEvent(page, "check", { manual: true }, [{ event: "harness://updater/check-result", kinds: ["available", "error", "up-to-date"] }], 30_000);
    report.recheckAfterUpdate = recheck.payload?.kind ?? recheck.event;
    if (recheck.payload?.kind !== "up-to-date") throw new StepError("RECHECK", `N+1 still sees an update: ${JSON.stringify(recheck)}`);

    // ---------------- log order: Host drained before the installer started
    const close = await closeGracefully(
      page,
      () => pidAlive(relaunched.pid),
      () => waitUntil(() => !pidAlive(relaunched.pid), 30_000, "CLOSE", "relaunched app did not exit"),
    );
    await browser.close().catch(() => {});
    report.close = close;
    await waitUntil(() => installProcesses().length === 0, 30_000, "ORPHANS", `processes left under the install dir after close: ${JSON.stringify(installProcesses())}`);
    const logs = await readAppLogs();
    const drainAt = logs.indexOf("update install: draining the Host");
    const drainedAt = logs.indexOf("update install: Host drained, launching the installer");
    report.logOrder = { draining: drainAt, drained: drainedAt };
    if (drainAt < 0 || drainedAt < drainAt) {
      throw new StepError("LOG_ORDER", `install log order missing (draining=${drainAt}, drained=${drainedAt}); stdout tail: ${stdoutTail.slice(-1500)}`);
    }

    // ---------------- U9: no updater installer left in %TEMP% (the shell sweeps on startup and in the close barrier)
    const leftTemp = updaterTempDirs().filter((name) => !tempBefore.includes(name) && !decoys.some((decoy) => decoy.name === name));
    const wrongDecoys = decoyMismatches(decoys);
    report.updaterTemp = {
      left: leftTemp,
      decoys: decoys.map((decoy) => `${decoy.name}: expected ${decoy.expect} (${decoy.why})`),
      sweepLog: logs.split(/\r?\n/).filter((line) => /leftover updater temp directory|kept an updater temp directory/.test(line)).map((line) => line.slice(-260)),
    };
    if (leftTemp.length > 0 || wrongDecoys.length > 0) {
      throw new StepError("UPDATER_TEMP", `left in %TEMP%: ${JSON.stringify(leftTemp)}; decoys wrong: ${JSON.stringify(wrongDecoys)}`);
    }
    return {
      versions: `${VERSION_N} -> ${VERSION_N1}`,
      build: {
        reused: built.reused,
        nSeconds: built.n.seconds,
        n1Seconds: built.n1.seconds,
        n1InstallerBytes: built.n1.bytes,
        authenticode: built.n.signed ? "signed" : "unsigned (HARNESS_ALLOW_UNSIGNED=1)",
      },
      negatives: report.negatives.map((entry) => `${entry.mode}: ${entry.check}${entry.download ? ` / ${entry.download}` : ""}${entry.checkError ? ` (${entry.checkError})` : ""}`),
      download: report.download,
      install: report.install,
      releaseNotes: report.releaseNotes,
      writes: report.writes,
      updaterTemp: report.updaterTemp,
      close,
    };
  } finally {
    feed.server.close();
  }
}

await main();
