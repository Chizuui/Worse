// Phase 5 browser gate — ZCode reference backend.
//
// Runs ZCode's own Electron-Main browser owner (BrowserGuestManager, unchanged source) inside real
// Electron with a real `<webview>` guest, wired the way packages/desktop/src/main/index.ts wires it:
// onOpenTabRequested → the window creates a `<webview>` → `did-attach-webview` → attachGuest. The
// gate drives it over a loopback TCP line channel (PHASE5_CONTROL_PORT) with the exact `browser-execute-request` the Host bridge
// produces, so both backends answer the same Host → Main request shapes.
//
// Differences from production (documented in the spec): the screenshot surface coordinator is the
// immediate lease (no renderer pane to prepare) and the window is shown off-screen.
import { connect } from "node:net";
import { app, BrowserWindow, nativeImage } from "electron";
import { BrowserGuestManager } from "../../../../packages/desktop/src/main/browserView/browserGuestManager.ts";

let sendLine: (payload: unknown) => void = () => undefined;

function reply(payload: unknown): void {
  sendLine(payload);
}

const userData = process.env.PHASE5_ELECTRON_USER_DATA;
if (userData) app.setPath("userData", userData);
app.disableHardwareAcceleration();
// 参考窗口放在屏幕外：关闭 Windows 原生遮挡计算，否则合成器视其为被遮挡而不产帧，截图会一直等待。
app.commandLine.appendSwitch("disable-features", "CalculateNativeWinOcclusion");
app.commandLine.appendSwitch("disable-renderer-backgrounding");
app.commandLine.appendSwitch("disable-background-timer-throttling");

void app.whenReady().then(async () => {
  const win = new BrowserWindow({
    show: true,
    x: -4000,
    y: -4000,
    width: 1400,
    height: 900,
    skipTaskbar: true,
    focusable: false,
    webPreferences: { webviewTag: true, contextIsolation: true, nodeIntegration: false, backgroundThrottling: false }
  });
  await win.loadURL("data:text/html,<!doctype html><html><body style='margin:0;background:#fff'></body></html>");

  const pendingTabs: Array<{ tabId: string; owner: { workspaceKey: string; sessionId: string; remoteSessionId?: string } }> = [];

  const manager = new BrowserGuestManager(
    () => undefined,
    undefined,
    (tabId) => {
      void win.webContents
        .executeJavaScript(`document.getElementById(${JSON.stringify(tabId)})?.remove()`)
        .catch(() => undefined);
    },
    (tabId, owner) => {
      pendingTabs.push({ tabId, owner });
      void win.webContents.executeJavaScript(
        `(() => { const w = document.createElement("webview"); w.id = ${JSON.stringify(tabId)};` +
          ` w.setAttribute("src", "about:blank"); w.style.cssText = "display:inline-flex;width:1280px;height:720px";` +
          ` document.body.appendChild(w); })()`
      );
    },
    undefined,
    undefined,
    (base64Png, target) => {
      const source = nativeImage.createFromBuffer(Buffer.from(base64Png, "base64"));
      if (source.isEmpty()) return undefined;
      const resized = source.resize({ width: target.width, height: target.height, quality: "best" });
      return resized.isEmpty() ? undefined : resized.toPNG().toString("base64");
    },
    {
      prepare: async (input) => ({
        surfaceScale: 1,
        webContentsId: input.webContentsId,
        viewport: input.viewport,
        release: () => undefined
      })
    },
    { warn: () => undefined },
    (windowId) => BrowserWindow.fromId(windowId)
  );

  win.webContents.on("did-attach-webview", (_event, guest) => {
    const entry = pendingTabs.shift();
    if (!entry) return;
    if (process.env.PHASE5_DEBUG === "1") {
      for (const name of ["did-start-navigation", "did-navigate", "did-fail-load", "did-finish-load", "dom-ready", "did-stop-loading"]) {
        guest.on(name as "dom-ready", (...args: unknown[]) =>
          process.stderr.write(`[ref] ${Date.now()} ${name} ${JSON.stringify(args.slice(1, 3))} url=${guest.getURL()}\n`)
        );
      }
    }
    // 与 UnifiedBrowserView 一致：<webview>.getWebContentsId() 只在 dom-ready 之后可用，renderer 此时才上报 attach。
    // 另外等初始 about:blank 的 did-finish-load：dom-ready 时它尚未发出，紧随其后的 navigate 会被这次过期的
    // did-finish-load 提前结算（Electron loadURL 语义），导致 ZCode 返回 state.url=about:blank。该竞态记录在
    // spec 第 3 节；gate 比较的是稳态行为，所以参考端在初始加载结束后再 attach。
    guest.once("did-stop-loading", () => {
      manager.attachGuest(entry.tabId, guest.id, {
        active: true,
        windowId: win.id,
        workspaceKey: entry.owner.workspaceKey,
        sessionId: entry.owner.sessionId,
        ...(entry.owner.remoteSessionId ? { remoteSessionId: entry.owner.remoteSessionId } : {})
      });
    });
  });

  // Electron 是 GUI 子系统程序，Windows 上 process.stdin 不可读（立即 end）；改用 gate 提供的 loopback TCP 通道。
  const socket = connect(Number(process.env.PHASE5_CONTROL_PORT), "127.0.0.1");
  socket.setEncoding("utf8");
  sendLine = (payload) => socket.write(`${JSON.stringify(payload)}\n`);
  let buffer = "";
  socket.on("data", (chunk: string) => {
    buffer += chunk;
    let index = buffer.indexOf("\n");
    while (index !== -1) {
      const line = buffer.slice(0, index).trim();
      buffer = buffer.slice(index + 1);
      index = buffer.indexOf("\n");
      if (line) void handle(JSON.parse(line) as { id: number; kind: string; request?: Record<string, unknown> });
    }
  });
  const quit = () => {
    manager.disposeAll();
    app.quit();
  };
  socket.on("end", quit);
  socket.on("error", quit);

  async function handle(message: { id: number; kind: string; request?: Record<string, unknown> }): Promise<void> {
    if (message.kind === "identity") {
      reply({ id: message.id, identity: manager.getBackendIdentity(win.id) });
      return;
    }
    const request = message.request ?? {};
    // 与 Main runBrowserCommandOnView 相同的 context 映射。
    const workspaceKey =
      (typeof request.workspaceIdentity === "string" ? request.workspaceIdentity.trim() : "") ||
      (request.workspaceKey as string | undefined) ||
      (request.workspacePath as string | undefined) ||
      String(request.sessionId);
    try {
      const result = await manager.execute(
        {
          requestId: String(request.requestId),
          ...(request.controlScopeId ? { controlScopeId: String(request.controlScopeId) } : {}),
          browserId: (request.browserId as string | undefined) ?? "legacy-iab",
          browserGeneration: (request.browserGeneration as number | undefined) ?? 0,
          windowId: win.id,
          workspaceKey,
          sessionId: String(request.sessionId),
          ...(request.turnId ? { turnId: String(request.turnId) } : {}),
          clientMode: (request.clientMode as "desktop-continuous" | undefined) ?? "desktop-continuous"
        },
        request.command as Parameters<typeof manager.execute>[1]
      );
      reply({ id: message.id, result });
    } catch (error) {
      reply({
        id: message.id,
        result: { ok: false, error: { code: "execution_error", message: error instanceof Error ? error.message : String(error) }, elapsedMs: 0 }
      });
    }
  }

  reply({ ready: true, electron: process.versions.electron });
});
