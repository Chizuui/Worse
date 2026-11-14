// App lifecycle for the Phase 7 performance gate: the packaged-like ZCode (Electron) and the
// extracted Harness release build, both attached over CDP with playwright-core and measured from
// inside the page. Every process started here is tracked; only those trees are ever stopped.
import { spawn } from "node:child_process";
import { createWriteStream } from "node:fs";
import { mkdir } from "node:fs/promises";
import { createRequire } from "node:module";
import net from "node:net";
import path from "node:path";
import process from "node:process";
import { pathToFileURL } from "node:url";
import { killTrees, processTree, ps, stillAlive } from "./procs.mjs";
import { cacheRoot, repoRoot } from "./fixtures.mjs";
import { restoreRegistry, snapshotRegistry } from "./regGuard.mjs";

const requireFromRepo = createRequire(path.join(repoRoot, "package.json"));
const now = () => performance.timeOrigin + performance.now();

/** Roots of every app tree this gate started and has not verified closed (pid -> ChildProcess). */
export const startedRoots = new Map();

/** Kills the trees of roots that were never verified closed and whose process has not exited. */
export async function killLeftoverRoots() {
  const live = [...startedRoots.entries()].filter(([, child]) => child.exitCode === null && child.signalCode === null).map(([pid]) => pid);
  if (live.length > 0) await killTrees(live).catch(() => {});
  return [...startedRoots.keys()];
}

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

let chromiumDriver;
async function chromium() {
  if (!chromiumDriver) {
    const playwright = await import(pathToFileURL(requireFromRepo.resolve("playwright-core")).href);
    chromiumDriver = playwright.chromium ?? playwright.default.chromium;
  }
  return chromiumDriver;
}

// In-page probe, installed as an init script (document start) or late by evaluate. Stamps:
//   workspaceAt  composer input attached + visible (first interactive workspace)
//   keydowns     capture-phase Enter keydowns (t_send)
//   acks         every v4 command ack pushed to window.__zcodeV4CommandAcksE2E, stamped at push
//   watch        token watcher for the first streamed delta (domAt / paintAt)
// All times are epoch ms (performance.timeOrigin + performance.now()).
export const PROBE_SOURCE = String.raw`(() => {
  if (window.__p7) return;
  const t = () => performance.timeOrigin + performance.now();
  const p7 = (window.__p7 = { installedAt: t(), readyStateAtInstall: document.readyState, workspaceAt: null, workspaceLate: false, keydowns: [], acks: [], watch: null });
  const acks = (window.__zcodeV4CommandAcksE2E = window.__zcodeV4CommandAcksE2E || []);
  for (const entry of acks) p7.acks.push({ ...entry, p7At: null });
  const push = acks.push.bind(acks);
  acks.push = (...items) => {
    const at = t();
    for (const item of items) p7.acks.push({ type: item && item.type, status: item && item.status, reasonCode: item && item.reasonCode, p7At: at });
    return push(...items);
  };
  window.addEventListener("keydown", (event) => { if (event.key === "Enter" && !event.shiftKey && !event.isComposing) p7.keydowns.push(t()); }, true);
  const SELECTOR = '[data-testid="v4-composer-input"]';
  const visible = (el) => !!el && el.isConnected && el.getClientRects().length > 0;
  let pendingVisibility = false;
  const checkWorkspace = () => {
    if (p7.workspaceAt !== null) return;
    const el = document.querySelector(SELECTOR);
    if (!el) return;
    if (visible(el)) { p7.workspaceAt = t(); return; }
    if (pendingVisibility) return;
    pendingVisibility = true;
    const loop = () => { if (p7.workspaceAt !== null) return; const cur = document.querySelector(SELECTOR); if (visible(cur)) p7.workspaceAt = t(); else requestAnimationFrame(loop); };
    requestAnimationFrame(loop);
  };
  const scanWatch = (records) => {
    const watch = p7.watch;
    if (!watch || watch.domAt !== null) return;
    for (const record of records) {
      const nodes = record.type === "characterData" ? [record.target] : Array.from(record.addedNodes);
      for (const node of nodes) {
        const text = node.textContent || "";
        if (text.includes(watch.token)) {
          watch.domAt = t();
          requestAnimationFrame(() => {
            watch.rafAt = t();
            const channel = new MessageChannel();
            channel.port1.onmessage = () => { watch.paintAt = t(); };
            channel.port2.postMessage(0);
          });
          return;
        }
      }
    }
  };
  const observer = new MutationObserver((records) => { checkWorkspace(); scanWatch(records); });
  const start = () => { observer.observe(document, { childList: true, subtree: true, characterData: true }); checkWorkspace(); };
  start();
  p7.expect = (token) => { p7.watch = { token, domAt: null, rafAt: null, paintAt: null }; };
})();`;

function proxyArgs() {
  // 只允许回环流量：真实网络请求全部指向本机不可达的 127.0.0.1:9。
  return ["--proxy-server=http://127.0.0.1:9", "--proxy-bypass-list=<-loopback>;localhost;127.0.0.1;*.localhost"];
}

function commonEnv({ home, cloudPort }) {
  const env = {
    ...process.env,
    ZCODE_ENDPOINT_ORIGIN: `http://127.0.0.1:${cloudPort}`,
    ZCODE_BASE_URL: `http://127.0.0.1:${cloudPort}`,
    ZCODE_WEB_REMOTE_CONTROL_RELAY_WS_URL: `ws://127.0.0.1:${cloudPort}/relay`,
    HTTP_PROXY: "http://127.0.0.1:9",
    HTTPS_PROXY: "http://127.0.0.1:9",
    NO_PROXY: "127.0.0.1,localhost",
    NODE_USE_ENV_PROXY: "1",
    HARNESS_DEVICE_MID: "phase7-perf-device",
    HARNESS_LOCALE: "en-US",
  };
  for (const key of Object.keys(env)) {
    if (/^OTEL_|^ELECTRON_RUN_AS_NODE$|^ELECTRON_RENDERER_URL$|^HARNESS_(NODE_BIN|HOST_ENTRY|RUST_HOST|REPO_ROOT|RUST_SERVICES)$/.test(key)) delete env[key];
  }
  env.ZCODE_PHASE7_PERF_HOME = home;
  return env;
}

async function connectCdp(port, child, timeoutMs) {
  const driver = await chromium();
  const deadline = Date.now() + timeoutMs;
  let lastError;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) throw new Error(`app exited early (${String(child.exitCode)})`);
    try {
      return await driver.connectOverCDP(`http://127.0.0.1:${port}`, { timeout: 5_000 });
    } catch (error) {
      lastError = error;
      // DevTools 端点在浏览器进程起来后才监听；失败即短间隔重试（每次 connect 自带超时）。
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
  }
  throw new Error(`could not attach over CDP on port ${port}: ${String(lastError?.message ?? lastError)}`);
}

const isMainPage = (kind, url) => (kind === "harness" ? /^https?:\/\/tauri\.localhost\//.test(url) : /\/renderer\/index\.html/.test(url) && !/windowKind=/.test(url));

async function findMainPage(kind, browser, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    for (const context of browser.contexts()) {
      for (const page of context.pages()) if (isMainPage(kind, page.url())) return page;
    }
    const context = browser.contexts()[0];
    if (context) await context.waitForEvent("page", { timeout: 500 }).catch(() => {});
    else await new Promise((resolve) => setTimeout(resolve, 100));
    // 页面可能已存在但尚未导航到主入口；短间隔重查 URL。
  }
  const urls = browser.contexts().flatMap((context) => context.pages().map((page) => page.url()));
  throw new Error(`no ${kind} main page; pages: ${urls.join(", ")}`);
}

/**
 * Launches one app. Returns a handle with the page, timings and close(). The probe is registered on
 * the default context as soon as CDP attaches, so a main document created afterwards gets it at
 * document start; otherwise it is evaluated late and `probeLate` is set.
 */
export async function launchApp(kind, { exe, home, homeEnv = {}, cloudPort, logDir, label }) {
  await mkdir(logDir, { recursive: true });
  const cdpPort = await freePort();
  const env = { ...commonEnv({ home, cloudPort }), ...homeEnv };
  let args = [];
  if (kind === "harness") {
    env.WEBVIEW2_USER_DATA_FOLDER = path.join(home, "harness-webview2");
    env.WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = [`--remote-debugging-port=${cdpPort}`, ...proxyArgs()].join(" ");
  } else {
    const userData = path.join(home, "zcode-electron-userdata");
    env.ZCODE_DESKTOP_USER_DATA_DIR = userData;
    env.ZCODE_DESKTOP_SESSION_DATA_DIR = path.join(home, "zcode-electron-sessiondata");
    env.ZCODE_DESKTOP_HOME_DIR = home;
    env.ZCODE_DESKTOP_APPLICATION_NAME = "ZCode Phase7 Perf";
    env.ZCODE_DISABLE_FIXED_REMOTE_DEBUGGING_PORT = "1";
    args = [`--user-data-dir=${userData}`, `--remote-debugging-port=${cdpPort}`, ...proxyArgs()];
    // ZCode 启动时无条件改写 HKCU 的 zcode:// 协议与资源管理器右键菜单（注册表不在隔离 home 内），
    // 先导出原值，关闭后恢复，避免把用户自己的注册项指向 gate 缓存布局。
    await snapshotRegistry({ dir: path.join(cacheRoot, "reg-guard"), marker: path.dirname(exe) });
  }
  const log = createWriteStream(path.join(logDir, `${label}.log`));
  const spawnAt = now();
  const child = spawn(exe, args, { cwd: home, env, stdio: ["ignore", "pipe", "pipe"], windowsHide: false });
  if (child.pid) startedRoots.set(child.pid, child);
  // 记录根进程的 pid+创建时间，关闭后据此确认它真的退出（避免 PID 复用误判，也不靠 exit 事件单独判断）。
  const rootIdentity = child.pid ? processTree([child.pid]).then((tree) => tree.filter((entry) => entry.pid === child.pid)).catch(() => []) : Promise.resolve([]);
  child.stdout.on("data", (chunk) => log.write(chunk));
  child.stderr.on("data", (chunk) => log.write(chunk));
  const exited = new Promise((resolve) => child.once("exit", (code) => resolve(code)));
  exited.then(() => log.end());
  const handle = { kind, label, child, spawnAt, exited, rootIdentity, log, cdpPort, browser: null, page: null, probeLate: false };
  try {
    const browser = await connectCdp(cdpPort, child, 120_000);
    handle.browser = browser;
    handle.cdpAttachedAt = now();
    const context = browser.contexts()[0];
    if (context) await context.addInitScript({ content: PROBE_SOURCE }).catch(() => {});
    const page = await findMainPage(kind, browser, 120_000);
    handle.page = page;
    page.on("console", (message) => {
      if (message.type() === "error" || message.type() === "warning") log.write(`[renderer:${message.type()}] ${message.text().slice(0, 500)}\n`);
    });
    const hasProbe = await page.evaluate(() => Boolean(window.__p7)).catch(() => false);
    if (!hasProbe) {
      handle.probeLate = true;
      await page.evaluate(PROBE_SOURCE);
    }
    return handle;
  } catch (error) {
    await closeApp(handle).catch(() => {});
    throw error;
  }
}

/** Waits for the interactive workspace; dismisses ZCode's first-run onboarding (Escape) if shown. */
export async function waitWorkspace(handle, timeoutMs = 120_000) {
  const { page } = handle;
  const deadline = Date.now() + timeoutMs;
  let onboarding = false;
  while (Date.now() < deadline) {
    const state = await page
      .evaluate(() => ({ at: window.__p7?.workspaceAt ?? null, onboarding: Boolean(document.querySelector('[data-testid="onboarding-page"]')) }))
      .catch(() => ({ at: null, onboarding: false }));
    if (state.at !== null) break;
    if (state.onboarding) {
      onboarding = true;
      await page.keyboard.press("Escape").catch(() => {});
    }
    await page.waitForFunction(() => window.__p7?.workspaceAt !== null || Boolean(document.querySelector('[data-testid="onboarding-page"]')), null, { timeout: 2_000, polling: "raf" }).catch(() => {});
  }
  const timing = await page.evaluate(() => {
    const paints = Object.fromEntries(performance.getEntriesByType("paint").map((entry) => [entry.name, performance.timeOrigin + entry.startTime]));
    const nav = performance.getEntriesByType("navigation")[0];
    return {
      timeOrigin: performance.timeOrigin,
      firstPaint: paints["first-paint"] ?? null,
      firstContentfulPaint: paints["first-contentful-paint"] ?? null,
      domContentLoaded: nav ? performance.timeOrigin + nav.domContentLoadedEventEnd : null,
      workspaceAt: window.__p7?.workspaceAt ?? null,
      probeInstalledAt: window.__p7?.installedAt ?? null,
      readyStateAtInstall: window.__p7?.readyStateAtInstall ?? null,
      composerAtInstall: false,
    };
  });
  if (timing.workspaceAt === null) throw new Error(`${handle.label}: interactive workspace (v4-composer-input) never appeared`);
  // 探针晚于主文档创建时：若安装时 composer 已可见，则该样本只是上界。
  const late = handle.probeLate && timing.workspaceAt - timing.probeInstalledAt < 5;
  const firstWindow = timing.firstPaint ?? timing.firstContentfulPaint;
  return {
    onboarding,
    probeLate: handle.probeLate,
    workspaceLate: late,
    firstWindowMs: firstWindow === null ? null : Math.round(firstWindow - handle.spawnAt),
    firstContentfulPaintMs: timing.firstContentfulPaint === null ? null : Math.round(timing.firstContentfulPaint - handle.spawnAt),
    navigationStartMs: Math.round(timing.timeOrigin - handle.spawnAt),
    domContentLoadedMs: timing.domContentLoaded === null ? null : Math.round(timing.domContentLoaded - handle.spawnAt),
    workspaceMs: Math.round(timing.workspaceAt - handle.spawnAt),
    cdpAttachMs: Math.round(handle.cdpAttachedAt - handle.spawnAt),
  };
}

/** Re-attaches over CDP after detach() (same page; the in-page probe state survives). */
export async function reattach(handle) {
  const browser = await connectCdp(handle.cdpPort, handle.child, 60_000);
  handle.browser = browser;
  handle.page = await findMainPage(handle.kind, browser, 30_000);
  const hasProbe = await handle.page.evaluate(() => Boolean(window.__p7)).catch(() => false);
  if (!hasProbe) await handle.page.evaluate(PROBE_SOURCE);
  return handle;
}

/** Disconnects playwright (the app keeps running). */
export async function detach(handle) {
  if (handle.browser) await handle.browser.close().catch(() => {});
  handle.browser = null;
  handle.page = null;
}

/**
 * Graceful close: WM_CLOSE to the main window (ZCode: closeToTrayOnWindows=false -> quit; Harness:
 * CloseRequested -> Host drain), then wait for the whole tree to exit. Forced only on our own tree
 * after the timeout.
 */
export async function closeApp(handle, timeoutMs = 45_000) {
  const { child } = handle;
  const root = await handle.rootIdentity;
  // 修复原因：一次完整运行中 ZCode 首次运行把 WM_CLOSE 处理成“隐藏到托盘”，根进程未退出，
  // 而 closeApp 已把它移出 startedRoots，整棵树残留到 gate 结束之后。现在：关窗后根进程若在宽限期内
  // 仍存活（托盘隐藏）就强制结束本树；最后按 pid+创建时间核实根进程与快照内进程全部退出，否则抛错。
  const graceMs = handle.kind === "zcode" ? 15_000 : timeoutMs;
  const tree = child.pid ? await processTree([child.pid]).catch(() => []) : [];
  const snapshot = [...root, ...tree.filter((entry) => !root.some((r) => r.pid === entry.pid))];
  let via = "already exited";
  if (child.exitCode === null && child.pid) {
    const out = await ps(`$p = Get-Process -Id ${child.pid} -ErrorAction SilentlyContinue; if ($p) { $p.CloseMainWindow() } else { 'gone' }`).catch((error) => String(error));
    via = `CloseMainWindow=${out}`;
    if (out.trim() !== "True" && handle.page) {
      // 没有主窗口句柄时回退到页面内关闭。
      if (handle.kind === "harness") await handle.page.evaluate(() => window.__TAURI_INTERNALS__.invoke("plugin:window|close", { label: "main" })).catch(() => {});
      else await handle.page.evaluate(() => window.close()).catch(() => {});
      via += "+page close";
    }
  }
  await detach(handle);
  let timer;
  const code = await Promise.race([handle.exited, new Promise((resolve) => (timer = setTimeout(() => resolve("timeout"), graceMs)))]);
  clearTimeout(timer);
  let forced = false;
  if (code === "timeout") {
    forced = true;
    via += ` (root alive after ${graceMs} ms)`;
    await killTrees([child.pid]);
  }
  // 根进程退出后等待整棵树（Host / Agent / WebView2 / GPU 等）退出，基于 pid+创建时间判定。
  const deadline = Date.now() + 30_000;
  let alive = await stillAlive(snapshot).catch(() => []);
  while (alive.length > 0 && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 500));
    alive = await stillAlive(snapshot).catch(() => []);
  }
  let orphans = [];
  if (alive.length > 0) {
    orphans = alive.map((entry) => `${entry.pid}`);
    await killTrees(alive.map((entry) => entry.pid));
    forced = true;
  }
  // 最终核实：根进程身份（启动时记录）与关闭前快照都必须已退出。
  let survivors = await stillAlive(snapshot);
  if (survivors.length > 0) {
    for (const entry of survivors) await killTrees([entry.pid]);
    await new Promise((resolve) => setTimeout(resolve, 1_000));
    survivors = await stillAlive(snapshot);
  }
  if (survivors.length > 0) throw new Error(`${handle.label}: processes still alive after close: ${survivors.map((entry) => `${entry.name}:${entry.pid}`).join(",")}`);
  if (root.length === 0 && child.exitCode === null && child.signalCode === null) {
    // 启动时没拿到根身份（WMI 瞬时故障）：本进程仍持有子进程句柄，PID 不会被复用，可直接经句柄结束根进程。
    child.kill("SIGKILL");
    let killTimer;
    await Promise.race([handle.exited, new Promise((resolve) => (killTimer = setTimeout(resolve, 10_000)))]);
    clearTimeout(killTimer);
    forced = true;
  }
  if (root.length === 0 && child.exitCode === null && child.signalCode === null) throw new Error(`${handle.label}: root identity unknown and the root has not exited`);
  startedRoots.delete(child.pid);
  if (handle.kind === "zcode") await restoreRegistry().catch(() => {});
  return { via, exitCode: code === "timeout" ? null : code, forced, orphansKilled: orphans, treeSize: snapshot.length };
}
