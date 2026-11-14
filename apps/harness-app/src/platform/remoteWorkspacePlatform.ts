import {
  unwrapRemoteWorkspaceBindResult,
  type BindRemoteWorkspaceSessionContextIpcResult,
  type IPlatformService,
  type RemoteConnectionRuntimeLog,
  type RemoteSessionClosedEvent,
  type RemoteSessionReconnectedEvent,
} from "@zcode/shared";
import { remoteWorkspaceBridge } from "../host/remoteWorkspaces.js";

type RemoteWorkspacePlatform = Pick<
  IPlatformService,
  | "onRemoteConnectionLog"
  | "onRemoteSessionClosed"
  | "onRemoteSessionReconnected"
  | "connectRemote"
  | "cancelPendingRemoteConnection"
  | "bindRemoteWorkspaceSessionContext"
  | "disposeRemoteSession"
  | "isDockerAvailable"
  | "listWSLDistros"
  | "listDockerContainers"
  | "listSSHConfigAliases"
>;

type ConnectRemoteResult = Awaited<ReturnType<IPlatformService["connectRemote"]>>;

function describeError(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * Desktop remote-workspace methods (Electron preload `connectRemote` & co.), carried over the
 * window attachment to the sidecar's remote session manager. Result shapes are the ZCode IPC ones.
 */
export function createRemoteWorkspacePlatform(): RemoteWorkspacePlatform {
  const bridge = remoteWorkspaceBridge;
  return {
    onRemoteConnectionLog: (handler) =>
      bridge.onEvent("remote-connection-log", (payload) =>
        handler(payload as RemoteConnectionRuntimeLog),
      ),
    onRemoteSessionClosed: (handler) =>
      bridge.onEvent("remote-session-closed", (payload) =>
        handler(payload as RemoteSessionClosedEvent),
      ),
    onRemoteSessionReconnected: (handler) =>
      bridge.onEvent("remote-session-reconnected", (payload) =>
        handler(payload as RemoteSessionReconnectedEvent),
      ),
    connectRemote: async (target, requestId, context) => {
      try {
        return (await bridge.request("connectRemote", {
          target,
          requestId,
          ...context,
        })) as ConnectRemoteResult;
      } catch (error) {
        // 传输层失败（Host 未附着等）按 ZCode IPC 的失败形状返回，界面走既有的连接失败分支。
        return { success: false, error: describeError(error) };
      }
    },
    cancelPendingRemoteConnection: async (requestId) => {
      await bridge.request("cancelPendingRemoteConnection", { requestId });
    },
    bindRemoteWorkspaceSessionContext: async (context) =>
      // 结构化结果在页面一侧还原为 Error，保留 code/retryable（与 ZCode desktop 平台一致）。
      unwrapRemoteWorkspaceBindResult(
        (await bridge.request(
          "bindRemoteWorkspaceSessionContext",
          context,
        )) as BindRemoteWorkspaceSessionContextIpcResult,
      ),
    disposeRemoteSession: async (sessionId) => {
      await bridge.request("disposeRemoteSession", { sessionId });
    },
    // 探测类接口与 ZCode 一致永不抛错：探测失败按“不可用 / 空列表”处理。
    isDockerAvailable: async () =>
      (await discover("isDockerAvailable", false as unknown)) === true,
    listWSLDistros: async () =>
      (await discover("listWSLDistros", [])) as Awaited<
        ReturnType<IPlatformService["listWSLDistros"]>
      >,
    listDockerContainers: async () =>
      (await discover("listDockerContainers", [])) as Awaited<
        ReturnType<IPlatformService["listDockerContainers"]>
      >,
    listSSHConfigAliases: async () =>
      (await discover("listSSHConfigAliases", [])) as Awaited<
        ReturnType<IPlatformService["listSSHConfigAliases"]>
      >,
  };
}

async function discover(method: string, fallback: unknown): Promise<unknown> {
  try {
    return (await remoteWorkspaceBridge.request(method)) ?? fallback;
  } catch {
    return fallback;
  }
}
