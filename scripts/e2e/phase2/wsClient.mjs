import { GateError, fail, withTimeout } from "./gate.mjs";

const CONNECT_TIMEOUT_MS = 10_000;
const HELLO_TIMEOUT_MS = 10_000;
const OPEN = 1;

function isFlowState(value) {
  return value === "saturated" || value === "drained";
}

/**
 * Opens a socket; resolves `{ ws }` on upgrade or `{ statusCode }` when the
 * server answers the upgrade with a plain HTTP status.
 */
export function connectSocket(WebSocket, url, origin, step) {
  const attempt = new Promise((resolve, reject) => {
    const ws = new WebSocket(url, {
      origin,
      handshakeTimeout: CONNECT_TIMEOUT_MS,
      perMessageDeflate: false,
      maxPayload: 64 * 1024 * 1024,
    });
    ws.once("open", () => resolve({ ws }));
    ws.once("unexpected-response", (_request, response) => {
      response.resume();
      resolve({ statusCode: response.statusCode ?? 0 });
    });
    ws.once("error", (error) => reject(new GateError(step, `websocket connection failed: ${error.message}`)));
  });
  return withTimeout(attempt, CONNECT_TIMEOUT_MS, step, "websocket upgrade");
}

/**
 * `@zcode/rpc` MessagePortLike over an attached socket, mirroring the
 * renderer adapter: inbound frames are buffered until `start()`.
 */
function createClientPort(ws, step) {
  const listeners = new Set();
  const buffered = [];
  let started = false;
  let attached = false;
  let settled = false;
  let settle;

  const ready = new Promise((resolve, reject) => {
    const timer = setTimeout(() => settle(new GateError(step, "timed out waiting for the attached frame")), HELLO_TIMEOUT_MS);
    settle = (error) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      if (error) {
        reject(error);
      } else {
        attached = true;
        resolve();
      }
    };
  });

  const dispatch = (payload) => {
    for (const listener of listeners) listener({ data: payload });
  };

  // Like MessagePort.start(), queued frames are delivered asynchronously so a
  // ChannelClient built right after MessagePortProtocol still sees Initialize.
  const drain = () => {
    while (buffered.length > 0) dispatch(buffered.shift());
  };

  const deliver = (payload) => {
    if (!started || buffered.length > 0) {
      buffered.push(payload);
      return;
    }
    dispatch(payload);
  };

  ws.on("message", (data, isBinary) => {
    if (isBinary) {
      if (attached) deliver(new Uint8Array(data));
      return;
    }
    let frame;
    try {
      frame = JSON.parse(data.toString("utf8"));
    } catch {
      return;
    }
    if (!attached) {
      if (frame.t === "attached") settle();
      if (frame.t === "error") settle(new GateError(step, `hello rejected (${String(frame.code)}): ${String(frame.message)}`));
      return;
    }
    if (frame.t === "flow" && isFlowState(frame.state)) {
      deliver({ __zcodeRpcControl: "connection-flow-v1", state: frame.state });
    }
  });
  ws.on("close", (code) => settle(new GateError(step, `websocket closed before attached (code ${code})`)));
  ws.on("error", (error) => settle(new GateError(step, `websocket error before attached: ${error.message}`)));

  return {
    ready,
    ws,
    addEventListener(type, listener) {
      if (type === "message") listeners.add(listener);
    },
    removeEventListener(type, listener) {
      if (type === "message") listeners.delete(listener);
    },
    postMessage(message) {
      if (ws.readyState !== OPEN) return;
      if (ArrayBuffer.isView(message)) {
        ws.send(new Uint8Array(message.buffer, message.byteOffset, message.byteLength));
        return;
      }
      if (message?.__zcodeRpcControl === "connection-flow-v1" && isFlowState(message.state)) {
        ws.send(JSON.stringify({ t: "flow", state: message.state }));
      }
    },
    start() {
      if (started) return;
      started = true;
      queueMicrotask(drain);
    },
    close() {
      if (ws.readyState === OPEN) ws.close(1000);
    },
  };
}

/** Connects with the allowed origin, sends the hello, and waits for `attached`. */
export async function attachSocket(WebSocket, url, origin, grant, step) {
  const connection = await connectSocket(WebSocket, url, origin, step);
  if (connection.statusCode !== undefined) {
    fail(step, `websocket upgrade was rejected with HTTP ${connection.statusCode}`);
  }
  const port = createClientPort(connection.ws, step);
  connection.ws.send(JSON.stringify({ t: "hello", attachmentId: grant.attachmentId, token: grant.token }));
  await port.ready;
  return port;
}

/** Sends a hello and resolves with the server's close code. */
export async function helloCloseCode(ws, hello, step) {
  const closed = new Promise((resolve) => ws.once("close", (code) => resolve(code)));
  ws.send(JSON.stringify({ t: "hello", ...hello }));
  return withTimeout(closed, HELLO_TIMEOUT_MS, step, "close after a rejected hello");
}
