import { createServer } from "node:http";
import { WebSocket, WebSocketServer } from "ws";
import { createAttachmentRegistry, type AttachmentRegistrationInput, type AttachmentRegistry } from "./attachments.ts";
import { log } from "./log.ts";
import type { AttachmentCloseReason, SidecarErrorCode } from "./protocol.ts";
import { createVirtualPort, type MessagePortLike, type RpcFlowState, type VirtualPort } from "./virtualPort.ts";
import { isAttachRequest, parseHello, parseTextRecord, readFlowRecord, rejectUpgrade, toBytes } from "./wsFrames.ts";
import { isOriginAllowed, resolveServerOptions, type AttachmentServerOptions, type ResolvedServerOptions } from "./wsOptions.ts";

export { HOST_WS_PATH } from "./wsFrames.ts";
export type { AttachmentServerOptions } from "./wsOptions.ts";

const CLOSE_AUTH_FAILED = 4401;
const CLOSE_UNKNOWN_ATTACHMENT = 4404;
const CLOSE_HELLO_TIMEOUT = 4408;
const CLOSE_BACKPRESSURE = 1013;

export type AttachedHandler = (attachmentId: string, port: MessagePortLike) => void;
export type ClosedHandler = (attachmentId: string, reason: AttachmentCloseReason) => void;
/** Text frame other than hello/flow on an attached socket (Phase 4 remote-workspace control channel). */
export type TextFrameHandler = (attachmentId: string, frame: Record<string, unknown>) => void;

export interface AttachmentServer {
  readonly port: number;
  registerPending(input: AttachmentRegistrationInput): void;
  readonly onAttached: (handler: AttachedHandler) => void;
  readonly onClosed: (handler: ClosedHandler) => void;
  /** Phase 4: receives text frames that are neither hello nor flow control. */
  readonly onTextFrame: (handler: TextFrameHandler) => void;
  /** Phase 4: sends a JSON text frame on an attached socket; false when it is not open. */
  sendText(attachmentId: string, frame: Record<string, unknown>): boolean;
  close(): Promise<void>;
}

interface Session {
  attachmentId: string | null;
  port: VirtualPort | null;
  closeReason: AttachmentCloseReason;
}

interface ConnectionContext {
  options: ResolvedServerOptions;
  registry: AttachmentRegistry;
  sessions: Map<WebSocket, Session>;
  emitAttached: AttachedHandler;
  emitClosed: ClosedHandler;
  emitText: TextFrameHandler;
  socketsByAttachment: Map<string, WebSocket>;
}

function closeCodeFor(code: SidecarErrorCode): number {
  return code === "attach_unknown" ? CLOSE_UNKNOWN_ATTACHMENT : CLOSE_AUTH_FAILED;
}

function fanOut<TArgs extends unknown[]>(handlers: Set<(...args: TArgs) => void>, label: string, ...args: TArgs): void {
  // Snapshot so a handler that (un)registers handlers cannot affect this dispatch.
  for (const handler of Array.from(handlers)) {
    try {
      handler(...args);
    } catch (error) {
      log("error", `${label} handler threw`, error);
    }
  }
}

/** Wires one accepted socket: hello admission, frame mapping, and close reporting. */
function handleConnection(client: WebSocket, context: ConnectionContext): void {
  const { options, registry, sessions } = context;
  const session: Session = { attachmentId: null, port: null, closeReason: "ws-closed" };
  sessions.set(client, session);

  const closeWith = (code: number, reason: string): void => {
    if (client.readyState === WebSocket.OPEN || client.readyState === WebSocket.CONNECTING) {
      client.close(code, reason);
    }
  };

  const fail = (code: SidecarErrorCode, message: string, closeCode: number): void => {
    clearTimeout(helloTimer);
    if (client.readyState === WebSocket.OPEN) {
      client.send(JSON.stringify({ t: "error", code, message }));
    }
    closeWith(closeCode, code);
  };

  const helloTimer = setTimeout(() => {
    fail("attach_rejected", "hello timeout", CLOSE_HELLO_TIMEOUT);
  }, options.helloTimeoutMs);
  helloTimer.unref();

  const sendBinary = (bytes: Uint8Array): void => {
    if (client.readyState !== WebSocket.OPEN) {
      log("debug", "dropping binary frame for a closed websocket");
      return;
    }
    if (client.bufferedAmount + bytes.byteLength > options.maxBufferedBytes) {
      session.closeReason = "backpressure";
      log("warn", "websocket outbound buffer exceeded; closing with 1013");
      client.close(CLOSE_BACKPRESSURE, "backpressure");
      return;
    }
    client.send(bytes, { binary: true });
  };

  const sendControl = (frame: { t: "flow"; state: RpcFlowState }): void => {
    if (client.readyState === WebSocket.OPEN) client.send(JSON.stringify(frame));
  };

  const admit = (data: WebSocket.RawData, isBinary: boolean): void => {
    const hello = isBinary ? null : parseHello(data);
    if (hello === null) {
      fail("attach_rejected", isBinary ? "first frame must be a hello" : "invalid hello frame", CLOSE_AUTH_FAILED);
      return;
    }
    const claim = registry.claim(hello);
    if (!claim.ok) {
      fail(claim.code, `attachment rejected: ${claim.code}`, closeCodeFor(claim.code));
      return;
    }
    clearTimeout(helloTimer);
    session.attachmentId = claim.attachmentId;
    session.port = createVirtualPort({ sendBinary, sendControl, closeSocket: closeWith });
    context.socketsByAttachment.set(claim.attachmentId, client);
    client.send(JSON.stringify({ t: "attached" }));
    context.emitAttached(claim.attachmentId, session.port);
  };

  client.on("message", (data, isBinary) => {
    if (session.attachmentId === null) {
      admit(data, isBinary);
      return;
    }
    if (session.port === null) return;
    if (isBinary) {
      session.port.receive(toBytes(data));
      return;
    }
    const record = parseTextRecord(data);
    if (record === null) return;
    const state = readFlowRecord(record);
    if (state !== null) {
      session.port.receive({ __zcodeRpcControl: "connection-flow-v1", state });
      return;
    }
    if (record.t !== "flow") context.emitText(session.attachmentId, record);
  });

  client.on("close", () => {
    clearTimeout(helloTimer);
    if (!sessions.delete(client)) return;
    // Virtual port close first, so the Host disposes the attachment before Rust hears about it.
    session.port?.notifyClosed();
    if (session.attachmentId !== null) {
      if (context.socketsByAttachment.get(session.attachmentId) === client) {
        context.socketsByAttachment.delete(session.attachmentId);
      }
      registry.release(session.attachmentId);
      context.emitClosed(session.attachmentId, session.closeReason);
    }
  });

  client.on("error", (error) => {
    log("warn", "websocket connection error", error);
  });
}

function listenLoopback(server: ReturnType<typeof createServer>): Promise<number> {
  return new Promise<number>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      server.off("error", reject);
      const address = server.address();
      if (address === null || typeof address === "string") {
        reject(new Error("attachment server failed to bind a loopback port"));
        return;
      }
      resolve(address.port);
    });
  });
}

/** Starts the loopback attachment server on `127.0.0.1:0`. */
export async function createAttachmentServer(input: AttachmentServerOptions = {}): Promise<AttachmentServer> {
  const options = resolveServerOptions(input);
  const attachedHandlers = new Set<AttachedHandler>();
  const closedHandlers = new Set<ClosedHandler>();
  const textHandlers = new Set<TextFrameHandler>();
  const socketsByAttachment = new Map<string, WebSocket>();
  const sessions = new Map<WebSocket, Session>();
  const emitClosed: ClosedHandler = (id, reason) => fanOut(closedHandlers, "attach.closed", id, reason);
  const emitAttached: AttachedHandler = (id, port) => fanOut(attachedHandlers, "attach", id, port);
  const emitText: TextFrameHandler = (id, frame) => fanOut(textHandlers, "text frame", id, frame);
  const registry = createAttachmentRegistry({ onExpired: (id) => emitClosed(id, "expired") });

  const httpServer = createServer((_request, response) => {
    response.statusCode = 404;
    response.end();
  });
  const wss = new WebSocketServer({ noServer: true, maxPayload: options.maxPayloadBytes });

  httpServer.on("upgrade", (request, socket, head) => {
    if (!isAttachRequest(request)) {
      rejectUpgrade(socket, 404);
      return;
    }
    if (!isOriginAllowed(request.headers.origin, options)) {
      log("warn", "rejected websocket upgrade with a disallowed origin");
      rejectUpgrade(socket, 403);
      return;
    }
    wss.handleUpgrade(request, socket, head, (client) => {
      handleConnection(client, { options, registry, sessions, emitAttached, emitClosed, emitText, socketsByAttachment });
    });
  });

  const port = await listenLoopback(httpServer);

  const close = async (): Promise<void> => {
    registry.close();
    socketsByAttachment.clear();
    for (const [client, session] of sessions) {
      sessions.delete(client);
      session.port?.notifyClosed();
      if (session.attachmentId !== null) emitClosed(session.attachmentId, "shutdown");
      client.terminate();
    }
    await new Promise<void>((resolve) => wss.close(() => resolve()));
    await new Promise<void>((resolve) => httpServer.close(() => resolve()));
  };

  return {
    port,
    registerPending: (pending) => registry.register(pending),
    onAttached: (handler) => attachedHandlers.add(handler),
    onClosed: (handler) => closedHandlers.add(handler),
    onTextFrame: (handler) => textHandlers.add(handler),
    sendText: (attachmentId, frame) => {
      const socket = socketsByAttachment.get(attachmentId);
      if (socket === undefined || socket.readyState !== WebSocket.OPEN) return false;
      socket.send(JSON.stringify(frame));
      return true;
    },
    close
  };
}
