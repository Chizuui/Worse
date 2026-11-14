// Phase 5: a Chromium page target presented to ZCode's BrowserGuestManager as the Electron `<webview>`
// guest `webContents` it expects (the `GuestWebContents` surface in browserGuestManager.ts).
//
// Two CDP sessions per target, both flattened on the one browser pipe:
//   - an internal session (always attached): navigation, lifecycle events, evaluate, capture,
//     history — the Electron `webContents` methods;
//   - a debugger session that exists only between `debugger.attach()` and `debugger.detach()`, so
//     domain state is reset on detach exactly like `webContents.debugger` (the manager relies on that
//     for its idle-release / restore barrier).
import { EventEmitter } from "node:events";
import { rename } from "node:fs/promises";
import path from "node:path";
import { log } from "./log.ts";
import type { CdpConnection, CdpParams } from "./browserCdp.ts";

type Listener = (...args: unknown[]) => void;
type DebuggerMessageListener = (event: unknown, method: string, params: unknown, sessionId?: string) => void;

interface HistoryEntry {
  id: number;
  url: string;
  title: string;
}

/** Electron `DownloadItem` subset used by the manager (`getSavePath`, `once("done")`). */
export class CdpDownloadItem {
  readonly #emitter = new EventEmitter();
  #savePath: string;
  #done = false;

  constructor(savePath: string) {
    this.#savePath = savePath;
  }

  getSavePath(): string {
    return this.#savePath;
  }

  once(event: "done", listener: (event: unknown, state: string) => void): void {
    this.#emitter.once(event, listener);
  }

  finish(state: "completed" | "cancelled" | "interrupted", finalPath?: string): void {
    if (this.#done) return;
    this.#done = true;
    if (finalPath !== undefined) this.#savePath = finalPath;
    this.#emitter.emit("done", {}, state);
  }
}

export interface CdpGuestDeps {
  connection: CdpConnection;
  targetId: string;
  id: number;
  /** Maps a CDP session to this guest so the registry routes its events here. */
  bindSession(sessionId: string, guest: CdpGuest): void;
  unbindSession(sessionId: string): void;
}

/** Text of a CDP `exceptionDetails`, matching what Electron's executeJavaScript rejects with. */
function describeException(details: CdpParams): string {
  const exception = details.exception as { description?: string; value?: unknown } | undefined;
  if (typeof exception?.description === "string") return exception.description;
  if (exception?.value !== undefined) return String(exception.value);
  return typeof details.text === "string" ? details.text : "Script failed to execute";
}

export class CdpGuest {
  readonly id: number;
  readonly targetId: string;
  /** Harness guests have no host renderer zoom (Electron: `hostWebContents.getZoomFactor()`). */
  readonly hostWebContents = null;
  /** Only the recorder reads it; Harness recording is reported unavailable (see browserOwner). */
  readonly mainFrame: unknown = Object.freeze({ harnessTarget: true });

  readonly #deps: CdpGuestDeps;
  readonly #events = new EventEmitter();
  readonly #downloadEvents = new EventEmitter();
  readonly #debuggerListeners = new Set<DebuggerMessageListener>();
  readonly #debuggerChildSessions = new Set<string>();
  #internalSession = "";
  #debuggerAttached = false;
  #debuggerSession: Promise<string> | null = null;
  #debuggerSessionId: string | null = null;
  #destroyed = false;
  #url = "about:blank";
  #title = "";
  #zoom = 1;
  #history: { entries: HistoryEntry[]; index: number } = { entries: [], index: -1 };
  #loadWaiters = new Set<{ resolve(): void; reject(error: Error): void }>();

  readonly debugger = {
    isAttached: (): boolean => this.#debuggerAttached && !this.#destroyed,
    attach: (_protocolVersion?: string): void => {
      if (this.#destroyed) throw new Error("Target is destroyed");
      if (this.#debuggerAttached) throw new Error("Debugger is already attached to the target");
      this.#debuggerAttached = true;
      const pending = this.#deps.connection
        .send("Target.attachToTarget", { targetId: this.targetId, flatten: true })
        .then((result) => {
          const sessionId = String(result.sessionId ?? "");
          if (sessionId === "") throw new Error("Target.attachToTarget returned no sessionId");
          this.#deps.bindSession(sessionId, this);
          if (this.#debuggerSession !== pending) {
            // attach 之后、会话建立之前已 detach：立即释放这个迟到的会话，保持“detach 即清空域状态”。
            this.#deps.unbindSession(sessionId);
            void this.#deps.connection.send("Target.detachFromTarget", { sessionId }).catch(() => undefined);
            throw new Error("Debugger was detached");
          }
          this.#debuggerSessionId = sessionId;
          return sessionId;
        });
      pending.catch(() => undefined);
      this.#debuggerSession = pending;
    },
    detach: (): void => {
      if (!this.#debuggerAttached) return;
      this.#debuggerAttached = false;
      const sessionId = this.#debuggerSessionId;
      this.#debuggerSession = null;
      this.#debuggerSessionId = null;
      for (const child of this.#debuggerChildSessions) this.#deps.unbindSession(child);
      this.#debuggerChildSessions.clear();
      if (sessionId !== null) {
        this.#deps.unbindSession(sessionId);
        void this.#deps.connection.send("Target.detachFromTarget", { sessionId }).catch(() => undefined);
      }
    },
    sendCommand: async (method: string, params?: unknown, sessionId?: string): Promise<unknown> => {
      if (!this.#debuggerAttached || this.#debuggerSession === null) {
        throw new Error("Debugger is not attached");
      }
      const rootSession = await this.#debuggerSession;
      const target = sessionId !== undefined && sessionId !== "" ? sessionId : rootSession;
      return await this.#deps.connection.send(method, (params ?? {}) as CdpParams, target);
    },
    on: (_event: "message", listener: DebuggerMessageListener): void => {
      this.#debuggerListeners.add(listener);
    },
    removeListener: (_event: "message", listener: DebuggerMessageListener): void => {
      this.#debuggerListeners.delete(listener);
    }
  };

  readonly session = {
    on: (event: "will-download", listener: Listener): void => {
      this.#downloadEvents.on(event, listener);
    },
    removeListener: (event: "will-download", listener: Listener): void => {
      this.#downloadEvents.removeListener(event, listener);
    }
  };

  readonly navigationHistory = {
    canGoBack: (): boolean => this.#history.index > 0,
    canGoForward: (): boolean => this.#history.index >= 0 && this.#history.index < this.#history.entries.length - 1,
    goBack: (): void => this.#goToOffset(-1),
    goForward: (): void => this.#goToOffset(1),
    getAllEntries: (): Array<{ url: string; title?: string }> =>
      this.#history.entries.map((entry) => ({ url: entry.url, title: entry.title })),
    getActiveIndex: (): number => this.#history.index,
    // Chromium 不支持从外部注入历史栈；恢复退化为导航到当前条目（规格中标注为 degraded）。
    restore: async (options: { entries: Array<{ url: string }>; index: number }): Promise<void> => {
      const entry = options.entries[options.index] ?? options.entries.at(-1);
      if (entry !== undefined && entry.url !== "") await this.loadURL(entry.url);
    }
  };

  private constructor(deps: CdpGuestDeps) {
    this.#deps = deps;
    this.id = deps.id;
    this.targetId = deps.targetId;
  }

  /** Attaches the internal session and enables the domains the Electron surface needs. */
  static async create(deps: CdpGuestDeps): Promise<CdpGuest> {
    const guest = new CdpGuest(deps);
    const attached = await deps.connection.send("Target.attachToTarget", { targetId: deps.targetId, flatten: true });
    guest.#internalSession = String(attached.sessionId ?? "");
    if (guest.#internalSession === "") throw new Error("Target.attachToTarget returned no sessionId");
    deps.bindSession(guest.#internalSession, guest);
    const send = (method: string, params: CdpParams = {}) => deps.connection.send(method, params, guest.#internalSession);
    await Promise.all([send("Page.enable"), send("Inspector.enable").catch(() => ({})), send("Page.setLifecycleEventsEnabled", { enabled: true })]);
    // 内部会话不得拦截 JS 弹窗：manager 通过 debugger 会话的 Page 域处理（与 Electron guest 一致）。
    await guest.#refreshHistory();
    return guest;
  }

  get internalSessionId(): string {
    return this.#internalSession;
  }

  // ---- Electron webContents surface -------------------------------------------------------

  getType(): string {
    return "webview";
  }

  getZoomFactor(): number {
    return this.#zoom;
  }

  setZoomFactor(factor: number): void {
    this.#zoom = factor;
  }

  isDestroyed(): boolean {
    return this.#destroyed;
  }

  getURL(): string {
    return this.#url;
  }

  getTitle(): string {
    return this.#title;
  }

  isCurrentlyAudible(): boolean {
    return false;
  }

  isBeingCaptured(): boolean {
    return false;
  }

  async loadURL(url: string): Promise<void> {
    this.#assertAlive();
    const loaded = new Promise<void>((resolve, reject) => {
      const waiter = {
        resolve: () => {
          this.#loadWaiters.delete(waiter);
          resolve();
        },
        reject: (error: Error) => {
          this.#loadWaiters.delete(waiter);
          reject(error);
        }
      };
      this.#loadWaiters.add(waiter);
    });
    loaded.catch(() => undefined);
    let result: CdpParams;
    try {
      result = await this.#send("Page.navigate", { url });
    } catch (error) {
      this.#rejectLoads(error instanceof Error ? error : new Error(String(error)));
      throw error;
    }
    if (typeof result.errorText === "string" && result.errorText !== "") {
      // 与 Electron loadURL 一致：导航失败时 reject（ERR_* 文本）。
      const error = new Error(`${result.errorText} (-1) loading '${url}'`);
      this.#rejectLoads(error);
      throw error;
    }
    if (result.loaderId === undefined) {
      // 同文档导航（hash 变化）不会产生新的 load。
      await this.#refreshHistory();
      this.#resolveLoads();
    }
    await loaded;
  }

  reload(): void {
    void this.#send("Page.reload").catch((error: unknown) => log("debug", "[browser] reload failed", error));
  }

  stop(): void {
    void this.#send("Page.stopLoading").catch(() => undefined);
  }

  async capturePage(): Promise<{ toPNG(): Buffer }> {
    this.#assertAlive();
    const result = await this.#send("Page.captureScreenshot", { format: "png" });
    const data = typeof result.data === "string" ? result.data : "";
    return { toPNG: () => Buffer.from(data, "base64") };
  }

  async executeJavaScript(script: string, userGesture = false): Promise<unknown> {
    this.#assertAlive();
    const result = await this.#send("Runtime.evaluate", {
      expression: script,
      awaitPromise: true,
      returnByValue: true,
      userGesture
    });
    if (result.exceptionDetails !== undefined) {
      throw new Error(describeException(result.exceptionDetails as CdpParams));
    }
    return (result.result as { value?: unknown } | undefined)?.value;
  }

  close(_options?: { waitForBeforeUnload?: boolean }): void {
    if (this.#destroyed) return;
    void this.#deps.connection.send("Target.closeTarget", { targetId: this.targetId }).catch(() => undefined);
  }

  on(event: string, listener: Listener): void {
    this.#events.on(event, listener);
  }

  once(event: string, listener: Listener): void {
    this.#events.once(event, listener);
  }

  removeListener(event: string, listener: Listener): void {
    this.#events.removeListener(event, listener);
  }

  // ---- registry callbacks -----------------------------------------------------------------

  /** Routes one CDP event received on a session bound to this guest. */
  handleSessionEvent(method: string, params: CdpParams, sessionId: string): void {
    if (sessionId === this.#internalSession) {
      this.#handleInternalEvent(method, params);
      return;
    }
    if (method === "Target.attachedToTarget" && sessionId === this.#debuggerSessionId) {
      // 由调试会话 auto-attach 出的子会话（OOPIF/worker）也走同一个 debugger 'message' 通道。
      const child = String((params as { sessionId?: unknown }).sessionId ?? "");
      if (child !== "") {
        this.#debuggerChildSessions.add(child);
        this.#deps.bindSession(child, this);
      }
    } else if (method === "Target.detachedFromTarget") {
      const child = String((params as { sessionId?: unknown }).sessionId ?? "");
      if (this.#debuggerChildSessions.delete(child)) this.#deps.unbindSession(child);
    }
    if (!this.#debuggerAttached) return;
    const reportedSession = sessionId === this.#debuggerSessionId ? "" : sessionId;
    for (const listener of Array.from(this.#debuggerListeners)) {
      try {
        listener({}, method, params, reportedSession);
      } catch (error) {
        log("warn", `[browser] debugger listener threw on ${method}`, error);
      }
    }
  }

  /** Browser-level `Target.targetInfoChanged`. */
  updateTargetInfo(info: { url?: unknown; title?: unknown }): void {
    if (typeof info.url === "string") this.#url = info.url;
    if (typeof info.title === "string") this.#title = info.title;
  }

  /** Browser-level `Browser.downloadWillBegin` for a download started by this page. */
  beginDownload(item: CdpDownloadItem): void {
    this.#downloadEvents.emit("will-download", {}, item, this);
  }

  /** Target destroyed (closed by the page, by `close()` or by the browser exiting). */
  markDestroyed(reason: string): void {
    if (this.#destroyed) return;
    this.#destroyed = true;
    this.#debuggerAttached = false;
    this.#rejectLoads(new Error(`ERR_ABORTED (-3) target destroyed: ${reason}`));
    this.#events.emit("destroyed");
    this.#events.removeAllListeners();
    this.#downloadEvents.removeAllListeners();
    this.#debuggerListeners.clear();
  }

  /** `Inspector.targetCrashed` / `Target.targetCrashed`: Electron's `render-process-gone`. */
  markCrashed(): void {
    this.#events.emit("render-process-gone", {}, { reason: "crashed", exitCode: -1 });
  }

  // ---- internals --------------------------------------------------------------------------

  #handleInternalEvent(method: string, params: CdpParams): void {
    const frameId = typeof params.frameId === "string" ? params.frameId : undefined;
    switch (method) {
      case "Page.frameStartedLoading":
        if (frameId === this.targetId) this.#events.emit("did-start-loading");
        return;
      case "Page.frameStoppedLoading":
        if (frameId === this.targetId) {
          // 先刷新 URL/标题/历史缓存再结算 loadURL：Electron 的 loadURL resolve 时 getTitle() 已是新文档标题。
          void this.#refreshHistory().finally(() => {
            this.#resolveLoads();
            this.#events.emit("did-stop-loading");
          });
        }
        return;
      case "Page.frameNavigated": {
        const frame = params.frame as { id?: string; parentId?: string; url?: string; urlFragment?: string } | undefined;
        if (frame?.parentId === undefined && typeof frame?.url === "string") {
          this.#url = frame.url + (frame.urlFragment ?? "");
          void this.#refreshHistory();
          this.#events.emit("did-navigate", {}, this.#url);
        }
        return;
      }
      case "Page.navigatedWithinDocument":
        if (frameId === this.targetId && typeof params.url === "string") {
          this.#url = params.url;
          void this.#refreshHistory();
          this.#events.emit("did-navigate-in-page", {}, this.#url, true);
        }
        return;
      case "Inspector.targetCrashed":
        this.markCrashed();
        return;
      default:
        return;
    }
  }

  async #refreshHistory(): Promise<void> {
    if (this.#destroyed) return;
    try {
      const history = await this.#send("Page.getNavigationHistory");
      const entries = Array.isArray(history.entries)
        ? (history.entries as Array<{ id?: number; url?: string; title?: string }>).map((entry) => ({
            id: Number(entry.id ?? 0),
            url: String(entry.url ?? ""),
            title: String(entry.title ?? "")
          }))
        : [];
      const index = typeof history.currentIndex === "number" ? history.currentIndex : entries.length - 1;
      this.#history = { entries, index };
      const current = entries[index];
      if (current !== undefined) {
        this.#url = current.url;
        if (current.title !== "") this.#title = current.title;
      }
    } catch {
      // 目标正在关闭时读取历史失败属正常，保留上一份缓存。
    }
  }

  #goToOffset(offset: number): void {
    const entry = this.#history.entries[this.#history.index + offset];
    if (entry === undefined) return;
    void this.#send("Page.navigateToHistoryEntry", { entryId: entry.id }).catch((error: unknown) =>
      log("debug", "[browser] history navigation failed", error)
    );
  }

  #resolveLoads(): void {
    for (const waiter of Array.from(this.#loadWaiters)) waiter.resolve();
  }

  #rejectLoads(error: Error): void {
    for (const waiter of Array.from(this.#loadWaiters)) waiter.reject(error);
  }

  #assertAlive(): void {
    if (this.#destroyed) throw new Error("Object has been destroyed");
  }

  #send(method: string, params: CdpParams = {}): Promise<CdpParams> {
    return this.#deps.connection.send(method, params, this.#internalSession);
  }
}

/** Renames a finished `allowAndName` download (saved as its GUID) to its suggested file name. */
export async function finalizeDownloadFile(downloadDir: string, guid: string, suggested: string): Promise<string> {
  const safeName = path.basename(suggested || guid).replace(/[<>:"/\\|?*\u0000-\u001f]/gu, "_") || guid;
  const source = path.join(downloadDir, guid);
  const parsed = path.parse(safeName);
  for (let attempt = 0; attempt < 100; attempt += 1) {
    const candidate = path.join(downloadDir, attempt === 0 ? safeName : `${parsed.name} (${attempt})${parsed.ext}`);
    try {
      await rename(source, candidate);
      return candidate;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "EEXIST" && (error as NodeJS.ErrnoException).code !== "EPERM") throw error;
    }
  }
  return source;
}
