// In-process stand-in for Electron's MessageChannelMain, used by the vendored ZCode remote session
// manager (`createMessageChannel` option). `port2` is handed to the Host with a remote-scoped
// `attach-service-port`; `port1` is piped to the renderer's scoped WebSocket attachment (or handed
// to a Phase 4B caller). Payloads are the RPC frames (Uint8Array) and `connection-flow-v1`
// control records, exactly what the Host exchanges with a WebSocket virtual port.
import { log } from "./log.ts";
import type { MessagePortLike } from "./virtualPort.ts";

type Payload = Uint8Array | Record<string, unknown>;
type MessageListener = (event: { data: unknown; ports: [] }) => void;

export interface LinkedPort extends MessagePortLike {
  on(event: "message", listener: MessageListener): void;
  on(event: "close", listener: () => void): void;
  readonly isClosed: boolean;
}

interface PortState {
  listeners: Set<MessageListener>;
  closeListeners: Set<() => void>;
  buffered: Payload[];
  started: boolean;
  closed: boolean;
}

function createState(): PortState {
  return { listeners: new Set(), closeListeners: new Set(), buffered: [], started: false, closed: false };
}

function dispatch(state: PortState, payload: Payload): void {
  const event = { data: payload, ports: [] as [] };
  for (const listener of Array.from(state.listeners)) {
    try {
      listener(event);
    } catch (error) {
      log("error", "linked port message listener threw", error);
    }
  }
}

function deliver(state: PortState, payload: Payload): void {
  if (state.closed) return;
  if (!state.started || state.buffered.length > 0) {
    state.buffered.push(payload);
    return;
  }
  dispatch(state, payload);
}

function markClosed(state: PortState): void {
  if (state.closed) return;
  state.closed = true;
  state.buffered.length = 0;
  state.listeners.clear();
  for (const listener of Array.from(state.closeListeners)) {
    state.closeListeners.delete(listener);
    try {
      listener();
    } catch (error) {
      log("error", "linked port close listener threw", error);
    }
  }
}

function createPort(own: PortState, peer: PortState): LinkedPort {
  function on(event: "message", listener: MessageListener): void;
  function on(event: "close", listener: () => void): void;
  function on(event: "message" | "close", listener: MessageListener | (() => void)): void {
    if (own.closed) return;
    if (event === "message") own.listeners.add(listener as MessageListener);
    else own.closeListeners.add(listener as () => void);
  }
  return {
    on,
    off(_event, listener) {
      own.listeners.delete(listener);
    },
    once(_event, listener) {
      if (own.closed) {
        listener();
        return;
      }
      own.closeListeners.add(listener);
    },
    postMessage(data) {
      deliver(peer, data);
    },
    start() {
      if (own.started || own.closed) return;
      own.started = true;
      // 与 MessagePort.start() 一样异步投递已排队消息：监听者通常在 start() 之后才完成订阅。
      queueMicrotask(() => {
        while (!own.closed && own.buffered.length > 0) {
          const next = own.buffered.shift();
          if (next !== undefined) dispatch(own, next);
        }
      });
    },
    close() {
      markClosed(own);
      markClosed(peer);
    },
    get isClosed() {
      return own.closed;
    }
  };
}

/** Two linked ports; closing either closes both (MessageChannelMain semantics). */
export function createLinkedPortPair(): { port1: LinkedPort; port2: LinkedPort } {
  const first = createState();
  const second = createState();
  return { port1: createPort(first, second), port2: createPort(second, first) };
}

/**
 * Pipes a WebSocket attachment port and a linked port both ways (RPC bytes and flow control).
 * Either side closing closes the other, so the Host disposes the attachment when the renderer
 * socket goes away and the socket closes when the Host detaches the port.
 */
export function pipePorts(socketPort: MessagePortLike, linked: LinkedPort): void {
  const toLinked = (event: { data: unknown }): void => {
    const data = event.data;
    if (data instanceof Uint8Array || (typeof data === "object" && data !== null)) {
      linked.postMessage(data as Payload);
    }
  };
  const toSocket = (event: { data: unknown }): void => {
    const data = event.data;
    if (data instanceof Uint8Array || (typeof data === "object" && data !== null)) {
      socketPort.postMessage(data as Payload);
    }
  };
  socketPort.on("message", toLinked);
  linked.on("message", toSocket);
  socketPort.once("close", () => linked.close());
  linked.once("close", () => socketPort.close());
  socketPort.start();
  linked.start();
}
