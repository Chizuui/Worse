// Renderer side of the Phase 4 remote-workspace control channel (spec phase-4-remote-workspaces.md
// section 5): `rw.req` / `rw.res` / `rw.evt` JSON text frames on an attached WebSocket.
import { GateError, withTimeout } from "../phase2/gate.mjs";

const OPEN = 1;

export function createControlChannel(ws) {
  let nextId = 0;
  const pending = new Map();
  const eventListeners = new Set();
  const events = [];

  ws.on("message", (data, isBinary) => {
    if (isBinary) return;
    let frame;
    try {
      frame = JSON.parse(data.toString("utf8"));
    } catch {
      return;
    }
    if (frame?.t === "rw.res") {
      const entry = pending.get(frame.id);
      if (!entry) return;
      pending.delete(frame.id);
      if (frame.ok === true) entry.resolve(frame.result);
      else entry.reject(Object.assign(new Error(String(frame.error?.message ?? "rw.req failed")), { code: frame.error?.code }));
      return;
    }
    if (frame?.t === "rw.evt") {
      events.push(frame);
      for (const listener of Array.from(eventListeners)) listener(frame);
    }
  });
  ws.on("close", () => {
    for (const entry of pending.values()) entry.reject(new Error("control socket closed"));
    pending.clear();
  });

  return {
    events,
    request(method, params, timeoutMs, step) {
      if (ws.readyState !== OPEN) return Promise.reject(new GateError(step, "control socket is not open"));
      nextId += 1;
      const id = `rw-${nextId}`;
      const result = new Promise((resolve, reject) => pending.set(id, { resolve, reject }));
      ws.send(JSON.stringify({ t: "rw.req", id, method, params }));
      return withTimeout(result, timeoutMs, step, `rw.req ${method}`);
    },
    onEvent(listener) {
      eventListeners.add(listener);
      return () => eventListeners.delete(listener);
    },
  };
}
