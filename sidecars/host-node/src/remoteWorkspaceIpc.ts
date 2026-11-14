// Renderer request handlers for remote workspaces: a port of the ZCode Main IPC handlers in
// `packages/desktop/src/main/desktopMainIpcRemote.ts` (ConnectRemote, CancelPendingRemoteConnection,
// BindRemoteWorkspaceSessionContext, DisposeRemoteSession, discovery, ScopedServicePortReady).
// Payload parsing and result shapes are kept identical; telemetry is not reported in Harness.
// The connect target may carry credentials, so payloads are never logged.
import { log } from "./log.ts";
import type { Json, RemoteWorkspaceSessionManager, RemoteWorkspaceVendor } from "./remoteWorkspaceVendor.ts";

export const REMOTE_WORKSPACE_METHODS = [
  "connectRemote",
  "cancelPendingRemoteConnection",
  "bindRemoteWorkspaceSessionContext",
  "disposeRemoteSession",
  "isDockerAvailable",
  "listWSLDistros",
  "listDockerContainers",
  "listSSHConfigAliases",
  "scopedPortReady"
] as const;

export type RemoteWorkspaceMethod = (typeof REMOTE_WORKSPACE_METHODS)[number];

export function isRemoteWorkspaceMethod(value: unknown): value is RemoteWorkspaceMethod {
  return typeof value === "string" && (REMOTE_WORKSPACE_METHODS as readonly string[]).includes(value);
}

export interface RemoteWorkspaceIpcContext {
  vendor: RemoteWorkspaceVendor;
  manager: RemoteWorkspaceSessionManager;
  window: unknown;
  webContentsId: number;
  /** Phase 4B web remote manager (filled once remote control starts); see spec section 9. */
  webRemoteControlManagerRef?: {
    current: { failRemoteSession(sessionId: string, reason: string, failure: unknown): void } | null;
  };
}

function asRecord(value: unknown): Json {
  return value !== null && typeof value === "object" && !Array.isArray(value) ? (value as Json) : {};
}

function trimmedString(value: unknown): string | undefined {
  return typeof value === "string" && value.trim().length > 0 ? value.trim() : undefined;
}

/** Non-blank string kept verbatim (ZCode keeps workspacePath/workspaceIdentity untrimmed on connect). */
function nonBlankString(value: unknown): string | undefined {
  return typeof value === "string" && value.trim().length > 0 ? value : undefined;
}

function readRetryable(error: unknown): boolean | undefined {
  if (error === null || typeof error !== "object" || !("retryable" in error)) return undefined;
  const retryable = (error as { retryable?: unknown }).retryable;
  return typeof retryable === "boolean" ? retryable : undefined;
}

function readErrorField(error: unknown, field: string): unknown {
  return error !== null && typeof error === "object" && field in error ? (error as Json)[field] : undefined;
}

function describeError(vendor: RemoteWorkspaceVendor, error: unknown): Json {
  const normalized = vendor.normalizeUnknownError(error);
  return {
    name: error instanceof Error ? error.name : undefined,
    message: normalized.message,
    code: normalized.code,
    exitCode: readErrorField(error, "exitCode")
  };
}

async function connectRemote(context: RemoteWorkspaceIpcContext, rawPayload: unknown): Promise<Json> {
  const { vendor, manager } = context;
  const wrapped = asRecord(rawPayload);
  const result = vendor.remoteTargetSchema.safeParse("target" in wrapped ? wrapped.target : rawPayload);
  if (!result.success) {
    const error = `Invalid connect-remote payload: ${vendor.formatZodError(result.error)}`;
    log("warn", "[connect-remote] invalid payload");
    return { success: false, error };
  }
  const requestId = trimmedString(wrapped.requestId);
  const workspacePath = nonBlankString(wrapped.workspacePath);
  const workspaceIdentity = nonBlankString(wrapped.workspaceIdentity);
  const confirmSourceServerInstall = wrapped.confirmSourceServerInstall === true;
  try {
    let reusedConnectionTarget: Json | undefined;
    const sessionId = await manager.createRemoteWorkspaceSession(
      context.window,
      result.data,
      requestId,
      workspacePath !== undefined || confirmSourceServerInstall
        ? {
            workspacePath,
            workspaceIdentity,
            ...(confirmSourceServerInstall ? { confirmSourceServerInstall: true } : {})
          }
        : undefined,
      {
        remoteUsageTelemetryEligible: true,
        onConnected: (descriptor) => {
          const reused = descriptor.reusedConnectionTarget;
          reusedConnectionTarget = reused !== null && typeof reused === "object" ? (reused as Json) : undefined;
        }
      }
    );
    log("info", `[connect-remote] connected kind=${result.data.kind} requestId=${requestId ?? "-"}`);
    return { success: true, sessionId, ...(reusedConnectionTarget ? { reusedConnectionTarget } : {}) };
  } catch (error) {
    const normalized = vendor.normalizeRemoteConnectionError(error);
    log("error", "[connect-remote] failed", {
      requestId,
      kind: result.data.kind,
      message: normalized.message,
      code: normalized.code
    });
    const reasonCode = vendor.isPublicZCodeErrorCode(normalized.code) ? normalized.code : undefined;
    const retryable = reasonCode !== undefined ? readRetryable(error) : undefined;
    const installOrUpgrade =
      reasonCode === "source-server-upgrade-required" || reasonCode === "source-server-install-required";
    const shape = vendor.hostRemoteWorkspaceConnectFailedResponseSchema.shape;
    const profileId = shape.profileId.safeParse(installOrUpgrade ? readErrorField(error, "profileId") : undefined);
    const resolvedWslTarget = shape.resolvedWslTarget.safeParse(
      installOrUpgrade ? readErrorField(error, "resolvedWslTarget") : undefined
    );
    return {
      success: false,
      error: normalized.message,
      ...(reasonCode !== undefined ? { reasonCode } : {}),
      ...(retryable !== undefined ? { retryable } : {}),
      ...(profileId.success && profileId.data ? { profileId: profileId.data } : {}),
      ...(resolvedWslTarget.success && resolvedWslTarget.data ? { resolvedWslTarget: resolvedWslTarget.data } : {})
    };
  }
}

function toBindFailure(vendor: RemoteWorkspaceVendor, error: unknown): Json {
  const normalized = vendor.normalizeUnknownError(error);
  const reasonCode = vendor.isPublicZCodeErrorCode(normalized.code) ? normalized.code : undefined;
  const retryable = reasonCode !== undefined ? readRetryable(error) : undefined;
  return {
    success: false,
    error: normalized.message,
    ...(reasonCode !== undefined ? { reasonCode } : {}),
    ...(retryable !== undefined ? { retryable } : {})
  };
}

async function bindContext(context: RemoteWorkspaceIpcContext, rawPayload: unknown): Promise<Json> {
  try {
    if (rawPayload === null || typeof rawPayload !== "object") {
      throw new Error("远程 workspace context payload 无效");
    }
    const payload = rawPayload as Json;
    const remoteSessionId = typeof payload.remoteSessionId === "string" ? payload.remoteSessionId.trim() : "";
    const workspacePath = typeof payload.workspacePath === "string" ? payload.workspacePath : "";
    const workspaceIdentity = typeof payload.workspaceIdentity === "string" ? payload.workspaceIdentity.trim() : "";
    const workspacePurpose =
      payload.workspacePurpose === "project" || payload.workspacePurpose === "conversation"
        ? payload.workspacePurpose
        : undefined;
    if (remoteSessionId === "" || workspacePath.trim() === "") {
      throw new Error("远程 workspace context 缺少 sessionId 或 workspacePath");
    }
    const result = await context.manager.bindRemoteWorkspaceSessionContext(
      remoteSessionId,
      {
        workspacePath,
        ...(workspaceIdentity !== "" ? { workspaceIdentity } : {}),
        ...(workspacePurpose !== undefined ? { workspacePurpose } : {})
      },
      context.webContentsId
    );
    return { success: true, result };
  } catch (error) {
    return toBindFailure(context.vendor, error);
  }
}

async function discover<T>(
  context: RemoteWorkspaceIpcContext,
  label: string,
  run: () => Promise<T>,
  fallback: T
): Promise<T> {
  try {
    return await run();
  } catch (error) {
    log("warn", `[${label}] detect failed:`, describeError(context.vendor, error));
    return fallback;
  }
}

/** Handles one renderer request; the result is what the ZCode IPC handler would return. */
export async function handleRemoteWorkspaceRequest(
  context: RemoteWorkspaceIpcContext,
  method: RemoteWorkspaceMethod,
  params: unknown
): Promise<unknown> {
  const { vendor, manager, webContentsId } = context;
  switch (method) {
    case "connectRemote":
      return connectRemote(context, params);
    case "cancelPendingRemoteConnection": {
      const requestId = trimmedString(asRecord(params).requestId);
      manager.cancelPendingRemoteWorkspaceSessionsForWindow(
        webContentsId,
        `cancel-pending-remote-connection:${webContentsId}`,
        requestId
      );
      return null;
    }
    case "bindRemoteWorkspaceSessionContext":
      return bindContext(context, params);
    case "disposeRemoteSession": {
      const sessionId = asRecord(params).sessionId;
      if (typeof sessionId === "string") {
        manager.disposeRemoteWorkspaceSession(sessionId, `dispose-remote-session:${sessionId}`, 150);
        // 修复说明：桌面主动断开远程会话时，ZCode 只在“不可恢复的连接关闭”路径通知手机；主动 dispose 后
        // Host 已摘除该会话的 attachment，手机 bridge 会一直等一个已关闭的端口。这里按同一失败语义
        // （workspace-closed）立即让手机 bridge 失败并释放其 attachment（fail closed），不影响自动重连的
        // replaceRemoteSession 路径。
        context.webRemoteControlManagerRef?.current?.failRemoteSession(sessionId, "harness:desktop-disposed", {
          reason: "workspace-closed",
          message: "远程工作区连接已结束，请在桌面端重新连接。"
        });
      }
      return null;
    }
    case "isDockerAvailable":
      return discover(context, "is-docker-available", () => vendor.isDockerAvailable(), false);
    case "listWSLDistros":
      return discover(context, "list-wsl-distros", () => vendor.listWSLDistros(), []);
    case "listDockerContainers":
      return discover(context, "list-docker-containers", () => vendor.listDockerContainers(), []);
    case "listSSHConfigAliases":
      return discover(context, "list-ssh-config-aliases", () => vendor.listSSHConfigAliasesFromLocalConfig(), []);
    case "scopedPortReady": {
      const payload = asRecord(params);
      const sessionId = trimmedString(payload.sessionId);
      const attachmentId = trimmedString(payload.attachmentId);
      if (sessionId !== undefined && attachmentId !== undefined) {
        manager.confirmRendererAttachmentReady(webContentsId, { sessionId, attachmentId });
      }
      return null;
    }
  }
}
