import { log } from "./log.ts";

export interface MessagePortLike {
  on(event: "message", listener: (event: { data: unknown; ports: [] }) => void): void;
  off(event: "message", listener: (event: { data: unknown; ports: [] }) => void): void;
  once(event: "close", listener: () => void): void;
  postMessage(data: Uint8Array | Record<string, unknown>): void;
  start(): void;
  close(): void;
}

export interface VirtualPortSink {
  sendBinary(bytes: Uint8Array): void;
  sendControl(frame: { t: "flow"; state: "saturated" | "drained" }): void;
  closeSocket(code: number, reason: string): void;
}

export type RpcFlowState = "saturated" | "drained";

export type VirtualPortPayload = Uint8Array | Record<string, unknown>;

export interface VirtualPort extends MessagePortLike {
  receive(payload: VirtualPortPayload): void;
  notifyClosed(): void;
  readonly isClosed: boolean;
}

type MessageListener = (event: { data: unknown; ports: [] }) => void;

function readFlowState(data: Record<string, unknown>): RpcFlowState | null {
  if (data.__zcodeRpcControl !== "connection-flow-v1") return null;
  const state = data.state;
  return state === "saturated" || state === "drained" ? state : null;
}

export function createVirtualPort(sink: VirtualPortSink): VirtualPort {
  const messageListeners = new Set<MessageListener>();
  const closeListeners = new Set<() => void>();
  const buffered: VirtualPortPayload[] = [];
  let started = false;
  let closed = false;

  const dispatch = (payload: VirtualPortPayload): void => {
    const event = { data: payload, ports: [] as [] };
    for (const listener of Array.from(messageListeners)) {
      try {
        listener(event);
      } catch (error) {
        log("error", "virtual port message listener threw", error);
      }
    }
  };

  const notifyClosed = (): void => {
    if (closed) return;
    closed = true;
    buffered.length = 0;
    for (const listener of Array.from(closeListeners)) {
      closeListeners.delete(listener);
      try {
        listener();
      } catch (error) {
        log("error", "virtual port close listener threw", error);
      }
    }
  };

  const on = (_event: "message", listener: MessageListener): void => {
    if (closed) return;
    messageListeners.add(listener);
  };

  const off = (_event: "message", listener: MessageListener): void => {
    messageListeners.delete(listener);
  };

  const once = (_event: "close", listener: () => void): void => {
    if (closed) {
      queueMicrotask(listener);
      return;
    }
    const wrapper = (): void => {
      closeListeners.delete(wrapper);
      listener();
    };
    closeListeners.add(wrapper);
  };

  const postMessage = (data: Uint8Array | Record<string, unknown>): void => {
    if (closed) {
      log("debug", "virtual port postMessage after close; dropped");
      return;
    }
    if (data instanceof Uint8Array) {
      sink.sendBinary(data);
      return;
    }
    const state = readFlowState(data);
    if (state !== null) {
      sink.sendControl({ t: "flow", state });
      return;
    }
    log("warn", "virtual port postMessage received an unsupported payload; dropped");
  };

  const drain = (): void => {
    while (!closed && buffered.length > 0) {
      const payload = buffered.shift();
      if (payload !== undefined) dispatch(payload);
    }
  };

  const start = (): void => {
    if (started || closed) return;
    started = true;
    // 修复说明：Electron MessagePortMain.start() 异步投递已排队消息；Host 的
    // MessagePortProtocol 在构造函数里调用 start()，其后才由 ChannelServer 订阅。
    // 同步回放会让 start() 之前到达的帧在订阅前被派发而丢失，因此用微任务保持同样语义。
    queueMicrotask(drain);
  };

  const close = (): void => {
    if (closed) return;
    notifyClosed();
    sink.closeSocket(1000, "host-closed");
  };

  const receive = (payload: VirtualPortPayload): void => {
    if (closed) return;
    // A non-empty queue means a drain is pending; queue behind it to keep frame order.
    if (!started || buffered.length > 0) {
      buffered.push(payload);
      return;
    }
    dispatch(payload);
  };

  return {
    on,
    off,
    once,
    postMessage,
    start,
    close,
    receive,
    notifyClosed,
    get isClosed() {
      return closed;
    }
  };
}
