// Phase 4: remote workspaces (SSH / WSL / Docker) in the Harness sidecar.
//
// The sidecar takes Electron Main's role for its single window: it runs ZCode's remote session
// manager (vendored unchanged), forwards the manager's Host messages through the parentPort shim,
// and serves the renderer's remote-workspace requests over the base attachment's WebSocket as
// `rw.req` / `rw.res` / `rw.evt` text frames. Scoped service ports become sidecar-minted WebSocket
// attachments that never pass through Rust, so remote file/git/terminal calls stay on the TS remote
// backends. See docs/specs/harness/phase-4-remote-workspaces.md.
import { randomBytes } from "node:crypto";
import { log } from "./log.ts";
import type { AttachBrokerHooks } from "./attachBroker.ts";
import type { ParentPortShim } from "./parentPort.ts";
import { resolveRemoteAssetDirs } from "./remoteWorkspaceAssets.ts";
import { createLinkedPortPair, pipePorts, type LinkedPort } from "./remoteWorkspaceChannel.ts";
import { handleRemoteWorkspaceRequest, isRemoteWorkspaceMethod } from "./remoteWorkspaceIpc.ts";
import {
  loadRemoteWorkspaceVendor,
  type Json,
  type ProviderProvisioningLaneStatus,
  type RemoteAssetDirs,
  type RemoteWorkspaceSessionManager,
  type RemoteWorkspaceVendor
} from "./remoteWorkspaceVendor.ts";
import type { AttachmentServer } from "./wsServer.ts";

/** Electron `webContents.id` / `BrowserWindow.id` of the one window this sidecar serves. */
export const HARNESS_WINDOW_ID = 1;
const SCOPED_GRANT_TTL_MS = 30_000;
const SCOPED_SERVICE_PORT_CHANNEL = "zcode:scoped-service-port";
const CHANNEL_PREFIX = "zcode:";

/** Host responses consumed by the remote session manager (Electron Main), never forwarded to Rust. */
const MANAGER_RESPONSE_TYPES = new Set([
  "remote-workspace-connection-log",
  "remote-workspace-connected",
  "remote-workspace-connect-failed",
  "remote-workspace-context-bound",
  "remote-workspace-context-bind-failed",
  "remote-workspace-reconnected",
  "remote-workspace-closed",
  "project-workspace-prepared",
  "provider-provisioning-source-changed",
  "provider-provisioning-execution-result"
]);
const LANE_REQUEST_TYPE = "provider-provisioning-lane-request";
const LANE_RESULT_TYPE = "provider-provisioning-lane-result";

type ChildMessageListener = (message: unknown) => void;

type AnyListener = (...args: unknown[]) => void;

/** The Electron `utilityProcess` surface the manager uses for the window Local Host. */
interface HostChildAdapter {
  readonly pid: number;
  postMessage(message: unknown, ports?: unknown[]): void;
  on(event: string, listener: AnyListener): HostChildAdapter;
  once(event: string, listener: AnyListener): HostChildAdapter;
  removeListener(event: string, listener: AnyListener): HostChildAdapter;
  off(event: string, listener: AnyListener): HostChildAdapter;
}

interface ScopedGrant {
  port1: LinkedPort;
  claimed: boolean;
}

export interface WebRemoteControlManagerRef {
  current: {
    failRemoteSession(sessionId: string, reason: string, failure: unknown): void;
    replaceRemoteSession?(event: Json): Promise<void>;
  } | null;
}

export interface RemoteWorkspaces {
  /** Hooks for `createAttachBroker` (scoped attachments, `init-local` remote assets, base tracking). */
  readonly brokerHooks: AttachBrokerHooks;
  /** Wires the control channel and scoped attachments to the WebSocket server. */
  attach(server: AttachmentServer): void;
  /** Settles once the vendored manager is loaded (or failed to load; remote features then fail closed). */
  readonly ready: Promise<void>;
  /** Manager for Phase 4B (web remote control); null until `ready` or when loading failed. */
  manager(): RemoteWorkspaceSessionManager | null;
  readonly webRemoteControlManagerRef: WebRemoteControlManagerRef;
  /** Phone project navigation (Main's `remoteSessionManager.prepareProjectWorkspace(win, …)`). */
  prepareProjectWorkspace(project: Json, requestId: string): Promise<unknown>;
  /** Main's `cancelProjectPreparation`: cancels a pending phone navigation of this window. */
  cancelProjectPreparation(requestId: string, reason: string): void;
  /** Gate-only counters (no targets, paths or credentials). */
  stats(): RemoteWorkspaceStats;
  dispose(): Promise<void>;
}

export interface RemoteWorkspaceStats {
  /** `connect-remote-workspace` messages posted to the Host. */
  connectRequests: number;
  /** `remote-workspace-connected` responses from the Host. */
  connectedEvents: number;
  /** Remote-scoped `attach-service-port` messages posted to the Host, by clientMode. */
  remoteScopedAttachRequests: Record<string, number>;
  /** The vendored manager's own route view (`getRemoteConnectionStats`), null before `ready`. */
  manager: { activeSessionCount: number; activeTargetCount: number } | null;
}

function asRecord(value: unknown): Json | null {
  return value !== null && typeof value === "object" && !Array.isArray(value) ? (value as Json) : null;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** Base (window) attachments are the local desktop-continuous ones Rust grants to the renderer. */
function isBaseHostMessage(hostMessage: Json): boolean {
  if (hostMessage.type === "init-local") return true;
  if (hostMessage.type !== "attach-service-port") return false;
  const scope = asRecord(hostMessage.scope);
  const clientMode = hostMessage.clientMode;
  return (scope === null || scope.kind === "local") && (clientMode === undefined || clientMode === "desktop-continuous");
}

function defaultLocale(): string {
  return Intl.DateTimeFormat().resolvedOptions().locale || "en";
}

function managerLogger(): { info(...args: unknown[]): void; warn(...args: unknown[]): void; error(...args: unknown[]): void } {
  const write =
    (level: "info" | "warn" | "error") =>
    (message: unknown, ...rest: unknown[]): void =>
      log(level, `[remote-workspace] ${String(message)}`, rest.length === 0 ? undefined : rest.length === 1 ? rest[0] : rest);
  return { info: write("info"), warn: write("warn"), error: write("error") };
}

export function installRemoteWorkspaces(input: {
  shim: ParentPortShim;
  env: NodeJS.ProcessEnv;
  cwd: string;
}): RemoteWorkspaces {
  const { shim, env, cwd } = input;
  const childListeners = new Set<ChildMessageListener>();
  const exitListeners = new Set<() => void>();
  const scopedGrants = new Map<string, ScopedGrant>();
  const baseAttachments = new Set<string>();
  const webRemoteControlManagerRef: WebRemoteControlManagerRef = { current: null };
  let currentBase: string | null = null;
  let hadBase = false;
  let locale = defaultLocale();
  let server: AttachmentServer | null = null;
  let vendor: RemoteWorkspaceVendor | null = null;
  let manager: RemoteWorkspaceSessionManager | null = null;
  let unregisterWindow: (() => void) | null = null;
  let disposed = false;
  const counters = { connectRequests: 0, connectedEvents: 0, remoteScopedAttachRequests: {} as Record<string, number> };

  /** Counts Main→Host remote messages for the gate's single-connection assertions. */
  const countHostBound = (message: unknown): void => {
    const record = asRecord(message);
    if (record === null) return;
    if (record.type === "connect-remote-workspace") counters.connectRequests += 1;
    if (record.type === "attach-service-port" && asRecord(record.scope)?.kind === "remote") {
      const mode = typeof record.clientMode === "string" ? record.clientMode : "unknown";
      counters.remoteScopedAttachRequests[mode] = (counters.remoteScopedAttachRequests[mode] ?? 0) + 1;
    }
  };

  const sendEvent = (event: string, payload: unknown): boolean => {
    // 与 Electron 刷新期间相同：没有可用的 base attachment 时事件被丢弃，由会话 owner 的超时/重附着收口。
    if (server === null || currentBase === null) return false;
    return server.sendText(currentBase, { t: "rw.evt", event, payload });
  };

  const deliverScopedPort = (payload: unknown, ports: unknown[] | undefined): void => {
    const port1 = ports?.[0] as LinkedPort | undefined;
    const record = asRecord(payload);
    const attachmentId = typeof record?.attachmentId === "string" ? record.attachmentId : "";
    if (port1 === undefined || attachmentId === "" || server === null || currentBase === null) {
      log("warn", "[remote-workspace] scoped service port dropped: no renderer attachment");
      port1?.close();
      return;
    }
    const token = randomBytes(32).toString("hex");
    scopedGrants.set(attachmentId, { port1, claimed: false });
    server.registerPending({ attachmentId, token, ttlMs: SCOPED_GRANT_TTL_MS });
    if (!sendEvent("scoped-service-port", { payload, token })) {
      scopedGrants.delete(attachmentId);
      port1.close();
    }
  };

  const webContents = {
    id: HARNESS_WINDOW_ID,
    isDestroyed: (): boolean => false,
    send(channel: string, payload: unknown): void {
      sendEvent(channel.startsWith(CHANNEL_PREFIX) ? channel.slice(CHANNEL_PREFIX.length) : channel, payload);
    },
    postMessage(channel: string, payload: unknown, ports?: unknown[]): void {
      if (channel === SCOPED_SERVICE_PORT_CHANNEL) {
        deliverScopedPort(payload, ports);
        return;
      }
      sendEvent(channel.startsWith(CHANNEL_PREFIX) ? channel.slice(CHANNEL_PREFIX.length) : channel, payload);
    }
  };
  const win = { id: HARNESS_WINDOW_ID, webContents, isDestroyed: (): boolean => false };

  // The window's Local Host as the manager sees it (Electron `utilityProcess`).
  const child: HostChildAdapter = {
    pid: process.pid,
    postMessage(message: unknown, ports?: unknown[]): void {
      countHostBound(message);
      shim.emitHostMessage(message, ports ?? []);
    },
    on(event: string, listener: AnyListener): HostChildAdapter {
      if (event === "message") childListeners.add(listener as ChildMessageListener);
      else if (event === "exit") exitListeners.add(listener as () => void);
      return child;
    },
    once(event: string, listener: AnyListener): HostChildAdapter {
      if (event === "message") {
        const wrapper: ChildMessageListener = (message) => {
          childListeners.delete(wrapper);
          listener(message);
        };
        childListeners.add(wrapper);
      } else if (event === "exit") {
        const wrapper = (): void => {
          exitListeners.delete(wrapper);
          listener();
        };
        exitListeners.add(wrapper);
      }
      return child;
    },
    removeListener(event: string, listener: AnyListener): HostChildAdapter {
      if (event === "message") childListeners.delete(listener as ChildMessageListener);
      else if (event === "exit") exitListeners.delete(listener as () => void);
      return child;
    },
    off(event: string, listener: AnyListener): HostChildAdapter {
      return child.removeListener(event, listener);
    }
  };

  const answerLaneRequest = (message: Json): void => {
    const requestId = message.requestId;
    const reply = (status: ProviderProvisioningLaneStatus): void =>
      shim.emitHostMessage({ type: LANE_RESULT_TYPE, requestId, status }, []);
    const sourceServerId = typeof message.sourceServerId === "string" ? message.sourceServerId : "";
    if (manager === null || sourceServerId === "") {
      reply({ lane: "none" });
      return;
    }
    const pending =
      message.action === "retry"
        ? manager.retryProviderProvisioning(sourceServerId)
        : manager.getProviderProvisioningLaneStatus(sourceServerId, { waitForIdle: message.action === "wait" });
    // 与 desktopHostProcess 一致：查询/重试失败也必须应答，避免 Host 一直等待 lane 结果。
    void pending.then(reply, (error: unknown) => {
      log("warn", "[provider-provisioning] lane request failed", { action: message.action, error: errorMessage(error) });
      reply({ lane: "none" });
    });
  };

  // Host → Main: every Host message reaches the manager's listener (as on Electron's utilityProcess);
  // the ones only Main consumes are kept away from Rust, which would otherwise answer lane requests too.
  const forwardToShell = shim.postMessage;
  shim.postMessage = (message: unknown): void => {
    for (const listener of Array.from(childListeners)) {
      try {
        listener(message);
      } catch (error) {
        log("error", "[remote-workspace] host message listener threw", error);
      }
    }
    const record = asRecord(message);
    const type = record?.type;
    if (type === "remote-workspace-connected") counters.connectedEvents += 1;
    if (record !== null && type === LANE_REQUEST_TYPE) {
      answerLaneRequest(record);
      return;
    }
    if (typeof type === "string" && MANAGER_RESPONSE_TYPES.has(type)) return;
    forwardToShell(message);
  };

  const currentAssets = (): RemoteAssetDirs =>
    vendor === null ? {} : resolveRemoteAssetDirs(vendor, { env, cwd, locale });

  const ready = loadRemoteWorkspaceVendor().then(
    (loaded) => {
      if (disposed) return;
      vendor = loaded;
      unregisterWindow = loaded.registerStubWindow(win);
      manager = loaded.createRemoteWorkspaceSessionManager({
        logger: managerLogger(),
        windowHostProcessMap: new Map([[HARNESS_WINDOW_ID, child]]),
        resolveRemoteAssetDirs: currentAssets,
        resolveWslTarget: loaded.createCanonicalWslTargetResolver(),
        createMessageChannel: createLinkedPortPair,
        webRemoteControlManagerRef
      });
      log("info", `[remote-workspace] manager ready (version ${loaded.ZCODE_VERSION}, env ${loaded.ZCODE_ENV})`);
    },
    (error: unknown) => {
      // 远程能力加载失败时保持本地 Host 可用；远程请求按失败关闭（fail closed）返回错误。
      log("error", "[remote-workspace] failed to load the remote session manager", error);
    }
  );

  const respond = (attachmentId: string, id: unknown, outcome: { ok: true; result: unknown } | { ok: false; error: Json }): void => {
    server?.sendText(attachmentId, { t: "rw.res", id, ...outcome });
  };

  const handleTextFrame = (attachmentId: string, frame: Json): void => {
    if (frame.t !== "rw.req") return;
    const id = frame.id;
    if (!baseAttachments.has(attachmentId)) {
      respond(attachmentId, id, { ok: false, error: { message: "remote workspace requests require the window attachment" } });
      return;
    }
    const method = frame.method;
    if (!isRemoteWorkspaceMethod(method)) {
      respond(attachmentId, id, { ok: false, error: { message: `unknown remote workspace method: ${String(method)}` } });
      return;
    }
    if (vendor === null || manager === null) {
      respond(attachmentId, id, {
        ok: false,
        error: { message: "remote workspaces are unavailable in this Host", code: "REMOTE_WORKSPACE_UNAVAILABLE" }
      });
      return;
    }
    const context = { vendor, manager, window: win, webContentsId: HARNESS_WINDOW_ID, webRemoteControlManagerRef };
    void handleRemoteWorkspaceRequest(context, method, frame.params).then(
      (result) => respond(attachmentId, id, { ok: true, result: result ?? null }),
      (error: unknown) => {
        log("warn", `[remote-workspace] ${method} failed: ${errorMessage(error)}`);
        respond(attachmentId, id, { ok: false, error: { message: errorMessage(error) } });
      }
    );
  };

  const brokerHooks: AttachBrokerHooks = {
    ownsAttachment: (attachmentId) => scopedGrants.has(attachmentId),
    prepareHostMessage: (hostMessage) => {
      if (hostMessage.type !== "init-local") return hostMessage;
      if (typeof hostMessage.locale === "string" && hostMessage.locale.trim() !== "") locale = hostMessage.locale;
      if (hostMessage.remoteAssets !== undefined || vendor === null) return hostMessage;
      return { ...hostMessage, remoteAssets: currentAssets() };
    },
    onDispatched: (attachmentId, hostMessage) => {
      if (!isBaseHostMessage(hostMessage)) return;
      baseAttachments.add(attachmentId);
      currentBase = attachmentId;
      if (hadBase && manager !== null) {
        // 渲染进程刷新后重新附着：与 desktopWindowLifecycle 的 renderer-reload 一致，为已连接的远程会话重新下发 scoped port。
        manager.reattachRemoteWorkspaceSessionsForWindow(win, "harness:renderer-reload");
      }
      hadBase = true;
    }
  };

  const attach = (target: AttachmentServer): void => {
    server = target;
    target.onTextFrame(handleTextFrame);
    target.onAttached((attachmentId, port) => {
      const grant = scopedGrants.get(attachmentId);
      if (grant === undefined) return;
      grant.claimed = true;
      pipePorts(port, grant.port1);
    });
    target.onClosed((attachmentId) => {
      const grant = scopedGrants.get(attachmentId);
      if (grant !== undefined) {
        scopedGrants.delete(attachmentId);
        // 未被认领（过期）的 grant 由这里关闭 port1；已认领的由 pipePorts 传播关闭。
        if (!grant.claimed) grant.port1.close();
        return;
      }
      if (baseAttachments.delete(attachmentId) && currentBase === attachmentId) {
        currentBase = Array.from(baseAttachments).at(-1) ?? null;
      }
    });
  };

  const dispose = async (): Promise<void> => {
    if (disposed) return;
    disposed = true;
    const active = manager;
    try {
      if (active !== null) await active.disposeAllAndWaitForAppShutdown("harness:app-shutdown");
    } catch (error) {
      log("warn", "[remote-workspace] shutdown dispose failed", error);
    }
    for (const listener of Array.from(exitListeners)) {
      try {
        listener();
      } catch (error) {
        log("warn", "[remote-workspace] exit listener threw", error);
      }
    }
    for (const grant of scopedGrants.values()) grant.port1.close();
    scopedGrants.clear();
    unregisterWindow?.();
  };

  const prepareProjectWorkspace = (project: Json, requestId: string): Promise<unknown> => {
    // 与 Main 的 prepareProjectWorkspace 依赖一致：窗口即本 sidecar 唯一窗口；管理器未加载时失败关闭。
    if (manager === null) return Promise.reject(new Error("remote workspaces are unavailable in this Host"));
    return manager.prepareProjectWorkspace(win, project, requestId);
  };

  const cancelProjectPreparation = (requestId: string, reason: string): void => {
    manager?.cancelPendingRemoteWorkspaceSessionsForWindow(HARNESS_WINDOW_ID, reason, requestId);
  };

  const stats = (): RemoteWorkspaceStats => {
    const routeStats = manager?.getRemoteConnectionStats();
    return {
      connectRequests: counters.connectRequests,
      connectedEvents: counters.connectedEvents,
      remoteScopedAttachRequests: { ...counters.remoteScopedAttachRequests },
      manager:
        routeStats === undefined
          ? null
          : { activeSessionCount: routeStats.activeSessionCount, activeTargetCount: routeStats.activeTargetCount }
    };
  };

  return {
    brokerHooks,
    attach,
    ready,
    manager: () => manager,
    webRemoteControlManagerRef,
    prepareProjectWorkspace,
    cancelProjectPreparation,
    stats,
    dispose
  };
}
