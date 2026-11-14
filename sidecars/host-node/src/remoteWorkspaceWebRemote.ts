// Phone (web remote control) attachments on the desktop's remote workspace sessions.
//
// Harness counterpart of the remote branch of ZCode's
// `packages/desktop/src/main/webRemoteControlSharedHostAttachments.ts`: the phone bridge for an
// SSH / WSL / Docker workspace becomes one more remote-scoped `web-remote-replayable` attachment on
// the session the desktop already owns, created by the vendored Phase 4A session manager's
// `attachRemoteWorkspaceSessionHost`. This module never connects anything: no
// `connect-remote-workspace`, no second remote session, Host or Agent. A session that is gone,
// offline, owned by another window or bound to another identity makes the manager throw, which
// the web remote manager turns into `workspace-bridge-error` (fail closed).
// See docs/specs/harness/phase-4-web-remote-control.md section 5.5.
import { log } from "./log.ts";
import type { RemoteWorkspaces } from "./remoteWorkspace.ts";
import type { LinkedPort } from "./remoteWorkspaceChannel.ts";

export interface RemoteBridgeContext {
  remoteSessionId?: string;
  workspacePath: string;
  workspaceIdentity?: string;
  terminalOwnerId?: string;
}

export interface RemoteBridgeAttachment {
  attachmentId: string;
  entryId: string;
  remoteSessionId: string;
  remoteKind: string;
  /** The in-process `port1` the web remote manager wraps (Electron `MessagePortMain` shape). */
  port: LinkedPort;
}

export interface RemoteWorkspaceWebRemoteBridge {
  /** ZCode `attachWorkspaceHost` for `kind: "remote"`; throws (fail closed) when the session cannot serve. */
  attach(windowId: number, context: RemoteBridgeContext, attachmentId: string): RemoteBridgeAttachment;
  /** Closes one phone attachment; returns false when the id is not a remote phone attachment. */
  release(attachmentId: string): boolean;
  /** ZCode `disposeRemoteSession`: closes every phone attachment of that session; returns their ids. */
  disposeRemoteSession(remoteSessionId: string): string[];
  /** Closes every phone attachment (window disposal); returns their ids. */
  disposeAll(): string[];
  /** Publishes the web remote manager to the session manager (`webRemoteControlManagerRef.current`). */
  bindWebRemoteManager(manager: unknown): void;
  prepareProjectWorkspace(project: Record<string, unknown>, requestId: string): Promise<unknown>;
  cancelProjectPreparation(requestId: string): void;
}

type SharedHostErrorCode = "REMOTE_SESSION_MISSING" | "REMOTE_WORKSPACE_IDENTITY_MISSING" | "REMOTE_WORKSPACE_UNAVAILABLE";

function sharedHostError(code: SharedHostErrorCode, message: string): Error & { code: SharedHostErrorCode } {
  return Object.assign(new Error(message), { code });
}

export function createRemoteWorkspaceWebRemoteBridge(remoteWorkspaces: RemoteWorkspaces): RemoteWorkspaceWebRemoteBridge {
  const attachments = new Map<string, { remoteSessionId: string; port: LinkedPort }>();

  const release = (attachmentId: string): boolean => {
    const entry = attachments.get(attachmentId);
    if (entry === undefined) return false;
    attachments.delete(attachmentId);
    try {
      // 关闭 port1 即关闭 Host 侧 port2（MessageChannelMain 语义），Host 随之释放该 attachment。
      entry.port.close();
    } catch (error) {
      log("warn", "[web-remote-control] failed to close remote workspace attachment", { attachmentId, error });
    }
    return true;
  };

  return {
    attach(windowId, context, attachmentId) {
      // 与 ZCode webRemoteControlSharedHostAttachments 的远程分支保持同样的前置校验与错误码。
      if (!context.remoteSessionId) {
        throw sharedHostError("REMOTE_SESSION_MISSING", "远程 workspace bridge 缺少 remoteSessionId。");
      }
      const workspaceIdentity = context.workspaceIdentity?.trim();
      if (!workspaceIdentity) {
        throw sharedHostError(
          "REMOTE_WORKSPACE_IDENTITY_MISSING",
          "远程 workspace bridge 缺少 workspaceIdentity，不能建立身份隔离。"
        );
      }
      const manager = remoteWorkspaces.manager();
      if (manager === null) {
        throw sharedHostError("REMOTE_WORKSPACE_UNAVAILABLE", "remote workspaces are unavailable in this Host");
      }
      // 会话不存在 / 离线 / 不属于本窗口 / 身份不匹配时由会话管理器抛错（fail closed），不会向 Host 发送任何消息。
      const attached = manager.attachRemoteWorkspaceSessionHost({
        windowId,
        remoteSessionId: context.remoteSessionId,
        workspacePath: context.workspacePath,
        workspaceIdentity,
        workspaceKey: workspaceIdentity,
        clientMode: "web-remote-replayable",
        ...(context.terminalOwnerId ? { terminalOwnerId: context.terminalOwnerId } : {})
      });
      const port = attached.port as LinkedPort;
      attachments.set(attachmentId, { remoteSessionId: context.remoteSessionId, port });
      // Host 摘除该 attachment（会话释放、重连换代）时同步移出登记，避免之后对已关闭端口重复释放。
      port.once("close", () => {
        if (attachments.get(attachmentId)?.port === port) attachments.delete(attachmentId);
      });
      return {
        attachmentId,
        entryId: `remote-session-host:${context.remoteSessionId}`,
        remoteSessionId: context.remoteSessionId,
        remoteKind: attached.remoteKind,
        port
      };
    },
    release,
    disposeRemoteSession(remoteSessionId) {
      const released: string[] = [];
      for (const [attachmentId, entry] of Array.from(attachments)) {
        if (entry.remoteSessionId === remoteSessionId && release(attachmentId)) released.push(attachmentId);
      }
      return released;
    },
    disposeAll() {
      const released: string[] = [];
      for (const attachmentId of Array.from(attachments.keys())) {
        if (release(attachmentId)) released.push(attachmentId);
      }
      return released;
    },
    bindWebRemoteManager(manager) {
      remoteWorkspaces.webRemoteControlManagerRef.current = manager as RemoteWorkspaces["webRemoteControlManagerRef"]["current"];
    },
    prepareProjectWorkspace(project, requestId) {
      return remoteWorkspaces.prepareProjectWorkspace(project, requestId);
    },
    cancelProjectPreparation(requestId) {
      remoteWorkspaces.cancelProjectPreparation(requestId, "mobile-navigation-superseded");
    }
  };
}
