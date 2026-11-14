#!/usr/bin/env node
// Phase 4 scripted UI e2e (spec docs/specs/harness/phase-4-web-remote-control.md, section 12.6, W16).
//
// Drives the real Harness debug app (Worse/target/debug/harness-app.exe, which loads the Vite dev
// server at http://localhost:1430) through WebView2's DevTools protocol with playwright-core:
//   1. the add-remote-connection dialog (WSL kind; connects to Ubuntu when `wsl.exe -l -q` lists it);
//   2. the mobile remote-control dialog, which pairs against ZCode's own local relay fixture
//      (`createFakeRelayV2`) and must render a QR code.
// Screenshots go to Worse/.work/phase4/ui-*.png. Prints PHASE4_UI_OK or PHASE4_UI_PARTIAL <reason>
// (or PHASE4_UI_FAIL <step> when the app itself cannot be driven).
//
// Safety: installIsolatedHome() runs first, so the app, its Host and Agent only see a throwaway home;
// login state is ZCode's own e2e fixture (`mock-token` / `zcode-e2e-user`), never real credentials;
// the endpoint origin and relay URL point at the local fixture, so the real ZCode cloud is never
// contacted. The window is closed through Tauri's window close (CloseRequested -> graceful Host
// shutdown), never destroy(); only processes this script started are stopped.
import { spawn, spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { createWriteStream, existsSync } from "node:fs";
import { mkdir, readdir, rm, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import net from "node:net";
import path from "node:path";
import process from "node:process";
import { fileURLToPath, pathToFileURL } from "node:url";
import { installIsolatedHome } from "./lib/isolatedHome.mjs";

// 修复说明：GUI 进程同样按 HOME/USERPROFILE/APPDATA 解析 ZCode 数据目录，必须在启动任何子进程前隔离。
const { home } = installIsolatedHome("phase4-ui");

const moduleDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(moduleDir, "../../..");
const worseRoot = path.join(repoRoot, "Worse");
const appDir = path.join(worseRoot, "apps/harness-app");
const appExe = path.join(worseRoot, "target/debug/harness-app.exe");
const shotDir = path.join(worseRoot, ".work/phase4");
const runDir = path.join(worseRoot, ".work/tmp/phase4-ui");
const kitFile = path.join(runDir, "phoneKit.mjs");
const DEV_URL = "http://localhost:1430";

// 与 ZCode desktop e2e fixture 相同的假账号；只写入隔离 home，绝不读取用户真实凭据。
const FIXTURE_JWT = "mock-token";
const FIXTURE_USER = "zcode-e2e-user";
const FIXTURE_CREDENTIALS = {
  "oauth:active_provider": "zai",
  "oauth:zai:access_token": FIXTURE_JWT,
  "oauth:zai:user_info": JSON.stringify({ id: FIXTURE_USER, username: "Harness Phase4", displayName: "Harness Phase4" }),
  zcodejwttoken: FIXTURE_JWT,
};

const requireFromRepo = createRequire(path.join(repoRoot, "package.json"));
const started = [];
const notes = [];
const shots = [];

class StepError extends Error {
  constructor(step, message) {
    super(message);
    this.step = step;
  }
}

function freePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      server.close(() => resolve(port));
    });
  });
}

function portInUse(port) {
  return new Promise((resolve) => {
    const socket = net.connect({ port, host: "127.0.0.1" });
    socket.once("connect", () => {
      socket.destroy();
      resolve(true);
    });
    socket.once("error", () => resolve(false));
  });
}

/** Resolves when `child` prints a line matching `pattern` (event-driven, no polling). */
function waitForOutput(child, pattern, timeoutMs, step) {
  return new Promise((resolve, reject) => {
    let buffer = "";
    const onData = (chunk) => {
      buffer += chunk.toString();
      const match = buffer.match(pattern);
      if (match) {
        cleanup();
        resolve(match);
      }
    };
    const onExit = (code) => {
      cleanup();
      reject(new StepError(step, `process exited (${String(code)}) before ${String(pattern)}; tail: ${buffer.slice(-600)}`));
    };
    const timer = setTimeout(() => {
      cleanup();
      reject(new StepError(step, `timed out waiting for ${String(pattern)}; tail: ${buffer.slice(-600)}`));
    }, timeoutMs);
    const cleanup = () => {
      clearTimeout(timer);
      child.stdout?.off("data", onData);
      child.stderr?.off("data", onData);
      child.off("exit", onExit);
    };
    child.stdout?.on("data", onData);
    child.stderr?.on("data", onData);
    child.once("exit", onExit);
  });
}

/** WebView2 opens its DevTools endpoint only after the browser process starts; retry the connect. */
async function connectCdp(chromium, port, exe, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  let lastError;
  while (Date.now() < deadline) {
    if (exe.exitCode !== null) throw new StepError("APP_START", `harness-app exited early (${String(exe.exitCode)})`);
    try {
      return await chromium.connectOverCDP(`http://127.0.0.1:${port}`, { timeout: 5_000 });
    } catch (error) {
      lastError = error;
      await new Promise((resolve) => setTimeout(resolve, 500));
    }
  }
  throw new StepError("CDP_CONNECT", `could not attach to WebView2 on port ${port}: ${String(lastError?.message ?? lastError)}`);
}

async function findAppPage(browser, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    for (const context of browser.contexts()) {
      for (const page of context.pages()) {
        if (page.url().startsWith(DEV_URL)) return page;
      }
    }
    const context = browser.contexts()[0];
    if (context) {
      await context.waitForEvent("page", { timeout: 1_000 }).catch(() => {});
    } else {
      await new Promise((resolve) => setTimeout(resolve, 250));
    }
  }
  throw new StepError("APP_PAGE", `no webview page at ${DEV_URL}`);
}

async function shot(page, name) {
  const file = path.join(shotDir, `ui-${name}.png`);
  await page.screenshot({ path: file });
  shots.push(path.relative(repoRoot, file).split(path.sep).join("/"));
}

const visible = (page, selector) => page.locator(selector).first().isVisible().catch(() => false);

/** Waits for the workspace shell; dismisses ZCode's first-run onboarding the way ZCode e2e does (Escape). */
async function enterWorkspace(page) {
  const deadline = Date.now() + 90_000;
  while (Date.now() < deadline) {
    if (await visible(page, '[data-testid="task-settings-button"]')) return;
    if (await visible(page, '[data-testid="onboarding-page"]')) await page.keyboard.press("Escape");
    await page
      .waitForSelector('[data-testid="task-settings-button"], [data-testid="onboarding-page"]', { timeout: 2_000 })
      .catch(() => {});
  }
  throw new StepError("WORKSPACE", "the workspace shell (task-settings-button) never appeared");
}

function wslHasUbuntu() {
  const result = spawnSync("wsl.exe", ["-l", "-q"], { encoding: "utf16le", timeout: 15_000, windowsHide: true });
  if (result.status !== 0) return false;
  return result.stdout
    .split(/\r?\n/)
    .map((line) => line.replace(/\0/g, "").trim())
    .some((line) => /^Ubuntu/i.test(line));
}

/** Returns null when no alert blocks the UI, else its text and whether Escape dismissed it. */
async function dismissBlockingAlert(page) {
  const alert = page.locator('[role="alertdialog"]').first();
  // 提示在启动后异步出现；给它一个有界窗口出现，而不是固定睡眠后检查。
  const appeared = await alert.waitFor({ state: "visible", timeout: 8_000 }).then(
    () => true,
    () => false,
  );
  if (!appeared) return null;
  const text = (await alert.innerText().catch(() => "")).replace(/\s+/g, " ").slice(0, 160);
  await shot(page, "00-blocking-alert");
  await page.keyboard.press("Escape");
  const dismissed = await alert.waitFor({ state: "hidden", timeout: 5_000 }).then(
    () => true,
    () => false,
  );
  return { text, dismissedByEscape: dismissed };
}

async function remoteConnection(page) {
  const result = { dialog: false, wslForm: false, connect: "not attempted" };
  await page.locator('[data-testid="composer-workspace-trigger"]').first().click({ timeout: 20_000 });
  await page.locator('[data-testid="composer-remote-connection"]').first().click({ timeout: 10_000 });
  const dialog = page.locator('[data-testid="ssh-dialog"]').first();
  await dialog.waitFor({ state: "visible", timeout: 15_000 });
  result.dialog = true;
  await shot(page, "01-remote-dialog");

  const wslKind = page.locator('[data-testid="remote-kind-wsl"]').first();
  if (!(await wslKind.isVisible().catch(() => false))) {
    result.connect = "WSL kind tile not shown";
    return result;
  }
  await wslKind.click();
  const distro = page.locator('[data-testid="wsl-distro-select"]').first();
  await distro.waitFor({ state: "visible", timeout: 10_000 });
  result.wslForm = true;
  if (!wslHasUbuntu()) {
    result.connect = "skipped: wsl.exe -l -q does not list Ubuntu";
    await shot(page, "02-wsl-form");
    return result;
  }
  await distro.fill("Ubuntu");
  await shot(page, "02-wsl-form");
  await dialog.locator("form button[type='submit']").first().click({ timeout: 10_000 });
  // 连接结果由 UI 的成功 / 失败标记给出；等待其中之一出现，不用固定睡眠。
  const outcome = await Promise.race([
    page.waitForSelector('[data-testid="ssh-success"]', { timeout: 180_000 }).then(() => "connected"),
    page.waitForSelector('[data-testid="remote-connection-failure"]', { timeout: 180_000 }).then(() => "failed"),
    page.waitForSelector('[data-testid="ssh-dialog"]', { state: "detached", timeout: 180_000 }).then(() => "dialog-closed"),
  ]).catch(() => "timeout");
  await shot(page, "03-wsl-result");
  if (outcome === "failed") {
    const text = await page.locator('[data-testid="remote-connection-failure"]').first().innerText().catch(() => "");
    result.connect = `failed: ${text.slice(0, 200)}`;
  } else {
    result.connect = outcome;
  }
  // 关闭可能残留的对话框，回到聊天界面。
  if (await visible(page, '[data-testid="ssh-dialog"]')) await page.keyboard.press("Escape");
  await page.waitForSelector('[data-testid="ssh-dialog"]', { state: "detached", timeout: 10_000 }).catch(() => {});
  return result;
}

async function webRemoteControl(page, relay) {
  const result = { dialog: false, qr: false, authRequired: false, qrOrigin: null };
  const trigger = page.locator('button[aria-label="Mobile remote control"], button[aria-label="移动端远程控制"]');
  await trigger.first().waitFor({ state: "visible", timeout: 20_000 });
  await trigger.first().click();
  result.dialog = true;
  const outcome = await Promise.race([
    page.waitForSelector('[data-testid="web-remote-control-scan-card"] img', { timeout: 60_000 }).then(() => "qr"),
    page.waitForSelector('[data-testid="web-remote-control-auth-required"]', { timeout: 60_000 }).then(() => "auth"),
  ]).catch(() => "timeout");
  await shot(page, "04-web-remote-control");
  if (outcome === "qr") {
    result.qr = true;
    // 只记录 QR 链接的 origin，不打印配对 token。
    const status = await page
      .evaluate(() => window.__TAURI_INTERNALS__?.invoke("remote_control_call", { method: "getStatus", params: {} }))
      .catch(() => null);
    const qrUrl = typeof status?.qrUrl === "string" ? status.qrUrl : null;
    if (qrUrl) {
      result.qrOrigin = new URL(qrUrl).origin;
      result.qrPointsAtLocalRelay = result.qrOrigin === new URL(relay.httpUrl).origin;
    }
  } else if (outcome === "auth") {
    result.authRequired = true;
  }
  result.relayDeviceAuthInits = relay.state?.deviceAuthInits ?? relay.deviceAuthInits ?? null;
  await page.keyboard.press("Escape");
  return result;
}

async function closeAppGracefully(page, exe) {
  if (exe.exitCode !== null) return { closed: true, exitCode: exe.exitCode, via: "already exited" };
  const exited = new Promise((resolve) => exe.once("exit", (code) => resolve(code)));
  // Tauri window close emits CloseRequested; the Rust shell shuts the Host down, then exits. Not destroy().
  await page
    .evaluate(() => window.__TAURI_INTERNALS__.invoke("plugin:window|close", { label: "main" }))
    .catch(() => {});
  const code = await Promise.race([exited, new Promise((resolve) => setTimeout(() => resolve("timeout"), 30_000))]);
  return { closed: code !== "timeout", exitCode: code, via: "plugin:window|close" };
}

function stopStarted() {
  for (const child of started.reverse()) {
    if (child.exitCode !== null || child.pid === undefined) continue;
    // 只结束本脚本启动的进程树。
    spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], { stdio: "ignore", windowsHide: true });
  }
}

async function buildPhoneKit() {
  const { build } = await import(pathToFileURL(requireFromRepo.resolve("esbuild")).href);
  await build({
    entryPoints: [path.join(moduleDir, "phase4-web-remote/phoneKit.entry.ts")],
    outfile: kitFile,
    bundle: true,
    platform: "node",
    format: "esm",
    target: "node24",
    external: ["ws"],
    logLevel: "warning",
    banner: {
      js: 'import { createRequire as __gateCreateRequire } from "node:module";\nconst require = __gateCreateRequire(import.meta.url);',
    },
  });
  return import(pathToFileURL(kitFile).href);
}

async function main() {
  if (process.platform !== "win32") throw new StepError("PLATFORM", "the WebView2 CDP driver needs Windows");
  if (!existsSync(appExe)) throw new StepError("BUILD", "Worse/target/debug/harness-app.exe is missing (cargo build -p harness-app)");
  if (await portInUse(1430)) throw new StepError("DEV_PORT", "port 1430 is already in use; stop the other dev server first");
  await rm(runDir, { recursive: true, force: true });
  await mkdir(runDir, { recursive: true });
  await mkdir(shotDir, { recursive: true });
  for (const name of await readdir(shotDir)) {
    if (/^ui-.*\.png$/.test(name)) await rm(path.join(shotDir, name), { force: true });
  }
  // 数据根就是隔离 home：凭据 fixture 写在 <home>/.zcode/v2/credentials.json。
  await mkdir(path.join(home, ".zcode", "v2"), { recursive: true });
  await writeFile(path.join(home, ".zcode", "v2", "credentials.json"), JSON.stringify(FIXTURE_CREDENTIALS), { mode: 0o600 });

  const kit = await buildPhoneKit();
  const otherApiCalls = [];
  const remoteApiCalls = [];
  const relay = await kit.createFakeRelayV2({
    users: { [FIXTURE_JWT]: FIXTURE_USER },
    // 本地 fixture 只实现远控 API。其余业务 API 若带着假 token 收到 401，ZCode 会按“凭据失效”登出并弹出
    // “登录已过期”，挡住整个界面；这里对非远控路径回 404（未实现），与 ZCode desktop e2e 用本地 fixture
    // 服务器的做法一致。只记录路径，不记录请求头或 token。
    onHttp: (call) => {
      if (call.path.startsWith("/api/v1/remote/")) {
        remoteApiCalls.push(`${call.method} ${call.path}${call.userId ? "" : " (no fixture user -> 401)"}`);
        return undefined;
      }
      otherApiCalls.push(`${call.method} ${call.path}`);
      return { status: 404, body: { code: 404, msg: "not implemented by the local fixture" } };
    },
  });
  const playwright = await import(pathToFileURL(requireFromRepo.resolve("playwright-core")).href);
  const chromium = playwright.chromium ?? playwright.default.chromium;

  let browser;
  try {
    const vite = spawn(process.execPath, ["scripts/run-vite.cjs", "--configLoader", "native", "--port", "1430", "--strictPort"], {
      cwd: appDir,
      env: process.env,
      stdio: ["ignore", "pipe", "pipe"],
      windowsHide: true,
    });
    started.push(vite);
    await waitForOutput(vite, /localhost:1430/, 120_000, "DEV_SERVER");

    const cdpPort = await freePort();
    const exe = spawn(appExe, [], {
      cwd: repoRoot,
      env: {
        ...process.env,
        WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${cdpPort}`,
        // WebView2 profile 与设备 id 都放进隔离 home，避免写入用户真实的 %LOCALAPPDATA% / %APPDATA%。
        WEBVIEW2_USER_DATA_FOLDER: path.join(home, "webview2"),
        HARNESS_DEVICE_MID: `phase4-ui-${randomUUID()}`,
        HARNESS_LOCALE: "en-US",
        ZCODE_ENDPOINT_ORIGIN: relay.httpUrl,
        ZCODE_WEB_REMOTE_CONTROL_RELAY_WS_URL: relay.wsUrl,
      },
      stdio: ["ignore", "pipe", "pipe"],
      windowsHide: false,
    });
    started.push(exe);
    let exeLog = "";
    // 应用自身输出（Rust shell + Host + 渲染日志）写到 .work/phase4/ui-app.log 供排查；只含测试 fixture 数据。
    const appLog = createWriteStream(path.join(shotDir, "ui-app.log"));
    const onAppOutput = (chunk) => {
      appLog.write(chunk);
      exeLog = (exeLog + chunk.toString()).slice(-8_000);
    };
    exe.stdout.on("data", onAppOutput);
    exe.stderr.on("data", onAppOutput);
    exe.once("exit", () => appLog.end());

    browser = await connectCdp(chromium, cdpPort, exe, 90_000);
    const page = await findAppPage(browser, 60_000);
    page.on("console", (message) => appLog.write(`[renderer:${message.type()}] ${message.text()}\n`));
    await page.waitForLoadState("domcontentloaded");
    await page.waitForSelector("#root *", { timeout: 60_000 }).catch(() => {});
    await shot(page, "00-start");

    const summary = { isolatedHome: true, webviewProfileIsolated: existsSync(path.join(home, "webview2")) };
    try {
      await enterWorkspace(page);
      summary.workspace = true;
    } catch (error) {
      summary.workspace = false;
      notes.push(`workspace: ${error.message}`);
      await shot(page, "00-no-workspace");
    }

    if (summary.workspace) {
      // ZCode 的“登录已过期”提示是 alertdialog；记录出现时间、截图，并尝试用 Escape 关闭，以便继续驱动可达界面。
      await page.evaluate(() => {
        const seen = new WeakSet();
        new MutationObserver(() => {
          for (const node of document.querySelectorAll('[role="alertdialog"]')) {
            if (seen.has(node)) continue;
            seen.add(node);
            console.info(`[phase4-ui] alertdialog appeared at ${new Date().toISOString()}: ${node.textContent?.slice(0, 120)}`);
          }
        }).observe(document.body, { childList: true, subtree: true });
      });
      summary.blockingAlert = await dismissBlockingAlert(page);
      const composerReachable = await page
        .locator('[data-testid="composer-workspace-trigger"]')
        .first()
        .waitFor({ state: "visible", timeout: 15_000 })
        .then(
          () => true,
          () => false,
        );
      if (!composerReachable) {
        summary.signInWall = true;
        await shot(page, "01-sign-in-wall");
        notes.push(
          summary.blockingAlert
            ? `ZCode shows "${summary.blockingAlert.text.split(" To ")[0]}" with the e2e fixture login; dismissing it opens the sign-in wall, ` +
                "so the add-remote-connection entry and the mobile remote-control dialog are not reachable without a real login (not attempted by design)"
            : "the composer (remote-connection entry) is not reachable; the UI shows a sign-in wall",
        );
      }
    }

    if (summary.workspace && !summary.signInWall) {
      try {
        summary.remoteConnection = await remoteConnection(page);
      } catch (error) {
        notes.push(`remote connection: ${String(error.message ?? error).split("\n")[0]}`);
        await shot(page, "01-remote-dialog-error").catch(() => {});
        await page.keyboard.press("Escape").catch(() => {});
      }
      try {
        summary.webRemoteControl = await webRemoteControl(page, relay);
      } catch (error) {
        notes.push(`web remote control: ${String(error.message ?? error).split("\n")[0]}`);
        await shot(page, "04-web-remote-control-error").catch(() => {});
      }
    }

    summary.close = await closeAppGracefully(page, exe);
    await browser.close().catch(() => {});
    browser = undefined;
    if (!summary.close.closed) notes.push("the window did not exit within 30s of a graceful close");

    const rc = summary.remoteConnection;
    const wr = summary.webRemoteControl;
    if (!summary.signInWall) {
      if (!rc?.dialog) notes.push("add-remote-connection dialog not reached");
      else if (rc.connect !== "connected" && !String(rc.connect).startsWith("skipped")) notes.push(`WSL connect: ${rc.connect}`);
      if (!wr?.qr) notes.push(wr?.authRequired ? "web remote control shows the sign-in wall" : "no pairing QR rendered");
      else if (wr.qrPointsAtLocalRelay === false) notes.push(`QR origin ${wr.qrOrigin} is not the local relay`);
    }
    summary.screenshots = shots;
    summary.localFixtureNonRemoteApiCalls = [...new Set(otherApiCalls)].slice(0, 20);
    summary.localFixtureRemoteApiCalls = [...new Set(remoteApiCalls)].slice(0, 20);
    if (notes.length === 0) {
      process.stdout.write(`PHASE4_UI_OK\n${JSON.stringify(summary, null, 2)}\n`);
    } else {
      process.stdout.write(`PHASE4_UI_PARTIAL ${notes.join("; ")}\n${JSON.stringify(summary, null, 2)}\n`);
      if (!summary.close.closed) process.stderr.write(`harness-app output (tail):\n${exeLog.slice(-3_000)}\n`);
    }
  } finally {
    if (browser) await browser.close().catch(() => {});
    stopStarted();
    await relay.close().catch(() => {});
    await rm(runDir, { recursive: true, force: true }).catch(() => {});
  }
}

main().then(
  () => process.exit(0),
  (error) => {
    stopStarted();
    const step = error instanceof StepError ? error.step : "UNEXPECTED";
    process.stdout.write(`PHASE4_UI_FAIL ${step}\nreason: ${error instanceof Error ? error.stack ?? error.message : String(error)}\n`);
    if (shots.length > 0) process.stdout.write(`screenshots: ${shots.join(", ")}\n`);
    process.exit(1);
  },
);
