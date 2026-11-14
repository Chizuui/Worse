import { log } from "./log.ts";
import type { ParentPortShim } from "./parentPort.ts";
import type { AttachOpenLine, AttachmentMessageType, SidecarLine } from "./protocol.ts";
import type { MessagePortLike } from "./virtualPort.ts";
import type { AttachmentServer } from "./wsServer.ts";

const DEFAULT_ATTACHMENT_TTL_MS = 30_000;

interface PendingAttachment {
  hostMessage: Record<string, unknown>;
  messageType: AttachmentMessageType;
}

export interface AttachBroker {
  open(line: AttachOpenLine): void;
}

/**
 * Optional hook that may wrap an attachment's port before the Host sees it
 * (Phase 3a: the Rust service bridge). Returning the port unchanged is a no-op.
 */
export type PortDecorator = (port: MessagePortLike, hostMessage: Record<string, unknown>) => MessagePortLike;

/**
 * Phase 4 (remote workspaces) hooks. Attachments the sidecar mints itself (remote-scoped
 * renderer ports) are owned elsewhere and never reported to Rust; Rust grants may get their Host
 * message completed (e.g. `remoteAssets` on `init-local`) and are observed once dispatched.
 */
export interface AttachBrokerHooks {
  ownsAttachment?(attachmentId: string): boolean;
  prepareHostMessage?(hostMessage: Record<string, unknown>): Record<string, unknown>;
  onDispatched?(attachmentId: string, hostMessage: Record<string, unknown>): void;
}

function resolveMessageType(hostMessage: Record<string, unknown>): AttachmentMessageType {
  return hostMessage.type === "init-local" ? "init-local" : "attach-service-port";
}

/**
 * Joins Rust `attach.open` grants with WebSocket hellos.
 *
 * The Host message is dispatched only after the hello succeeds, with the
 * attachment's virtual port injected as `ports[0]`. Tokens stay inside the
 * server registry and are never logged.
 */
export function createAttachBroker(
  server: AttachmentServer,
  shim: ParentPortShim,
  emit: (line: SidecarLine) => void,
  decoratePort: PortDecorator = (port) => port,
  hooks: AttachBrokerHooks = {}
): AttachBroker {
  const pending = new Map<string, PendingAttachment>();
  const ownsExternally = (attachmentId: string): boolean =>
    !pending.has(attachmentId) && hooks.ownsAttachment?.(attachmentId) === true;

  server.onAttached((attachmentId, port) => {
    if (ownsExternally(attachmentId)) return;
    const entry = pending.get(attachmentId);
    if (entry === undefined) {
      log("error", `attachment ${attachmentId} opened without a pending host message`);
      port.close();
      return;
    }
    pending.delete(attachmentId);
    const hostMessage = hooks.prepareHostMessage?.(entry.hostMessage) ?? entry.hostMessage;
    shim.emitHostMessage(hostMessage, [decoratePort(port, hostMessage)]);
    hooks.onDispatched?.(attachmentId, hostMessage);
    emit({ v: 1, t: "attach.active", attachmentId, messageType: entry.messageType });
  });

  server.onClosed((attachmentId, reason) => {
    if (ownsExternally(attachmentId)) return;
    pending.delete(attachmentId);
    emit({ v: 1, t: "attach.closed", attachmentId, reason });
  });

  const open = (line: AttachOpenLine): void => {
    const { requestId, attachmentId } = line;
    if (pending.has(attachmentId)) {
      emit({
        v: 1,
        t: "attach.failed",
        requestId,
        code: "attach_duplicate",
        message: `attachment already pending: ${attachmentId}`
      });
      return;
    }
    pending.set(attachmentId, {
      hostMessage: line.hostMessage,
      messageType: resolveMessageType(line.hostMessage)
    });
    server.registerPending({
      attachmentId,
      token: line.token,
      ttlMs: line.ttlMs > 0 ? line.ttlMs : DEFAULT_ATTACHMENT_TTL_MS
    });
    emit({ v: 1, t: "attach.opened", requestId, attachmentId });
  };

  return { open };
}
