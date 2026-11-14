// Harness wiring for ZCode web remote control.
//
// Electron Main owned `webRemoteControlManager` in ZCode. Harness has no Main process, so the same
// manager (bundled unchanged into dist-vendor/webRemoteRuntime.js) runs here, in the process that
// already hosts the window's Host. This module only supplies what Main supplied: login/settings
// readers, the device identity, an in-memory port pair for the shared Host attachment, and the
// renderer round-trips (status, reconnect, renderer actions), which travel to the Rust shell as
// `harness-web-remote-*` messages inside the existing `host` NDJSON envelope.
//
// Ownership (AGENTS.md): this layer stores no task, queue, or snapshot state. The phone attaches to
// the window's existing Host as one more `attach-service-port` (clientMode `web-remote-replayable`);
// no second Host or Agent is created. Remote (SSH/WSL/Docker) bridges reuse the desktop's existing
// remote session through `remoteWorkspaceWebRemote.ts` and fail closed when it is gone.
import { randomUUID } from "node:crypto";
import { hostname } from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { log } from "./log.ts";
import {
  createMcpTempAttachmentHandlers,
  runMcpTempAttachmentRequest,
  type McpTempAttachmentHandlers
} from "./mcpTempAttachmentHandlers.ts";
import type { RemoteTargetDiscovery } from "./remoteTargets.ts";
import type { RemoteWorkspaceWebRemoteBridge } from "./remoteWorkspaceWebRemote.ts";
import { createVirtualPort, type VirtualPort } from "./virtualPort.ts";

export const WEB_REMOTE_MESSAGE_PREFIX = "harness-web-remote-";
export const WEB_REMOTE_CALL = "harness-web-remote-call";
export const WEB_REMOTE_RESULT = "harness-web-remote-result";
export const WEB_REMOTE_EVENT = "harness-web-remote-event";

/** One Harness window owns one Host process, so the manager's window id is constant. */
const WINDOW_ID = 1;
const LEGACY_RELAY_CREDENTIAL_KEY = "web-remote-control:external-relay:pass_hash";

export type WebRemoteMethod =
  | "start"
  | "stop"
  | "getStatus"
  | "notifyAuthStateChanged"
  | "syncWorkspaces"
  | "reconnectResult"
  | "rendererActionResult"
  | "getAttachmentStats"
  | "getRemoteWorkspaceStats"
  // Desktop renderer platform requests Main used to answer (temp attachments, MCP user directory).
  | "platformRequest";

const METHODS: ReadonlySet<string> = new Set<WebRemoteMethod>([
  "start",
  "stop",
  "getStatus",
  "notifyAuthStateChanged",
  "syncWorkspaces",
  "reconnectResult",
  "rendererActionResult",
  "getAttachmentStats",
  "getRemoteWorkspaceStats",
  "platformRequest"
]);

export type WebRemoteEvent = "status" | "reconnectRequest" | "rendererActionRequest" | "attachment";

type Json = Record<string, unknown>;

interface SafeParse<T> {
  safeParse(value: unknown): { success: true; data: T } | { success: false; error: unknown };
}

interface CredentialService {
  load(key: string): Promise<string | null>;
  delete(key: string): Promise<void>;
}

interface SettingService {
  get(): Promise<Json>;
  update(patch: Json): Promise<unknown>;
}

interface WebRemoteManager {
  authorizeStart(windowId: number, context: Json): unknown;
  startAuthorized(windowId: number, context: Json, authorization: unknown): Promise<unknown>;
  stop(windowId: number): Promise<void>;
  getStatus(windowId: number): unknown;
  notifyAuthStateChanged(windowId: number, change: Json): Promise<void>;
  syncAvailableWorkspaces(windowId: number, workspaces: unknown[]): void;
  restorePreviouslyEnabled(windowId: number, workspaces: unknown[]): Promise<unknown>;
  disposeWindow(windowId: number): Promise<void>;
}

/** The subset of dist-vendor/webRemoteRuntime.js this module uses. */
export interface WebRemoteVendor {
  createWebRemoteControlManager(dependencies: Json): WebRemoteManager;
  createWebRemoteControlFeatureGate(enabled?: boolean): unknown;
  createWebRemoteControlDeviceStorage(dependencies: Json): unknown;
  createCredentialService(): CredentialService;
  createSettingService(): SettingService;
  buildZCodeEndpointUrls(origin: string, options?: { appVersion?: string }): Json & { origin: string; remoteUrl: string };
  resolveRuntimeZCodeEndpointOrigin(env: NodeJS.ProcessEnv, options?: { overrideOrigin?: string | null }): string;
  resolveWebRemoteControlRelayWsUrl(options: { endpointOrigin?: string; overrideUrl?: string }): string;
  webRemoteControlReconnectWorkspaceResultSchema: SafeParse<Json & { requestId: string; success: boolean; error?: string }>;
  webRemoteControlRendererActionResultSchema: SafeParse<Json & { requestId: string; success: boolean; error?: string }>;
  webRemoteControlWorkspaceTargetsSchema: SafeParse<unknown[]>;
  webRemoteControlAuthStateChangeSchema: SafeParse<Json>;
  WEB_REMOTE_CONTROL_WORKSPACE_RECONNECT_TIMEOUT_MS: number;
  WEB_REMOTE_CONTROL_RENDERER_ACTION_TIMEOUT_MS: number;
}

export interface WebRemoteHostDependencies {
  /** Delivers a message (and transferred ports) to the Host, as Main's `utilityProcess.postMessage`. */
  postToHost(message: Json, ports: unknown[]): void;
  /** Writes one `harness-web-remote-*` message to the Rust shell. */
  postToShell(message: Json): void;
  env: NodeJS.ProcessEnv;
  loadVendor?: () => Promise<WebRemoteVendor>;
  /** Phone bridges on the desktop's remote (SSH/WSL/Docker) sessions; absent → remote bridges fail closed. */
  remoteWorkspaces?: RemoteWorkspaceWebRemoteBridge;
  /** Docker/WSL/SSH listings from the desktop OS; absent → those phone requests fail closed. */
  remoteTargets?: RemoteTargetDiscovery;
  /** Gate-only remote session counters (`getRemoteWorkspaceStats`). */
  remoteWorkspaceStats?: () => unknown;
  /** Temp text attachment + MCP user-directory handlers (Main's); defaults to the vendored ZCode code. */
  mcpTempAttachments?: McpTempAttachmentHandlers;
}

export interface WebRemoteHost {
  /** Observes shell→Host messages; captures the device identity from `init-local`. */
  observeHostMessage(message: Json): void;
  /** Handles one `harness-web-remote-call` from the shell. Never throws. */
  handleShellMessage(message: Json): void;
  dispose(): Promise<void>;
}

/** Message counts per phone attachment; read-only diagnostics, never payload contents. */
export interface AttachmentStats {
  attachmentId: string;
  open: boolean;
  /** `remote` = remote-scoped attachment on the desktop's remote session (message counts stay 0). */
  kind?: "local" | "remote";
  remoteSessionId?: string;
  messagesToHost: number;
  messagesFromHost: number;
}

interface PendingRoundTrip {
  resolve(value: Json): void;
  reject(error: Error): void;
  timer: ReturnType<typeof setTimeout>;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function isRecord(value: unknown): value is Json {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function readString(record: Json, key: string): string | undefined {
  const value = record[key];
  return typeof value === "string" && value.trim() !== "" ? value : undefined;
}

function readOAuthUserId(raw: string | null): string {
  if (!raw) return "";
  try {
    const parsed: unknown = JSON.parse(raw);
    if (!isRecord(parsed)) return "";
    const id = typeof parsed.id === "string" ? parsed.id.trim() : "";
    const userId = typeof parsed.user_id === "string" ? parsed.user_id.trim() : "";
    return id || userId;
  } catch {
    return "";
  }
}

/**
 * Same reads as Main's `createTelemetryUserIdLoader` + `createTelemetryAuthorizationLoader`
 * (packages/services/src/node.ts): the JWT is returned only while the active provider and user id
 * stay unchanged across the reads, so a sign-out or account switch mid-read yields no login.
 */
async function loadLogin(credentials: CredentialService): Promise<{ userId: string; jwt: string } | undefined> {
  try {
    const provider = (await credentials.load("oauth:active_provider"))?.trim();
    if (provider !== "zai" && provider !== "bigmodel") return undefined;
    const readUserId = async (): Promise<string> =>
      readOAuthUserId(await credentials.load(`oauth:${provider}:user_info`));
    const userId = await readUserId();
    if (!userId) return undefined;
    const jwt = (await credentials.load("zcodejwttoken"))?.trim();
    if ((await credentials.load("oauth:active_provider"))?.trim() !== provider || (await readUserId()) !== userId) {
      return undefined;
    }
    return jwt && /^[\x21-\x7e]+$/.test(jwt) ? { userId, jwt } : undefined;
  } catch (error) {
    // 凭据解密失败与 Main 一致按未登录处理；只记录错误类别，绝不输出凭据内容。
    log("warn", "web remote control could not read the login", errorMessage(error));
    return undefined;
  }
}

async function loadBundledVendor(): Promise<WebRemoteVendor> {
  // src/ (tests) and dist/ (runtime) sit at the same depth as dist-vendor/.
  const here = path.dirname(fileURLToPath(import.meta.url));
  const bundle = path.resolve(here, "..", "dist-vendor", "webRemoteRuntime.js");
  return (await import(pathToFileURL(bundle).href)) as WebRemoteVendor;
}

export function createWebRemoteHost(dependencies: WebRemoteHostDependencies): WebRemoteHost {
  const env = dependencies.env;
  let deviceMid = env.HARNESS_DEVICE_MID?.trim() || undefined;
  let appVersion = env.HARNESS_APP_VERSION?.trim() || undefined;
  let managerPromise: Promise<{ vendor: WebRemoteVendor; manager: WebRemoteManager }> | undefined;
  const attachments = new Map<string, VirtualPort>();
  const stats = new Map<string, AttachmentStats>();
  const pendingReconnects = new Map<string, PendingRoundTrip>();
  const pendingActions = new Map<string, PendingRoundTrip>();
  let nextAttachment = 0;
  let disposed = false;
  // 手机 platform-request 与桌面渲染层 platformRequest 共用同一个 owner，避免两条写入路径。
  const mcpTempAttachments = dependencies.mcpTempAttachments ?? createMcpTempAttachmentHandlers();

  const remoteTargets = (): RemoteTargetDiscovery => {
    if (!dependencies.remoteTargets) throw new Error("Remote target discovery is unavailable in this Host.");
    return dependencies.remoteTargets;
  };

  const emitEvent = (event: WebRemoteEvent, payload: unknown): void => {
    if (disposed) return;
    dependencies.postToShell({ type: WEB_REMOTE_EVENT, event, payload });
  };

  const managerLogger = {
    info: (...args: unknown[]) => log("info", "[web-remote-control]", args),
    warn: (...args: unknown[]) => log("warn", "[web-remote-control]", args),
    error: (...args: unknown[]) => log("error", "[web-remote-control]", args)
  };

  const roundTrip = (
    pending: Map<string, PendingRoundTrip>,
    event: WebRemoteEvent,
    request: Json & { requestId: string },
    timeoutMs: number,
    timeoutMessage: string
  ): Promise<Json> =>
    new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        pending.delete(request.requestId);
        reject(new Error(timeoutMessage));
      }, timeoutMs);
      pending.set(request.requestId, { resolve, reject, timer });
      emitEvent(event, request);
    });

  const settleRoundTrip = (pending: Map<string, PendingRoundTrip>, result: Json & { requestId: string; success: boolean; error?: string }): void => {
    const entry = pending.get(result.requestId);
    if (!entry) return;
    pending.delete(result.requestId);
    clearTimeout(entry.timer);
    if (result.success) entry.resolve(result);
    else entry.reject(new Error(result.error ?? "renderer request failed"));
  };

  const markRemoteReleased = (attachmentId: string): void => {
    const entry = stats.get(attachmentId);
    if (!entry?.open) return;
    entry.open = false;
    emitEvent("attachment", { attachmentId, state: "released" });
  };

  const releaseAttachment = (attachmentId: string): void => {
    if (dependencies.remoteWorkspaces?.release(attachmentId)) {
      markRemoteReleased(attachmentId);
      return;
    }
    const port = attachments.get(attachmentId);
    if (!port) return;
    attachments.delete(attachmentId);
    const entry = stats.get(attachmentId);
    if (entry) entry.open = false;
    port.close();
    emitEvent("attachment", { attachmentId, state: "released" });
  };

  /** One in-memory channel; each side's posts are delivered to the other side's listeners. */
  const createPortPair = (entry: AttachmentStats): { hostPort: VirtualPort; managerPort: VirtualPort } => {
    const ports: { host?: VirtualPort; manager?: VirtualPort } = {};
    const sinkTo = (peer: () => VirtualPort | undefined, count: () => void) => ({
      // Electron MessagePort 投递的是结构化克隆；这里复制字节，避免发送方复用缓冲区时改写对端数据。
      sendBinary: (bytes: Uint8Array) => {
        count();
        peer()?.receive(bytes.slice());
      },
      sendControl: (frame: { t: "flow"; state: "saturated" | "drained" }) =>
        peer()?.receive({ __zcodeRpcControl: "connection-flow-v1", state: frame.state }),
      closeSocket: () => peer()?.notifyClosed()
    });
    ports.host = createVirtualPort(
      sinkTo(
        () => ports.manager,
        () => {
          entry.messagesFromHost += 1;
        }
      )
    );
    ports.manager = createVirtualPort(
      sinkTo(
        () => ports.host,
        () => {
          entry.messagesToHost += 1;
        }
      )
    );
    return { hostPort: ports.host, managerPort: ports.manager };
  };

  const createManager = async (): Promise<{ vendor: WebRemoteVendor; manager: WebRemoteManager }> => {
    const vendor = await (dependencies.loadVendor ?? loadBundledVendor)();
    if (!deviceMid) {
      throw new Error("Web remote control needs the device identity; the Host has not been initialised.");
    }
    const version = appVersion ?? "0.0.0";
    const credentials = vendor.createCredentialService();
    const settings = vendor.createSettingService();
    const relayWsOverride = env.ZCODE_WEB_REMOTE_CONTROL_RELAY_WS_URL?.trim() || undefined;
    const remoteUrlOverride = env.ZCODE_WEB_REMOTE_CONTROL_URL?.trim() || undefined;
    const getEndpointUrls = async (): Promise<Json> => {
      const override = (await settings.get()).zcodeEndpointOrigin;
      const origin = vendor.resolveRuntimeZCodeEndpointOrigin(env, {
        overrideOrigin: typeof override === "string" ? override : null
      });
      const derived = vendor.buildZCodeEndpointUrls(origin, { appVersion: version });
      return {
        ...derived,
        relayWsUrl: vendor.resolveWebRemoteControlRelayWsUrl({
          endpointOrigin: derived.origin,
          ...(relayWsOverride ? { overrideUrl: relayWsOverride } : {})
        }),
        remoteUrl: remoteUrlOverride ?? derived.remoteUrl
      };
    };
    const initial = await getEndpointUrls();

    const manager = vendor.createWebRemoteControlManager({
      getEndpointUrls,
      relayWsUrl: initial.relayWsUrl,
      mobileRemoteControlUrl: initial.remoteUrl,
      deviceMid,
      deviceName: hostname(),
      appVersion: version,
      getLogin: () => loadLogin(credentials),
      deviceStorage: vendor.createWebRemoteControlDeviceStorage({
        deleteLegacyCredential: () => credentials.delete(LEGACY_RELAY_CREDENTIAL_KEY),
        loadSettings: () => settings.get(),
        patchSettings: async (patch: Json) => {
          await settings.update(patch);
        }
      }),
      startupRestoreStorageProvider: {
        load: async () => (await settings.get()).webRemoteControlLastEnabledContext,
        save: async (context: Json) => {
          await settings.update({
            webRemoteControlLastEnabledContext: {
              workspacePath: context.workspacePath,
              workspaceIdentity: context.workspaceIdentity,
              initialTaskId: context.initialTaskId
            }
          });
        },
        clear: async () => {
          await settings.update({ webRemoteControlLastEnabledContext: undefined });
        }
      },
      featureGate: vendor.createWebRemoteControlFeatureGate(),
      logger: managerLogger,
      // 手机端平台请求：与 Main 一样在桌面侧执行；依赖缺失时明确失败（fail closed），手机端会收到 success:false 而不是悬挂。
      platformHandlers: {
        // Docker/WSL/SSH 列表与 Main 一致：在桌面系统上运行 ZCode 自己的探测代码（remoteTargets.ts）；不可用时失败关闭。
        isDockerAvailable: () => remoteTargets().isDockerAvailable(),
        listWSLDistros: () => remoteTargets().listWSLDistros(),
        listDockerContainers: () => remoteTargets().listDockerContainers(),
        listSSHConfigAliases: () => remoteTargets().listSSHConfigAliases(),
        // 修复：此前临时文本附件与 MCP 用户目录读写在 Harness 中直接返回 unavailable；
        // 现与 Main 一致运行 ZCode 自己的处理函数（mcpTempAttachmentHandlers.ts），写入相同的数据根与 ~/.zcode/cli/config.json。
        createTempTextAttachment: (payload: unknown) => mcpTempAttachments.createTempTextAttachment(payload),
        loadMcpFromUserDirectory: (payload: unknown) => mcpTempAttachments.loadMcpFromUserDirectory(payload),
        saveMcpToUserDirectory: (payload: unknown) => mcpTempAttachments.saveMcpToUserDirectory(payload),
        migrateLegacyCommonMcp: (payload: unknown) => mcpTempAttachments.migrateLegacyCommonMcp(payload)
      },
      reconnectWorkspace: async (_windowId: number, workspaceKey: string) => {
        await roundTrip(
          pendingReconnects,
          "reconnectRequest",
          { requestId: `web-remote-reconnect-${randomUUID()}`, workspaceKey },
          vendor.WEB_REMOTE_CONTROL_WORKSPACE_RECONNECT_TIMEOUT_MS,
          "Web remote control reconnect request timed out."
        );
      },
      runRendererAction: (_windowId: number, action: Json) =>
        roundTrip(
          pendingActions,
          "rendererActionRequest",
          { ...action, requestId: `web-remote-action-${randomUUID()}` },
          vendor.WEB_REMOTE_CONTROL_RENDERER_ACTION_TIMEOUT_MS,
          "Web remote control action timed out."
        ),
      onStatusChanged: (_windowId: number, status: unknown) => emitEvent("status", status),
      prepareProjectWorkspace: (_windowId: number, project: Json, requestId: string) => {
        if (!dependencies.remoteWorkspaces) return Promise.reject(new Error("Project navigation is unavailable."));
        return dependencies.remoteWorkspaces.prepareProjectWorkspace(project, requestId);
      },
      cancelProjectPreparation: (_windowId: number, requestId: string) => {
        dependencies.remoteWorkspaces?.cancelProjectPreparation(requestId);
      },
      attachWorkspaceHost: async (windowId: number, context: Json) => {
        if (context.kind !== "local") {
          // 远程 workspace：复用桌面已有的远程会话（同一 Host 上多一个 remote-scoped attachment），绝不新建连接；
          // 会话已不存在/离线/身份不匹配时由会话管理器抛错，手机收到 workspace-bridge-error（fail closed）。
          const bridge = dependencies.remoteWorkspaces;
          if (!bridge) throw new Error("Remote workspace bridges are unavailable in this Host.");
          const attachmentId = `harness-web-remote-attachment-${nextAttachment + 1}`;
          const attached = bridge.attach(
            windowId,
            {
              workspacePath: typeof context.workspacePath === "string" ? context.workspacePath : "",
              ...(readString(context, "remoteSessionId") ? { remoteSessionId: readString(context, "remoteSessionId") } : {}),
              ...(readString(context, "workspaceIdentity") ? { workspaceIdentity: readString(context, "workspaceIdentity") } : {}),
              ...(readString(context, "terminalOwnerId") ? { terminalOwnerId: readString(context, "terminalOwnerId") } : {})
            },
            attachmentId
          );
          nextAttachment += 1;
          stats.set(attachmentId, {
            attachmentId,
            open: true,
            kind: "remote",
            remoteSessionId: attached.remoteSessionId,
            messagesToHost: 0,
            messagesFromHost: 0
          });
          // Host 摘除该 attachment（会话释放/换代）时同步更新诊断状态。
          attached.port.once("close", () => markRemoteReleased(attachmentId));
          emitEvent("attachment", {
            attachmentId,
            state: "opened",
            clientMode: "web-remote-replayable",
            scope: "remote",
            remoteKind: attached.remoteKind
          });
          return {
            entryId: attached.entryId,
            attachmentId,
            process: { pid: process.pid },
            port: attached.port,
            remoteKind: attached.remoteKind
          };
        }
        nextAttachment += 1;
        const attachmentId = `harness-web-remote-attachment-${nextAttachment}`;
        const entry: AttachmentStats = { attachmentId, open: true, kind: "local", messagesToHost: 0, messagesFromHost: 0 };
        stats.set(attachmentId, entry);
        const { hostPort, managerPort } = createPortPair(entry);
        attachments.set(attachmentId, managerPort);
        const terminalOwnerId = readString(context, "terminalOwnerId");
        dependencies.postToHost(
          {
            type: "attach-service-port",
            requestId: `harness-web-remote-request-${nextAttachment}`,
            attachmentId,
            clientMode: "web-remote-replayable",
            ...(terminalOwnerId ? { terminalOwnerId } : {}),
            scope: { kind: "local" }
          },
          [hostPort]
        );
        emitEvent("attachment", { attachmentId, state: "opened", clientMode: "web-remote-replayable" });
        return {
          entryId: `harness-host:${WINDOW_ID}`,
          attachmentId,
          process: { pid: process.pid },
          port: managerPort
        };
      },
      setTerminalOwner: (_windowId: number, ownerId: string | null) => {
        dependencies.postToHost({ type: "set-terminal-owner", ownerId }, []);
      },
      releaseWorkspaceHostAttachment: releaseAttachment,
      disposeWorkspaceHostAttachmentsForWindow: () => {
        for (const attachmentId of Array.from(attachments.keys())) releaseAttachment(attachmentId);
        for (const attachmentId of dependencies.remoteWorkspaces?.disposeAll() ?? []) markRemoteReleased(attachmentId);
      },
      disposeWorkspaceHostAttachmentsForRemoteSession: (remoteSessionId: string) => {
        for (const attachmentId of dependencies.remoteWorkspaces?.disposeRemoteSession(remoteSessionId) ?? []) {
          markRemoteReleased(attachmentId);
        }
      }
    });
    // 与 ZCode index.ts 一致：会话管理器经 webRemoteControlManagerRef 通知手机（不可恢复断开、自动重连换代）。
    dependencies.remoteWorkspaces?.bindWebRemoteManager(manager);
    return { vendor, manager };
  };

  const ensureManager = (): Promise<{ vendor: WebRemoteVendor; manager: WebRemoteManager }> => {
    managerPromise ??= createManager().catch((error: unknown) => {
      managerPromise = undefined;
      throw error;
    });
    return managerPromise;
  };

  const invoke = async (method: WebRemoteMethod, params: Json): Promise<unknown> => {
    switch (method) {
      case "getAttachmentStats":
        return Array.from(stats.values(), (entry) => ({ ...entry }));
      case "getRemoteWorkspaceStats":
        return dependencies.remoteWorkspaceStats?.() ?? null;
      case "platformRequest":
        // 桌面渲染层的 Main 平台请求不依赖远控是否启动、登录或设备身份。
        return runMcpTempAttachmentRequest(mcpTempAttachments, params);
      case "getStatus":
        if (!managerPromise) return { status: "idle" };
        return (await ensureManager()).manager.getStatus(WINDOW_ID);
      case "start": {
        const context = params.context;
        if (!isRecord(context) || typeof context.workspacePath !== "string" || context.workspacePath.trim() === "") {
          throw new Error("Web remote control needs a workspace to start.");
        }
        const { manager } = await ensureManager();
        const authorization = manager.authorizeStart(WINDOW_ID, context);
        return manager.startAuthorized(WINDOW_ID, context, authorization);
      }
      case "stop":
        if (!managerPromise) return null;
        await (await ensureManager()).manager.stop(WINDOW_ID);
        return null;
      case "notifyAuthStateChanged": {
        if (!managerPromise) return null;
        const { vendor, manager } = await ensureManager();
        const parsed = vendor.webRemoteControlAuthStateChangeSchema.safeParse(params.change);
        if (!parsed.success) throw new Error("invalid auth state change");
        await manager.notifyAuthStateChanged(WINDOW_ID, parsed.data);
        return null;
      }
      case "syncWorkspaces": {
        const { vendor, manager } = await ensureManager();
        const parsed = vendor.webRemoteControlWorkspaceTargetsSchema.safeParse(params.workspaces);
        if (!parsed.success) throw new Error("invalid workspace targets");
        manager.syncAvailableWorkspaces(WINDOW_ID, parsed.data);
        void manager.restorePreviouslyEnabled(WINDOW_ID, parsed.data).catch((error: unknown) => {
          log("warn", "web remote control restore failed", errorMessage(error));
        });
        return null;
      }
      case "reconnectResult": {
        const { vendor } = await ensureManager();
        const parsed = vendor.webRemoteControlReconnectWorkspaceResultSchema.safeParse(params.result);
        if (!parsed.success) throw new Error("invalid reconnect result");
        settleRoundTrip(pendingReconnects, parsed.data);
        return null;
      }
      case "rendererActionResult": {
        const { vendor } = await ensureManager();
        const parsed = vendor.webRemoteControlRendererActionResultSchema.safeParse(params.result);
        if (!parsed.success) throw new Error("invalid renderer action result");
        settleRoundTrip(pendingActions, parsed.data);
        return null;
      }
    }
  };

  return {
    observeHostMessage(message) {
      if (message.type !== "init-local") return;
      deviceMid = readString(message, "deviceMid") ?? deviceMid;
    },
    handleShellMessage(message) {
      const id = readString(message, "id");
      const method = readString(message, "method");
      if (message.type !== WEB_REMOTE_CALL || id === undefined) {
        log("warn", "ignored a malformed web remote control message");
        return;
      }
      const reply = (result: { ok: true; value: unknown } | { ok: false; error: string }): void =>
        dependencies.postToShell({ type: WEB_REMOTE_RESULT, id, ...result });
      if (disposed) {
        reply({ ok: false, error: "Web remote control is shutting down." });
        return;
      }
      if (method === undefined || !METHODS.has(method)) {
        reply({ ok: false, error: `unknown web remote control method: ${method ?? "<missing>"}` });
        return;
      }
      appVersion = readString(message, "appVersion") ?? appVersion;
      const params = isRecord(message.params) ? message.params : {};
      invoke(method as WebRemoteMethod, params).then(
        (value) => reply({ ok: true, value: value ?? null }),
        (error: unknown) => reply({ ok: false, error: errorMessage(error) })
      );
    },
    async dispose() {
      if (disposed) return;
      for (const pending of [pendingReconnects, pendingActions]) {
        for (const entry of pending.values()) {
          clearTimeout(entry.timer);
          entry.reject(new Error("Web remote control is shutting down."));
        }
        pending.clear();
      }
      if (managerPromise) {
        try {
          await (await managerPromise).manager.disposeWindow(WINDOW_ID);
        } catch (error) {
          log("warn", "web remote control dispose failed", errorMessage(error));
        }
      }
      disposed = true;
      for (const attachmentId of Array.from(attachments.keys())) releaseAttachment(attachmentId);
      for (const attachmentId of dependencies.remoteWorkspaces?.disposeAll() ?? []) markRemoteReleased(attachmentId);
      dependencies.remoteWorkspaces?.bindWebRemoteManager(null);
    }
  };
}
