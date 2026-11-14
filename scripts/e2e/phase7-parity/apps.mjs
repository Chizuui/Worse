// App lifecycle for the Phase 7 parity gate: Harness (debug harness-app + its Vite dev server) and
// ZCode (Electron from packages/desktop + the ZCode renderer Vite dev server), both driven over CDP
// with playwright-core. Every process started here is recorded and only those trees are stopped.
// All children inherit the isolated home installed by the gate (installIsolatedHome).
import { spawn, spawnSync } from "node:child_process";
import { createWriteStream, existsSync } from "node:fs";
import { mkdir } from "node:fs/promises";
import { createRequire } from "node:module";
import http from "node:http";
import net from "node:net";
import path from "node:path";
import process from "node:process";
import { pathToFileURL } from "node:url";
import { isolatedHomeDir } from "../lib/isolatedHome.mjs";
import { repoRoot } from "../phase2/environment.mjs";
import { GateError, fail } from "../phase2/gate.mjs";

export const worseRoot = path.join(repoRoot, "Worse");
const harnessAppDir = path.join(worseRoot, "apps/harness-app");
export const harnessExe = path.join(worseRoot, "target/debug/harness-app.exe");
const desktopDir = path.join(repoRoot, "packages/desktop");
export const HARNESS_DEV_URL = "http://localhost:1430";
export const ZCODE_DEV_URL = "http://localhost:5174";

const started = [];
/** Every child ever started (pid → label), kept after stop so the gate can verify each one is gone. */
const everStarted = new Map();
const requireFromRepo = createRequire(path.join(repoRoot, "package.json"));
const requireFromDesktop = createRequire(path.join(desktopDir, "package.json"));

export function freePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      server.close(() => resolve(port));
    });
  });
}

export function portInUse(port) {
  return new Promise((resolve) => {
    const socket = net.connect({ port, host: "127.0.0.1" });
    socket.once("connect", () => {
      socket.destroy();
      resolve(true);
    });
    socket.once("error", () => resolve(false));
  });
}

/** Resolves when `child` prints a line matching `pattern` (event-driven). */
export function waitForOutput(child, pattern, timeoutMs, step) {
  return new Promise((resolve, reject) => {
    let buffer = "";
    const cleanup = () => {
      clearTimeout(timer);
      child.stdout?.off("data", onData);
      child.stderr?.off("data", onData);
      child.off("exit", onExit);
    };
    const onData = (chunk) => {
      buffer = (buffer + chunk.toString()).slice(-20_000);
      if (pattern.test(buffer)) {
        cleanup();
        resolve();
      }
    };
    const onExit = (code) => {
      cleanup();
      reject(new GateError(step, `process exited (${String(code)}) before ${String(pattern)}; tail: ${buffer.slice(-800)}`));
    };
    const timer = setTimeout(() => {
      cleanup();
      reject(new GateError(step, `timed out waiting for ${String(pattern)}; tail: ${buffer.slice(-800)}`));
    }, timeoutMs);
    child.stdout?.on("data", onData);
    child.stderr?.on("data", onData);
    child.once("exit", onExit);
  });
}

function track(child, label = path.basename(child.spawnfile ?? "child")) {
  started.push(child);
  if (child.pid !== undefined) everStarted.set(child.pid, label);
  return child;
}

function isAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error.code === "EPERM";
  }
}

/** Ends one process tree this gate started; returns what taskkill reported (never silent). */
function killTree(child) {
  if (child.pid === undefined || !isAlive(child.pid)) return null;
  // 只结束本 gate 启动的进程树。
  const result = spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], { encoding: "utf8", windowsHide: true });
  const report = { pid: child.pid, status: result.status, message: (result.stderr || result.stdout || "").trim(), alive: isAlive(child.pid) };
  if (report.alive) {
    // 修复：taskkill /T 依赖 WMI 枚举进程树；WMI 异常时它返回 "ERROR: Call cancelled"（status 1）且什么都不结束，
    // 第 5 次运行因此留下两个 Vite。此时直接 TerminateProcess 本 gate 启动的根进程（Vite 的子进程只有 conhost 和
    // esbuild 服务，后者在 stdin 管道关闭时自行退出）。
    try {
      process.kill(child.pid);
      report.fallback = "process.kill";
    } catch (error) {
      report.fallback = `process.kill failed: ${error.code}`;
    }
    report.alive = isAlive(child.pid);
  }
  return report;
}

export function stopStarted() {
  // 修复：此前按 child.exitCode 判断是否需要结束，并忽略 taskkill 结果；第 3 次运行打印 OK 后 Vite 仍存活、
  // gate 进程无法退出，而命令行匹配隔离 home 的残留检查看不到 Vite。改为按 PID 实际存活判断并返回结果。
  return started.splice(0).reverse().map(killTree).filter(Boolean);
}

/** Tracked PIDs that are still running (verification after stop; complements the command-line scan). */
export function trackedAlive() {
  return [...everStarted].filter(([pid]) => isAlive(pid)).map(([pid, label]) => `${pid} ${label}`);
}

/** Local stand-in for the ZCode cloud: every request gets 404 so neither app reaches a real service. */
export function startDeadCloud() {
  const paths = [];
  const server = http.createServer((request, response) => {
    paths.push(`${request.method} ${request.url?.split("?")[0]}`);
    response.writeHead(404, { "content-type": "application/json" });
    response.end('{"code":404,"msg":"phase7 local fixture"}');
  });
  server.on("upgrade", (_request, socket) => socket.destroy());
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve({ server, paths, port: server.address().port })));
}

let playwright;
async function chromium() {
  if (!playwright) playwright = await import(pathToFileURL(requireFromRepo.resolve("playwright-core")).href);
  return playwright.chromium ?? playwright.default.chromium;
}

async function connectCdp(port, child, timeoutMs, step) {
  const driver = await chromium();
  const deadline = Date.now() + timeoutMs;
  let lastError;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) fail(step, `app exited early (${String(child.exitCode)})`);
    try {
      return await driver.connectOverCDP(`http://127.0.0.1:${port}`, { timeout: 5_000 });
    } catch (error) {
      lastError = error;
      // DevTools 端点在浏览器进程起来后才监听；连接失败即重试（每次 connect 自带超时）。
      await new Promise((resolve) => setTimeout(resolve, 500));
    }
  }
  fail(step, `could not attach over CDP on port ${port}: ${String(lastError?.message ?? lastError)}`);
}

async function findPage(browser, urlPrefix, timeoutMs, step) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    for (const context of browser.contexts()) {
      for (const page of context.pages()) {
        const url = page.url();
        // ZCode 也会创建其它窗口（更新状态、资源管理器等）；主窗口是 index 入口且没有 windowKind 查询。
        if (url.startsWith(urlPrefix) && !/windowKind=/.test(url)) return page;
      }
    }
    const context = browser.contexts()[0];
    if (context) await context.waitForEvent("page", { timeout: 1_000 }).catch(() => {});
    else await new Promise((resolve) => setTimeout(resolve, 250));
  }
  fail(step, `no app page at ${urlPrefix}`);
}

/** Dev servers are started once per gate and shared by every launch of the same app. */
const devServers = new Map();

async function ensureDevServer(kind) {
  if (devServers.has(kind)) return devServers.get(kind);
  const port = kind === "harness" ? 1430 : 5174;
  if (await portInUse(port)) {
    fail(`${kind.toUpperCase()}_DEV_PORT`, `port ${port} is already in use (another dev server or gate); stop it and rerun`);
  }
  let child;
  if (kind === "harness") {
    child = spawn(process.execPath, ["scripts/run-vite.cjs", "--configLoader", "native", "--port", "1430", "--strictPort"], {
      cwd: harnessAppDir,
      env: process.env,
      stdio: ["ignore", "pipe", "pipe"],
      windowsHide: true,
    });
  } else {
    const viteBin = path.join(path.dirname(requireFromDesktop.resolve("vite/package.json")), "bin", "vite.js");
    child = spawn(process.execPath, [viteBin, "dev", "--port", "5174", "--strictPort"], {
      cwd: desktopDir,
      // ZCode 开发服务器默认注入 React Scan 浮层（右下角工具条会盖住发送/停止按钮）；它只是开发诊断工具，
      // 不属于产品行为，按 vite.config.ts 的开关关闭。
      env: { ...process.env, ZCODE_REACT_SCAN: "0" },
      stdio: ["ignore", "pipe", "pipe"],
      windowsHide: true,
    });
  }
  track(child);
  await waitForOutput(child, kind === "harness" ? /localhost:1430/ : /localhost:5174/, 180_000, `${kind.toUpperCase()}_DEV_SERVER`);
  devServers.set(kind, child);
  return child;
}

/**
 * Launches one app instance on the isolated home and returns a handle
 * `{ kind, page, browser, child, log, close() }`. `close()` is graceful (window close → Host drain).
 */
/**
 * Environment that points one launch at `home`, a sub-directory of the gate's isolated home (same keys as
 * lib/isolatedHome.mjs buildEnv), so each app run of the gate starts from its own fresh data.
 */
export function homeEnv(home) {
  const isolated = isolatedHomeDir();
  const inside = path.relative(isolated, home);
  // 只允许隔离 home 本身或其子目录，防止把真实 home 交给应用。
  if (inside.startsWith("..") || path.isAbsolute(inside)) fail("ISOLATION", `${home} is outside the isolated home ${isolated}`);
  const env = {
    HOME: home,
    USERPROFILE: home,
    APPDATA: path.join(home, "AppData", "Roaming"),
    LOCALAPPDATA: path.join(home, "AppData", "Local"),
    XDG_CONFIG_HOME: path.join(home, ".config"),
    XDG_DATA_HOME: path.join(home, ".local", "share"),
    XDG_CACHE_HOME: path.join(home, ".cache"),
    XDG_STATE_HOME: path.join(home, ".local", "state"),
    ZCODE_DESKTOP_HOME_DIR: home,
    ZCODE_DATA_BASE_DIR: home,
    CODEX_HOME: path.join(home, ".codex"),
    CLAUDE_CONFIG_DIR: path.join(home, ".claude"),
  };
  if (process.platform === "win32") {
    const parsed = path.parse(home);
    env.HOMEDRIVE = parsed.root.replace(/[\\/]+$/, "");
    env.HOMEPATH = home.slice(env.HOMEDRIVE.length);
  }
  return env;
}

export async function launchApp(kind, { cloud, evidenceDir, label, home = isolatedHomeDir(), extraEnv = {} }) {
  if (kind === "harness" && !existsSync(harnessExe)) fail("BUILD", "Worse/target/debug/harness-app.exe is missing", "cargo build -p harness-app (from Worse/)");
  if (kind === "zcode" && !existsSync(path.join(desktopDir, "out/main/index.js"))) {
    fail("ZCODE_BUILD", "packages/desktop/out/main/index.js is missing", "pnpm --filter @zcode/desktop exec tsup --config tsup.config.ts");
  }
  await ensureDevServer(kind);
  for (const dir of ["AppData/Roaming", "AppData/Local"]) await mkdir(path.join(home, dir), { recursive: true });
  const cdpPort = await freePort();
  const common = {
    ...process.env,
    ...homeEnv(home),
    ...extraEnv,
    ZCODE_ENDPOINT_ORIGIN: `http://127.0.0.1:${cloud.port}`,
    ZCODE_WEB_REMOTE_CONTROL_RELAY_WS_URL: `ws://127.0.0.1:${cloud.port}/relay`,
  };
  let child;
  if (kind === "harness") {
    child = spawn(harnessExe, [], {
      cwd: repoRoot,
      env: {
        // 固定 device id 避免每次启动生成新身份；extraEnv（在 common 里）可以覆盖它。
        HARNESS_DEVICE_MID: "phase7-parity-device",
        ...common,
        WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${cdpPort}`,
        // WebView2 profile 放进隔离 home，避免写入真实 %LOCALAPPDATA%。
        WEBVIEW2_USER_DATA_FOLDER: path.join(home, "harness-webview2"),
      },
      stdio: ["ignore", "pipe", "pipe"],
      windowsHide: false,
    });
  } else {
    const electronExe = requireFromDesktop("electron");
    child = spawn(electronExe, [".", `--remote-debugging-port=${cdpPort}`], {
      cwd: desktopDir,
      env: {
        ...common,
        ELECTRON_RENDERER_URL: ZCODE_DEV_URL,
        // 与 ZCode wdio e2e 相同：不占用开发态固定的 9229 调试端口（用户可能正开着 ZCode Dev）。
        ZCODE_DISABLE_FIXED_REMOTE_DEBUGGING_PORT: "1",
        ELECTRON_ENABLE_LOGGING: "1",
      },
      stdio: ["ignore", "pipe", "pipe"],
      windowsHide: false,
    });
  }
  track(child);
  const log = createWriteStream(path.join(evidenceDir, `parity-${label}.log`));
  child.stdout.on("data", (chunk) => log.write(chunk));
  child.stderr.on("data", (chunk) => log.write(chunk));
  const exited = new Promise((resolve) => child.once("exit", (code) => resolve(code)));
  exited.then(() => log.end());

  const browser = await connectCdp(cdpPort, child, 120_000, `${kind.toUpperCase()}_CDP`);
  const page = await findPage(browser, kind === "harness" ? HARNESS_DEV_URL : ZCODE_DEV_URL, 90_000, `${kind.toUpperCase()}_PAGE`);
  page.on("console", (message) => log.write(`[renderer:${message.type()}] ${message.text()}\n`));
  page.on("pageerror", (error) => log.write(`[renderer:pageerror] ${error.message}\n`));
  await page.waitForLoadState("domcontentloaded");

  async function close() {
    if (child.exitCode !== null) return { closed: true, exitCode: child.exitCode, via: "already exited" };
    if (kind === "harness") {
      // Tauri 窗口关闭 → CloseRequested → Rust shell 先关 Host 再退出（不调用 destroy()）。
      await page.evaluate(() => window.__TAURI_INTERNALS__.invoke("plugin:window|close", { label: "main" })).catch(() => {});
    } else {
      // ZCode：closeToTrayOnWindows=false（fixture 写入）时最后一个窗口关闭 → app.quit() → prepareAppQuit 关 Host。
      await page.evaluate(() => window.close()).catch(() => {});
    }
    const code = await Promise.race([exited, new Promise((resolve) => setTimeout(() => resolve("timeout"), 45_000))]);
    await browser.close().catch(() => {});
    if (code === "timeout") {
      spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], { stdio: "ignore", windowsHide: true });
      return { closed: false, exitCode: null, via: "forced after 45 s" };
    }
    // 退出后进程树里不应再有子进程（Host/Agent/harness-host）；只检查本进程启动的树。
    return { closed: true, exitCode: code, via: kind === "harness" ? "plugin:window|close" : "window.close" };
  }

  return { kind, label, page, browser, child, log, close };
}

/** Dev servers only (apps are closed by their handles); used at the very end. */
export function stopDevServers() {
  const report = [...devServers.values()].map(killTree).filter(Boolean);
  devServers.clear();
  return report;
}
