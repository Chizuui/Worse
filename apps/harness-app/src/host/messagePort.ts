import type { MessagePortLike as RpcMessagePortLike, MessagePortPayload } from "@zcode/rpc";
import {
  connectionClosedError,
  parseServerFrame,
  toSendable,
  type ClientFrame,
} from "./hostFrames.js";

/** Renderer-side Host port: the RPC transport plus the attach handshake result. */
export interface HostMessagePort extends RpcMessagePortLike {
  readonly ready: Promise<void>;
  /** Settles once with the reason the attachment ended (remote close, rejection, or `close()`). */
  readonly closed: Promise<Error>;
  /** Phase 4: sends a remote-workspace control request on this attachment (queued until attached). */
  sendControl(frame: Extract<ClientFrame, { t: "rw.req" }>): void;
  /** Phase 4: observes remote-workspace control frames (`rw.res` / `rw.evt`) from the sidecar. */
  onControl(listener: (frame: Record<string, unknown>) => void): () => void;
}

const ATTACH_TIMEOUT_MS = 10_000;

type InboundListener = (event: { data: MessagePortPayload }) => void;
type OutboundFrame = Uint8Array | ClientFrame;

/**
 * Builds the `@zcode/rpc` MessagePort adapter for one WebSocket attachment.
 *
 * Construction is synchronous so the RPC client can be created immediately;
 * inbound frames are buffered until `start()`, and outbound frames are held
 * until the sidecar acknowledges the hello with `attached`.
 */
export function createHostMessagePort(input: {
  url: string;
  token: string;
  attachmentId: string;
}): HostMessagePort {
  const socket = new WebSocket(input.url);
  socket.binaryType = "arraybuffer";

  const listeners = new Set<InboundListener>();
  const controlListeners = new Set<(frame: Record<string, unknown>) => void>();
  const inbound: MessagePortPayload[] = [];
  const outbound: OutboundFrame[] = [];

  let started = false;
  let attached = false;
  let closed = false;

  let resolveReady: () => void = () => {};
  let rejectReady: (error: Error) => void = () => {};
  const ready = new Promise<void>((resolve, reject) => {
    resolveReady = resolve;
    rejectReady = reject;
  });

  let resolveClosed: (reason: Error) => void = () => {};
  const closedReason = new Promise<Error>((resolve) => {
    resolveClosed = resolve;
  });

  const timer = setTimeout(() => {
    shutdown(connectionClosedError("Host attachment timed out"));
  }, ATTACH_TIMEOUT_MS);

  const dispatch = (payload: MessagePortPayload): void => {
    for (const listener of listeners) {
      listener({ data: payload });
    }
  };

  const drainInbound = (): void => {
    while (!closed && inbound.length > 0) {
      const payload = inbound.shift();
      if (payload !== undefined) {
        dispatch(payload);
      }
    }
  };

  const pushInbound = (payload: MessagePortPayload): void => {
    if (closed) {
      return;
    }
    // A non-empty queue means a drain is pending; queue behind it to keep frame order.
    if (started && inbound.length === 0) {
      dispatch(payload);
    } else {
      inbound.push(payload);
    }
  };

  const sendFrame = (frame: OutboundFrame): void => {
    socket.send(frame instanceof Uint8Array ? toSendable(frame) : JSON.stringify(frame));
  };

  const queueOrSend = (frame: OutboundFrame): void => {
    if (attached) {
      sendFrame(frame);
    } else {
      outbound.push(frame);
    }
  };

  function settleAttached(): void {
    if (attached || closed) {
      return;
    }
    attached = true;
    clearTimeout(timer);
    resolveReady();
    for (const frame of outbound.splice(0)) {
      sendFrame(frame);
    }
  }

  function shutdown(error: Error): void {
    if (closed) {
      return;
    }
    closed = true;
    clearTimeout(timer);
    rejectReady(error);
    resolveClosed(error);
    inbound.length = 0;
    outbound.length = 0;
    listeners.clear();
    controlListeners.clear();
    socket.close();
  }

  function handleServerText(text: string): void {
    const frame = parseServerFrame(text);
    if (frame === null) {
      return;
    }
    if (frame.t === "attached") {
      settleAttached();
    } else if (frame.t === "control") {
      for (const listener of Array.from(controlListeners)) {
        listener(frame.frame);
      }
    } else if (frame.t === "flow") {
      pushInbound({ __zcodeRpcControl: "connection-flow-v1", state: frame.state });
    } else {
      const detail = frame.message.length > 0 ? frame.message : frame.code;
      shutdown(connectionClosedError(`Host attachment rejected: ${detail}`));
    }
  }

  socket.addEventListener("open", () => {
    if (!closed) {
      socket.send(
        JSON.stringify({ t: "hello", attachmentId: input.attachmentId, token: input.token }),
      );
    }
  });

  socket.addEventListener("message", (event: MessageEvent<unknown>) => {
    if (typeof event.data === "string") {
      handleServerText(event.data);
    } else if (event.data instanceof ArrayBuffer) {
      pushInbound(new Uint8Array(event.data));
    }
  });

  socket.addEventListener("close", () => {
    shutdown(connectionClosedError("Host attachment closed"));
  });

  return {
    ready,
    closed: closedReason,
    addEventListener(_type, listener) {
      listeners.add(listener);
    },
    removeEventListener(_type, listener) {
      listeners.delete(listener);
    },
    postMessage(message) {
      if (closed) {
        return;
      }
      queueOrSend(message instanceof Uint8Array ? message : { t: "flow", state: message.state });
    },
    sendControl(frame) {
      if (!closed) {
        queueOrSend(frame);
      }
    },
    onControl(listener) {
      if (closed) {
        return () => {};
      }
      controlListeners.add(listener);
      return () => {
        controlListeners.delete(listener);
      };
    },
    start() {
      if (started || closed) {
        return;
      }
      started = true;
      // 修复说明：MessagePortProtocol 在构造函数里先挂监听再调用 start()，而 ChannelClient
      // 在其后才订阅协议消息。若此处同步回放已缓冲的帧，重载附着时 Host 早已发出的
      // Initialize 会在 ChannelClient 订阅前被派发而丢失，UI 永远等不到初始化。
      // 浏览器 MessagePort.start() 本就是异步投递排队消息，这里用微任务保持同样语义。
      queueMicrotask(drainInbound);
    },
    close() {
      shutdown(connectionClosedError("Host message port closed"));
    },
  };
}
