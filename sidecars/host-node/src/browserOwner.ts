// Phase 5: Harness's browser owner — the Electron-Main + renderer half of ZCode's in-app browser,
// re-hosted in the Host sidecar.
//
// ZCode desktop: Host bridge --parentPort--> Main `runBrowserCommandOnView` -> BrowserGuestManager ->
// renderer `<webview>` guest (attachGuest) -> webContents.debugger (CDP).
// Harness:       Host bridge --shim.postMessage (intercepted here)--> the same BrowserGuestManager
// (vendored unchanged) -> a Chromium page target over CDP (browserGuest.ts) standing in for the guest.
//
// This module owns: the `browserBackendIdentity` on `init-local`, `browser-execute-request` →
// `browser-execute-result`, the Chromium process (lazy, single flight), and the renderer's job of
// materialising a guest for every tab the manager asks for. See docs/specs/harness/phase-5-browser.md.
import { mkdir } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import type { AttachBrokerHooks } from "./attachBroker.ts";
import { CdpDownloadItem, CdpGuest, finalizeDownloadFile } from "./browserGuest.ts";
import { launchChromiumWithProfileFallback, resolveBrowserExecutable, type CdpParams, type ChromiumProcess } from "./browserCdp.ts";
import {
  loadBrowserRuntimeVendor,
  type BrowserClientMode,
  type BrowserCommandLike,
  type BrowserCommandResultLike,
  type BrowserExecutionContext,
  type BrowserGuestManager,
  type BrowserRuntimeVendor,
  type BrowserViewport
} from "./browserVendor.ts";
import { log } from "./log.ts";
import type { ParentPortShim } from "./parentPort.ts";

/** The one window this sidecar serves (same constant as remoteWorkspace.ts). */
export const BROWSER_WINDOW_ID = 1;
const BROWSER_EXECUTE_REQUEST = "browser-execute-request";
const BROWSER_EXECUTE_RESULT = "browser-execute-result";
const FIRST_GUEST_ID = 1_000;

/** Commands that never need a page: answered by the manager without starting Chromium. */
const NO_LAUNCH_METHODS = new Set([
  "openScope",
  "releaseScope",
  "turnEnded",
  "closeSession",
  "cancelRequest",
  "list",
  "listUserTabs",
  "browserVisibilityGet",
  "browserVisibilitySet",
  "recordingStatus",
  "recordingCancel",
  "capabilities"
]);

export interface BrowserExecuteRequest {
  type?: string;
  requestId: string;
  browserId?: string;
  browserGeneration?: number;
  controlScopeId?: string;
  sessionId: string;
  turnId?: string;
  workspaceKey?: string;
  workspacePath?: string;
  workspaceIdentity?: string;
  remoteSessionId?: string;
  clientMode?: BrowserClientMode;
  sessionContext?: "live" | "cached";
  command: BrowserCommandLike;
}

export interface BrowserOwnerStats {
  enabled: boolean;
  disabledReason: string | null;
  executable: string | null;
  headless: boolean;
  identityInjected: boolean;
  chromiumPid: number | null;
  chromiumLaunches: number;
  guests: number;
  executed: number;
}

export interface BrowserOwner {
  /** Settles once the vendored manager is loaded (or failed to load; the browser then stays unavailable). */
  readonly ready: Promise<void>;
  readonly brokerHooks: AttachBrokerHooks;
  /** Routes the Host's `browser-execute-request` here instead of the shell. */
  install(shim: ParentPortShim): void;
  /** One request → one result, exactly what Main would answer. */
  execute(request: BrowserExecuteRequest): Promise<BrowserCommandResultLike>;
  /** Backend identity Main would put on `init-local`; null while the browser is unavailable. */
  backendIdentity(): { id: string; generation: number } | null;
  stats(): BrowserOwnerStats;
  dispose(): Promise<void>;
}

export interface BrowserOwnerOptions {
  env?: NodeJS.ProcessEnv;
  /** Gates inject the vendor module they already loaded. */
  loadVendor?: () => Promise<BrowserRuntimeVendor>;
}

function failure(code: string, message: string, startedAt: number): BrowserCommandResultLike {
  return { ok: false, error: { code, message }, elapsedMs: Date.now() - startedAt };
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** PNG IHDR width/height (bytes 16..24). */
function pngSize(base64Png: string): BrowserViewport | null {
  const bytes = Buffer.from(base64Png.slice(0, 64), "base64");
  if (bytes.length < 24 || bytes.readUInt32BE(12) !== 0x49484452) return null;
  return { width: bytes.readUInt32BE(16), height: bytes.readUInt32BE(20) };
}

/** Owns one Chromium process and the page targets presented to the manager as guests. */
class ChromiumGuestHost {
  readonly #vendor: BrowserRuntimeVendor;
  readonly #executable: string;
  readonly #profileDir: string;
  readonly #downloadDir: string;
  readonly #headless: boolean;
  readonly #env: NodeJS.ProcessEnv;
  readonly #onPopup: (guest: CdpGuest, opener: CdpGuest | undefined) => void;
  #process: ChromiumProcess | null = null;
  #starting: Promise<ChromiumProcess> | null = null;
  #spareTargets: string[] = [];
  #nextGuestId = FIRST_GUEST_ID;
  readonly #guestsByTarget = new Map<string, CdpGuest>();
  readonly #guestsBySession = new Map<string, CdpGuest>();
  readonly #unregister = new Map<number, () => void>();
  readonly #ownTargets = new Set<string>();
  readonly #downloads = new Map<string, { item: CdpDownloadItem; suggested: string }>();
  launches = 0;

  constructor(input: {
    vendor: BrowserRuntimeVendor;
    executable: string;
    profileDir: string;
    downloadDir: string;
    headless: boolean;
    env: NodeJS.ProcessEnv;
    onPopup: (guest: CdpGuest, opener: CdpGuest | undefined) => void;
  }) {
    this.#vendor = input.vendor;
    this.#executable = input.executable;
    this.#profileDir = input.profileDir;
    this.#downloadDir = input.downloadDir;
    this.#headless = input.headless;
    this.#env = input.env;
    this.#onPopup = input.onPopup;
  }

  get pid(): number | null {
    return this.#process?.pid ?? null;
  }

  get guestCount(): number {
    return this.#guestsByTarget.size;
  }

  /** Lazy, single-flight launch; a Chromium that exited is relaunched on the next request. */
  ensure(): Promise<ChromiumProcess> {
    if (this.#process !== null && !this.#process.connection.closed) return Promise.resolve(this.#process);
    this.#starting ??= this.#launch().finally(() => {
      this.#starting = null;
    });
    return this.#starting;
  }

  async #launch(): Promise<ChromiumProcess> {
    const startedAt = Date.now();
    const chromium = await launchChromiumWithProfileFallback({
      executable: this.#executable,
      profileDir: this.#profileDir,
      headless: this.#headless,
      env: this.#env
    });
    this.launches += 1;
    const connection = chromium.connection;
    connection.onEvent((method, params, sessionId) => this.#route(method, params, sessionId));
    connection.onClose((reason) => this.#handleBrowserGone(chromium, reason));
    await connection.send("Target.setDiscoverTargets", { discover: true });
    await mkdir(this.#downloadDir, { recursive: true }).catch(() => undefined);
    await connection
      .send("Browser.setDownloadBehavior", { behavior: "allowAndName", downloadPath: this.#downloadDir, eventsEnabled: true })
      .catch((error: unknown) => log("warn", `[browser] downloads unavailable: ${errorText(error)}`));
    const targets = await connection.send("Target.getTargets");
    this.#spareTargets = (Array.isArray(targets.targetInfos) ? (targets.targetInfos as Array<{ targetId?: string; type?: string }>) : [])
      .filter((info) => info.type === "page" && typeof info.targetId === "string")
      .map((info) => String(info.targetId));
    for (const targetId of this.#spareTargets) this.#ownTargets.add(targetId);
    this.#process = chromium;
    log("info", `[browser] chromium launched in ${Date.now() - startedAt}ms`);
    return chromium;
  }

  /**
   * ZCode Main resizes with `nativeImage.resize({quality:"best"})`; Harness has no native image
   * engine, so the same high-quality downsample runs in a private blank Chromium target
   * (createImageBitmap resizeQuality "high" → OffscreenCanvas → PNG).
   */
  async resizePng(base64Png: string, target: BrowserViewport): Promise<string | undefined> {
    const chromium = await this.ensure();
    const sessionId = await this.#utilitySession(chromium);
    const width = Math.max(1, Math.round(target.width));
    const height = Math.max(1, Math.round(target.height));
    const expression = `(async () => {
      const bytes = Uint8Array.from(atob(${JSON.stringify(base64Png)}), (c) => c.charCodeAt(0));
      const bitmap = await createImageBitmap(new Blob([bytes], { type: "image/png" }), { resizeWidth: ${width}, resizeHeight: ${height}, resizeQuality: "high" });
      const canvas = new OffscreenCanvas(${width}, ${height});
      canvas.getContext("2d").drawImage(bitmap, 0, 0);
      const out = new Uint8Array(await (await canvas.convertToBlob({ type: "image/png" })).arrayBuffer());
      let text = "";
      for (let i = 0; i < out.length; i += 32768) text += String.fromCharCode(...out.subarray(i, i + 32768));
      return btoa(text);
    })()`;
    const result = await chromium.connection.send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true }, sessionId);
    const value = (result.result as { value?: unknown } | undefined)?.value;
    return typeof value === "string" && value !== "" ? value : undefined;
  }

  #utility: { chromium: ChromiumProcess; session: Promise<string> } | null = null;

  #utilitySession(chromium: ChromiumProcess): Promise<string> {
    if (this.#utility?.chromium === chromium) return this.#utility.session;
    const session = (async () => {
      const created = await chromium.connection.send("Target.createTarget", { url: "about:blank", background: true });
      const targetId = String(created.targetId ?? "");
      this.#ownTargets.add(targetId);
      const attached = await chromium.connection.send("Target.attachToTarget", { targetId, flatten: true });
      return String(attached.sessionId ?? "");
    })();
    session.catch(() => {
      if (this.#utility?.session === session) this.#utility = null;
    });
    this.#utility = { chromium, session };
    return session;
  }

  /** A fresh page guest: reuses the launch's blank page first (closing it could end a headful browser). */
  async createGuest(): Promise<CdpGuest> {
    const chromium = await this.ensure();
    let targetId = this.#spareTargets.shift();
    if (targetId === undefined) {
      const created = await chromium.connection.send("Target.createTarget", { url: "about:blank" });
      targetId = String(created.targetId ?? "");
      if (targetId === "") throw new Error("Target.createTarget returned no targetId");
      this.#ownTargets.add(targetId);
    }
    return await this.#adopt(chromium, targetId);
  }

  async #adopt(chromium: ChromiumProcess, targetId: string): Promise<CdpGuest> {
    const guest = await CdpGuest.create({
      connection: chromium.connection,
      targetId,
      id: this.#nextGuestId++,
      bindSession: (sessionId, owner) => this.#guestsBySession.set(sessionId, owner),
      unbindSession: (sessionId) => this.#guestsBySession.delete(sessionId)
    });
    this.#guestsByTarget.set(targetId, guest);
    this.#unregister.set(guest.id, this.#vendor.registerGuestWebContents(guest));
    return guest;
  }

  #route(method: string, params: CdpParams, sessionId: string | undefined): void {
    if (sessionId !== undefined && sessionId !== "") {
      this.#guestsBySession.get(sessionId)?.handleSessionEvent(method, params, sessionId);
      return;
    }
    switch (method) {
      case "Target.targetCreated":
        this.#handleTargetCreated(params.targetInfo as { targetId?: string; type?: string; openerId?: string } | undefined);
        return;
      case "Target.targetInfoChanged": {
        const info = params.targetInfo as { targetId?: string; url?: unknown; title?: unknown } | undefined;
        if (typeof info?.targetId === "string") this.#guestsByTarget.get(info.targetId)?.updateTargetInfo(info);
        return;
      }
      case "Target.targetDestroyed":
        if (typeof params.targetId === "string") this.#forget(params.targetId, "target destroyed");
        return;
      case "Target.targetCrashed":
        if (typeof params.targetId === "string") this.#guestsByTarget.get(params.targetId)?.markCrashed();
        return;
      case "Browser.downloadWillBegin":
        this.#beginDownload(params);
        return;
      case "Browser.downloadProgress":
        void this.#progressDownload(params);
        return;
      default:
        return;
    }
  }

  #handleTargetCreated(info: { targetId?: string; type?: string; openerId?: string } | undefined): void {
    if (info?.type !== "page" || typeof info.targetId !== "string" || typeof info.openerId !== "string") return;
    if (this.#ownTargets.has(info.targetId) || !this.#guestsByTarget.has(info.openerId)) return;
    // ZCode：window.open/target=_blank 经 setWindowOpenHandler 交给 renderer 开成用户 tab；
    // Harness 没有 renderer 面板，直接把新 target 作为 user-origin guest 交给 manager（可被 agent 认领）。
    const chromium = this.#process;
    if (chromium === null) return;
    const opener = this.#guestsByTarget.get(info.openerId);
    this.#ownTargets.add(info.targetId);
    void this.#adopt(chromium, info.targetId)
      .then((guest) => this.#onPopup(guest, opener))
      .catch((error: unknown) => log("warn", `[browser] failed to adopt popup target: ${errorText(error)}`));
  }

  #beginDownload(params: CdpParams): void {
    const guid = typeof params.guid === "string" ? params.guid : "";
    const frameId = typeof params.frameId === "string" ? params.frameId : "";
    const guest = this.#guestsByTarget.get(frameId);
    if (guid === "" || guest === undefined) return;
    const suggested = typeof params.suggestedFilename === "string" ? params.suggestedFilename : guid;
    const item = new CdpDownloadItem(path.join(this.#downloadDir, guid));
    this.#downloads.set(guid, { item, suggested });
    guest.beginDownload(item);
  }

  async #progressDownload(params: CdpParams): Promise<void> {
    const guid = typeof params.guid === "string" ? params.guid : "";
    const entry = this.#downloads.get(guid);
    if (entry === undefined) return;
    if (params.state === "completed") {
      this.#downloads.delete(guid);
      const finalPath = await finalizeDownloadFile(this.#downloadDir, guid, entry.suggested).catch(() => undefined);
      entry.item.finish("completed", finalPath);
    } else if (params.state === "canceled") {
      this.#downloads.delete(guid);
      entry.item.finish("cancelled");
    }
  }

  #forget(targetId: string, reason: string): void {
    this.#ownTargets.delete(targetId);
    this.#spareTargets = this.#spareTargets.filter((id) => id !== targetId);
    const guest = this.#guestsByTarget.get(targetId);
    if (guest === undefined) return;
    this.#guestsByTarget.delete(targetId);
    for (const [sessionId, owner] of this.#guestsBySession) if (owner === guest) this.#guestsBySession.delete(sessionId);
    this.#unregister.get(guest.id)?.();
    this.#unregister.delete(guest.id);
    guest.markDestroyed(reason);
  }

  #handleBrowserGone(chromium: ChromiumProcess, reason: string): void {
    if (this.#process === chromium) this.#process = null;
    log("warn", `[browser] chromium connection closed: ${reason}`);
    for (const targetId of Array.from(this.#guestsByTarget.keys())) this.#forget(targetId, reason);
    for (const entry of this.#downloads.values()) entry.item.finish("interrupted");
    this.#downloads.clear();
    this.#spareTargets = [];
    this.#ownTargets.clear();
  }

  async close(): Promise<void> {
    const pending = this.#starting;
    const chromium = this.#process ?? (pending ? await pending.catch(() => null) : null);
    this.#process = null;
    if (chromium !== null) await chromium.close();
  }
}

export function createBrowserOwner(options: BrowserOwnerOptions = {}): BrowserOwner {
  const env = options.env ?? process.env;
  const headless = env.HARNESS_BROWSER_HEADFUL !== "1";
  const explicitlyDisabled = env.HARNESS_BROWSER === "off";
  const executable = explicitlyDisabled ? null : resolveBrowserExecutable(env).path;
  const home = os.homedir();
  const profileDir = env.HARNESS_BROWSER_PROFILE_DIR?.trim() || path.join(home, ".zcode", "harness", "browser-profile");
  const downloadDir = env.HARNESS_BROWSER_DOWNLOAD_DIR?.trim() || path.join(home, "Downloads");

  let vendor: BrowserRuntimeVendor | null = null;
  let manager: BrowserGuestManager | null = null;
  let chromium: ChromiumGuestHost | null = null;
  let disabledReason: string | null = explicitlyDisabled
    ? "disabled by HARNESS_BROWSER=off"
    : executable === null
      ? "no Chrome, Chromium or Edge installation was found (set HARNESS_BROWSER_EXECUTABLE)"
      : null;
  let identityInjected = false;
  let executed = 0;
  let disposed = false;
  const guestsByTab = new Map<string, CdpGuest>();

  const openTab = async (tabId: string, owner: BrowserExecutionContext): Promise<void> => {
    if (chromium === null || manager === null) return;
    const previous = guestsByTab.get(tabId);
    if (previous !== undefined && !previous.isDestroyed()) previous.close();
    const guest = await chromium.createGuest();
    guestsByTab.set(tabId, guest);
    guest.once("destroyed", () => {
      if (guestsByTab.get(tabId) === guest) guestsByTab.delete(tabId);
    });
    // 等价于 renderer 的 browserViewAttachGuest：以 owner scope（window/workspace/session）上报 guest。
    const result = manager.attachGuest(tabId, guest.id, {
      active: true,
      windowId: owner.windowId,
      workspaceKey: owner.workspaceKey,
      sessionId: owner.sessionId,
      ...(owner.remoteSessionId ? { remoteSessionId: owner.remoteSessionId } : {})
    });
    if (!result.ok) {
      log("warn", `[browser] manager rejected guest attach tab=${tabId} reason=${result.reason ?? "?"}`);
      guestsByTab.delete(tabId);
      guest.close();
    }
  };

  const adoptPopup = (guest: CdpGuest, opener: CdpGuest | undefined): void => {
    if (manager === null) return;
    const tabId = `harness-popup-${guest.id}`;
    const openerTab = Array.from(guestsByTab.entries()).find(([, candidate]) => candidate === opener)?.[0];
    const owner = openerTab === undefined ? null : manager.getTabOwner(openerTab);
    guestsByTab.set(tabId, guest);
    guest.once("destroyed", () => {
      if (guestsByTab.get(tabId) === guest) guestsByTab.delete(tabId);
    });
    // 弹窗归属 opener 的 window/workspace/session scope，与 ZCode renderer 打开的用户 tab 一致。
    const result = manager.attachGuest(tabId, guest.id, {
      active: false,
      windowId: BROWSER_WINDOW_ID,
      ...(owner ? { workspaceKey: owner.workspaceKey, sessionId: owner.sessionId } : {}),
      ...(owner?.remoteSessionId ? { remoteSessionId: owner.remoteSessionId } : {})
    });
    if (!result.ok) {
      log("warn", `[browser] popup attach rejected reason=${result.reason ?? "?"}`);
      guestsByTab.delete(tabId);
      guest.close();
    }
  };

  const ready = (async () => {
    if (disabledReason !== null) {
      log("info", `[browser] in-app browser unavailable: ${disabledReason}`);
      return;
    }
    try {
      vendor = await (options.loadVendor ?? loadBrowserRuntimeVendor)();
    } catch (error) {
      disabledReason = `browser runtime bundle failed to load: ${errorText(error)}`;
      log("error", `[browser] ${disabledReason}`);
      return;
    }
    const loaded = vendor;
    chromium = new ChromiumGuestHost({
      vendor: loaded,
      executable: executable ?? "",
      profileDir,
      downloadDir,
      headless,
      env,
      onPopup: adoptPopup
    });
    manager = new loaded.BrowserGuestManager(
      (message) => log("debug", message),
      undefined,
      (tabId) => {
        const guest = guestsByTab.get(tabId);
        guestsByTab.delete(tabId);
        if (guest !== undefined && !guest.isDestroyed()) guest.close();
      },
      (tabId, owner) => {
        void openTab(tabId, owner).catch((error: unknown) => log("warn", `[browser] failed to open tab ${tabId}: ${errorText(error)}`));
      },
      undefined,
      undefined,
      // executor 以 2x 质量倍率截图后要求降采样到 CSS px（ZCode 用 nativeImage）；尺寸已符合时直接返回。
      async (base64Png, target) => {
        const size = pngSize(base64Png);
        if (size !== null && size.width === target.width && size.height === target.height) return base64Png;
        if (chromium === null) return undefined;
        return await chromium.resizePng(base64Png, target).catch((error: unknown) => {
          log("warn", `[browser] screenshot resize failed: ${errorText(error)}`);
          return undefined;
        });
      },
      {
        // 没有宿主合成器：CDP 截图直接读取 target 的 surface，租约立即就绪。
        prepare: async (input) => ({
          surfaceScale: 1,
          webContentsId: input.webContentsId,
          viewport: input.viewport,
          release: () => undefined
        })
      },
      {
        warn: (message) => log("warn", message)
        // recording.createRecorder 依赖 Electron 隐藏窗口的 MediaRecorder：Harness 不提供（recordingStart 在 execute 前即返回 capability_unsupported）。
      }
    );
    log("info", `[browser] in-app browser owner ready executable=${executable ?? "?"} headless=${headless}`);
  })();

  const execute = async (request: BrowserExecuteRequest): Promise<BrowserCommandResultLike> => {
    const startedAt = Date.now();
    await ready;
    if (manager === null || chromium === null) {
      return failure("backend_unavailable", `Harness in-app browser is unavailable: ${disabledReason ?? "not initialized"}`, startedAt);
    }
    const method = typeof request.command?.method === "string" ? request.command.method : "?";
    if (method === "recordingStart") {
      return failure(
        "capability_unsupported",
        "Browser video recording is not available in Harness (it needs Electron's offscreen MediaRecorder); use screenshots instead.",
        startedAt
      );
    }
    if (!NO_LAUNCH_METHODS.has(method)) {
      try {
        await chromium.ensure();
      } catch (error) {
        return failure("backend_unavailable", `Harness could not start Chromium: ${errorText(error)}`, startedAt);
      }
    }
    // 与 Main runBrowserCommandOnView 相同的 workspace 身份键：workspaceIdentity?.trim() || workspaceKey || workspacePath || sessionId。
    const workspaceKey =
      request.workspaceIdentity?.trim() || request.workspaceKey || request.workspacePath || request.sessionId;
    executed += 1;
    try {
      return await manager.execute(
        {
          requestId: request.requestId,
          ...(request.controlScopeId ? { controlScopeId: request.controlScopeId } : {}),
          browserId: request.browserId ?? "legacy-iab",
          browserGeneration: request.browserGeneration ?? 0,
          windowId: BROWSER_WINDOW_ID,
          workspaceKey,
          sessionId: request.sessionId,
          ...(request.turnId ? { turnId: request.turnId } : {}),
          ...(request.remoteSessionId ? { remoteSessionId: request.remoteSessionId } : {}),
          clientMode: request.clientMode ?? "desktop-continuous"
        },
        request.command
      );
    } catch (error) {
      log("error", `[browser] execute threw method=${method}: ${errorText(error)}`);
      return failure("execution_error", errorText(error), startedAt);
    }
  };

  const backendIdentity = (): { id: string; generation: number } | null =>
    manager === null ? null : manager.getBackendIdentity(BROWSER_WINDOW_ID);

  const brokerHooks: AttachBrokerHooks = {
    prepareHostMessage: (hostMessage) => {
      if (hostMessage.type !== "init-local" || hostMessage.browserBackendIdentity !== undefined) return hostMessage;
      const identity = backendIdentity();
      if (identity === null) return hostMessage;
      if (!identityInjected) log("info", `[browser] backend identity injected into init-local id=${identity.id}`);
      identityInjected = true;
      return { ...hostMessage, browserBackendIdentity: identity };
    }
  };

  const install = (shim: ParentPortShim): void => {
    const forwardToShell = shim.postMessage;
    shim.postMessage = (message: unknown) => {
      const record = message as Partial<BrowserExecuteRequest> | null;
      if (record === null || typeof record !== "object" || record.type !== BROWSER_EXECUTE_REQUEST) {
        forwardToShell(message);
        return;
      }
      const requestId = typeof record.requestId === "string" ? record.requestId : "";
      void execute(record as BrowserExecuteRequest)
        .catch((error: unknown) => failure("execution_error", errorText(error), Date.now()))
        .then((result) => {
          if (!disposed) shim.emitHostMessage({ type: BROWSER_EXECUTE_RESULT, requestId, result }, []);
        });
    };
  };

  return {
    ready,
    brokerHooks,
    install,
    execute,
    backendIdentity,
    stats: () => ({
      enabled: manager !== null,
      disabledReason,
      executable,
      headless,
      identityInjected,
      chromiumPid: chromium?.pid ?? null,
      chromiumLaunches: chromium?.launches ?? 0,
      guests: chromium?.guestCount ?? 0,
      executed
    }),
    dispose: async () => {
      if (disposed) return;
      disposed = true;
      await ready;
      try {
        manager?.disposeAll();
      } catch (error) {
        log("warn", `[browser] manager dispose failed: ${errorText(error)}`);
      }
      await chromium?.close();
    }
  };
}

/** Composes several broker hook sets (the broker accepts one). */
export function composeBrokerHooks(...sets: AttachBrokerHooks[]): AttachBrokerHooks {
  return {
    ownsAttachment: (attachmentId) => sets.some((hooks) => hooks.ownsAttachment?.(attachmentId) === true),
    prepareHostMessage: (hostMessage) => sets.reduce((message, hooks) => hooks.prepareHostMessage?.(message) ?? message, hostMessage),
    onDispatched: (attachmentId, hostMessage) => {
      for (const hooks of sets) hooks.onDispatched?.(attachmentId, hostMessage);
    }
  };
}
