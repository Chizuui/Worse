// P6 of the Phase 5 plugins gate: real GUI (Windows, WebView2 over CDP) with the debug harness-app.
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { createWriteStream, existsSync } from "node:fs";
import { mkdir } from "node:fs/promises";
import { createRequire } from "node:module";
import path from "node:path";
import process from "node:process";
import { pathToFileURL } from "node:url";
import { isolatedHomeDir } from "../lib/isolatedHome.mjs";
import { repoRoot } from "../phase2/environment.mjs";
import { fail, withTimeout } from "../phase2/gate.mjs";
import { connectRpc } from "../phase2/rpcClient.mjs";
import { attachSocket } from "../phase2/wsClient.mjs";
import { runDir } from "./common.mjs";
import { assertGuestProbe, probeGuest } from "./guestProbe.mjs";
import {
  DEV_URL,
  appDir,
  appExe,
  closeAppGracefully,
  connectCdp,
  evidenceDir,
  findAppPage,
  freePort,
  portInUse,
  startDeadCloud,
  started,
  stopStarted,
  waitForOutput,
} from "./guiProcess.mjs";

export async function runGui(modules) {
  if (process.env.HARNESS_PHASE5_GUI === "0") return { skipped: "HARNESS_PHASE5_GUI=0" };
  if (process.platform !== "win32") fail("P6_PLATFORM", "the WebView2 CDP driver needs Windows; set HARNESS_PHASE5_GUI=0 to skip explicitly");
  if (!existsSync(appExe)) fail("P6_BUILD", "Worse/target/debug/harness-app.exe is missing", "cargo build -p harness-app (from Worse/)");
  if (await portInUse(1430)) fail("P6_DEV_PORT", "port 1430 is already in use; stop the other dev server first");
  await mkdir(evidenceDir, { recursive: true });
  const home = isolatedHomeDir();
  const workspace = path.join(runDir, "gui-workspace");
  await mkdir(workspace, { recursive: true });
  const requireFromRepo = createRequire(path.join(repoRoot, "package.json"));
  const playwright = await import(pathToFileURL(requireFromRepo.resolve("playwright-core")).href);
  const chromium = playwright.chromium ?? playwright.default.chromium;
  const cloud = await startDeadCloud();
  const summary = {};
  let browser;
  let connection;
  let port;
  try {
    const vite = spawn(process.execPath, ["scripts/run-vite.cjs", "--configLoader", "native", "--port", "1430", "--strictPort"], {
      cwd: appDir,
      env: process.env,
      stdio: ["ignore", "pipe", "pipe"],
      windowsHide: true,
    });
    started.push(vite);
    await waitForOutput(vite, /localhost:1430/, 120_000, "P6_DEV_SERVER");

    const cdpPort = await freePort();
    const exe = spawn(appExe, [], {
      cwd: repoRoot,
      env: {
        ...process.env,
        WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${cdpPort}`,
        // WebView2 profile 放进隔离 home，避免写入真实 %LOCALAPPDATA%。
        WEBVIEW2_USER_DATA_FOLDER: path.join(home, "webview2"),
        HARNESS_DEVICE_MID: `phase5-plugins-${randomUUID()}`,
        HARNESS_LOCALE: "en-US",
        ZCODE_ENDPOINT_ORIGIN: `http://127.0.0.1:${cloud.port}`,
        ZCODE_WEB_REMOTE_CONTROL_RELAY_WS_URL: `ws://127.0.0.1:${cloud.port}/relay`,
      },
      stdio: ["ignore", "pipe", "pipe"],
      windowsHide: false,
    });
    started.push(exe);
    const appLog = createWriteStream(path.join(evidenceDir, "plugins-gui-app.log"));
    exe.stdout.on("data", (chunk) => appLog.write(chunk));
    exe.stderr.on("data", (chunk) => appLog.write(chunk));
    exe.once("exit", () => appLog.end());
    // 修复说明：Host 初始化前申请的 grant 都携带 init-local；若与渲染层首个附着竞争，会出现
    // “init-local became active on an already initialized Host”，后到的连接收不到 Initialize。
    // 先等渲染层自己的附着激活（日志事件），之后的 grant 才是 attach-service-port。
    const rendererAttached = waitForOutput(exe, /Host attachment active[^\n]*message_type=InitLocal/, 120_000, "P6_RENDERER_ATTACH");

    browser = await connectCdp(chromium, cdpPort, exe, 90_000);
    const page = await findAppPage(browser, 60_000);
    page.on("console", (message) => appLog.write(`[renderer:${message.type()}] ${message.text()}\n`));
    await page.waitForLoadState("domcontentloaded");
    await page.waitForFunction(() => Boolean(window.__TAURI_INTERNALS__), undefined, { timeout: 60_000 });

    // A second attachment to the app's own Host (same grant path the renderer uses).
    await rendererAttached;
    const grant = await page.evaluate(() => window.__TAURI_INTERNALS__.invoke("host_open_attachment"));
    if (!grant?.url || !grant?.token) fail("P6_GRANT", `host_open_attachment returned ${JSON.stringify(Object.keys(grant ?? {}))}`);
    port = await attachSocket(modules.WebSocket, grant.url, DEV_URL, grant, "P6_ATTACH_WS");
    connection = await connectRpc(modules.rpc, port, "P6_INITIALIZE");
    const handle = await withTimeout(
      connection.client.getChannel("gen-ui").call("prepareSandbox", [
        {
          workspacePath: workspace,
          sessionId: "phase5-gui",
          path: path.join(workspace, "phase5-gui-widget.html"),
          html: '<div id="phase5">phase5 gui fixture</div>',
          ownerWebContentsId: 1,
          instanceKey: "phase5:gui",
        },
      ]),
      60_000,
      "P6_PREPARE",
      "gen-ui.prepareSandbox",
    );
    if (!handle?.shellUrl?.startsWith("http://hsandbox.shell-")) fail("P6_PREPARE", `unexpected shellUrl ${String(handle?.shellUrl)}`);
    const pluginOriginPrefix = "http://hsandbox.plugin-";

    // Mount exactly what ZCode's pluginUiPagePlane mounts; the Harness platform adopts it.
    await page.evaluate((shellUrl) => {
      const host = document.createElement("div");
      host.id = "phase5-sandbox-host";
      host.style.cssText = "position:fixed;right:16px;bottom:16px;width:360px;height:200px;z-index:2147483647;display:flex;background:#fff;border:2px solid #4a7";
      const element = document.createElement("webview");
      element.setAttribute("data-zcode-retained-sandbox", "phase5");
      element.style.cssText = "display:flex;width:100%;height:100%";
      const events = [];
      window.__phase5Events = events;
      for (const type of ["dom-ready", "did-frame-finish-load", "did-start-navigation"]) {
        element.addEventListener(type, (event) => events.push({ type, isMainFrame: event.isMainFrame ?? null }));
      }
      element.setAttribute("src", shellUrl);
      host.append(element);
      document.body.append(host);
    }, handle.shellUrl);
    await page.waitForFunction(
      () => window.__phase5Events.some((event) => event.type === "did-frame-finish-load" && event.isMainFrame === false),
      undefined,
      { timeout: 30_000 },
    ).catch(() => undefined);
    const events = await page.evaluate(() => window.__phase5Events);
    if (!events.some((event) => event.type === "dom-ready")) fail("P6_ADOPT", `no dom-ready; events=${JSON.stringify(events)}`);
    if (!events.some((event) => event.type === "did-frame-finish-load" && event.isMainFrame === false)) {
      fail("P6_PLUGIN_FRAME", `plugin frame never reported load; events=${JSON.stringify(events)}`);
    }
    const shellFrame = page.frames().find((frame) => frame.url().startsWith("http://hsandbox.shell-"));
    const pluginFrame = page.frames().find((frame) => frame.url().startsWith(pluginOriginPrefix));
    if (!shellFrame || !pluginFrame) fail("P6_FRAMES", `frames: ${page.frames().map((frame) => frame.url().slice(0, 60)).join(", ")}`);
    const pluginProbe = await pluginFrame.evaluate(probeGuest);
    const shellProbe = await shellFrame.evaluate(probeGuest);
    if (!pluginProbe.text.includes("phase5 gui fixture")) fail("P6_PLUGIN_TEXT", `plugin text: ${pluginProbe.text}`);
    // Gen UI 记录未声明 clipboardWrite：shell 不向插件帧转授，插件帧应为 false；shell 帧本身由渲染层授予 clipboard-write。
    assertGuestProbe("P6_GUEST_PLUGIN", pluginProbe, false);
    assertGuestProbe("P6_GUEST_SHELL", shellProbe, true);
    await page.screenshot({ path: path.join(evidenceDir, "plugins-gui.png") });

    // Retained move: ZCode moves sandbox elements with moveBefore; the guest must not reload.
    const move = await page.evaluate(() => {
      const element = document.querySelector('webview[data-zcode-retained-sandbox="phase5"]');
      const before = window.__phase5Events.length;
      const target = document.createElement("div");
      target.id = "phase5-move-target";
      target.style.cssText = "position:fixed;left:16px;bottom:16px;width:360px;height:200px;z-index:2147483647;display:flex";
      document.body.append(target);
      if (typeof target.moveBefore !== "function") return { supported: false };
      target.moveBefore(element, null);
      return { supported: true, before, hasCallback: typeof element.connectedMoveCallback === "function" };
    });
    if (move.supported) {
      // 等一个渲染帧之后再比较事件数：重新加载会产生 did-start-navigation。
      await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
      const after = await page.evaluate(() => window.__phase5Events.length);
      const stillThere = page.frames().some((frame) => frame === pluginFrame && !frame.isDetached());
      if (after !== move.before || !stillThere || !move.hasCallback) {
        fail("P6_MOVE", `moveBefore reloaded or detached the guest (events ${move.before}→${after}, attached=${stillThere})`);
      }
    }

    const disposed = await page.evaluate(
      ({ sandboxId, initId }) => window.__TAURI_INTERNALS__.invoke("plugin_sandbox_call", { method: "dispose", params: { sandboxId, initId } }),
      { sandboxId: handle.sandboxId, initId: handle.initId },
    );
    // 已释放的沙箱：重新打开同一 shell URL 必须拿不到 shell 文档（协议层 404）。
    const afterDispose = await page.evaluate(async (shellUrl) => {
      const frame = document.createElement("iframe");
      frame.id = "phase5-after-dispose";
      frame.src = shellUrl.replace(/#.*$/, "");
      const loaded = new Promise((resolve) => frame.addEventListener("load", resolve, { once: true }));
      document.body.append(frame);
      await loaded;
      return true;
    }, handle.shellUrl);
    const disposedFrame = page.frames().find((frame) => frame.name() === "" && frame.parentFrame() === page.mainFrame() && frame.url().startsWith("http://hsandbox.shell-") && frame !== shellFrame);
    const disposedHasShell = disposedFrame
      ? await disposedFrame.evaluate(() => Boolean(document.querySelector('meta[name="harness-sandbox"]'))).catch(() => false)
      : false;
    if (!afterDispose || disposedHasShell) fail("P6_DISPOSE", "the shell document is still served after dispose");
    const forbidden = await page
      .evaluate(() => window.__TAURI_INTERNALS__.invoke("plugin_sandbox_call", { method: "fetch", params: {} }).then(() => "accepted", () => "rejected"));
    if (forbidden !== "rejected") fail("P6_RENDERER_ALLOWLIST", "the renderer could call the sandbox fetch method");

    summary.gui = {
      events: events.map((event) => `${event.type}${event.isMainFrame === false ? "(sub)" : ""}`),
      pluginFrame: { origin: pluginProbe.origin.replace(/[a-f0-9]{32}/, "<h32>"), policy: pluginProbe.policy, getUserMedia: pluginProbe.getUserMedia, geolocation: pluginProbe.geolocation, ipcFetch: pluginProbe.ipcFetch },
      shellFrame: { origin: shellProbe.origin.replace(/[a-f0-9]{32}/, "<h32>"), policy: shellProbe.policy, getUserMedia: shellProbe.getUserMedia },
      guestTauriGlobals: { __TAURI_INTERNALS__: pluginProbe.tauriInternals, ipc: pluginProbe.ipcGlobal, __TAURI__: pluginProbe.tauri, chromeWebview: pluginProbe.chromeWebview },
      guestInvoke: {
        plugin: { host_status: pluginProbe.invokeHostStatus, host_open_attachment: pluginProbe.invokeOpenAttachment },
        shell: { host_status: shellProbe.invokeHostStatus, host_open_attachment: shellProbe.invokeOpenAttachment },
      },
      move: move.supported ? "moveBefore, no reload" : "moveBefore unsupported (fallback re-mount)",
      dispose: { result: disposed ?? null, shellAfterDispose: disposedHasShell ? "served" : "404" },
      rendererFetchCall: forbidden,
      screenshot: "Worse/.work/phase5/plugins-gui.png",
      cloudRequests: cloud.paths.length,
    };
    connection.dispose?.();
    port.close();
    connection = undefined;
    summary.gui.close = await closeAppGracefully(page, exe);
    if (!summary.gui.close.closed) fail("P6_CLOSE", "the app did not exit within 30 s of the window close request");
    return summary;
  } finally {
    try {
      connection?.dispose?.();
      port?.close();
    } catch {
      // 清理阶段的错误不覆盖主结果。
    }
    await browser?.close().catch(() => undefined);
    stopStarted();
    cloud.server.close();
  }
}
