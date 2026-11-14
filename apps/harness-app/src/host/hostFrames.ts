import type { MessagePortFlowState } from "@zcode/rpc";

/** Text control frames the sidecar sends on an attachment socket. */
export type ServerFrame =
  | { t: "attached" }
  | { t: "flow"; state: MessagePortFlowState }
  | { t: "error"; code: string; message: string }
  /** Phase 4 remote-workspace control frames (`rw.res` / `rw.evt`), passed through unparsed. */
  | { t: "control"; frame: Record<string, unknown> };

/** Text control frames the renderer sends; the hello carries the token and is never logged. */
export type ClientFrame =
  | { t: "hello"; attachmentId: string; token: string }
  | { t: "flow"; state: MessagePortFlowState }
  | { t: "rw.req"; id: string; method: string; params?: unknown };

/** Parses a sidecar text frame; unknown or malformed frames yield null. */
export function parseServerFrame(text: string): ServerFrame | null {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    return null;
  }
  if (typeof value !== "object" || value === null) {
    return null;
  }
  const record = value as Record<string, unknown>;
  if (record.t === "attached") {
    return { t: "attached" };
  }
  if (record.t === "flow") {
    const state = record.state;
    return state === "saturated" || state === "drained" ? { t: "flow", state } : null;
  }
  if (record.t === "rw.res" || record.t === "rw.evt") {
    return { t: "control", frame: record };
  }
  if (record.t === "error") {
    return {
      t: "error",
      code: typeof record.code === "string" ? record.code : "internal",
      message: typeof record.message === "string" ? record.message : "",
    };
  }
  return null;
}

/** Returns exactly the view's bytes as an ArrayBuffer, copying only when the view is partial. */
export function toSendable(frame: Uint8Array): ArrayBuffer {
  const { buffer, byteOffset, byteLength } = frame;
  if (buffer instanceof ArrayBuffer && byteOffset === 0 && byteLength === buffer.byteLength) {
    return buffer;
  }
  const copy = new Uint8Array(byteLength);
  copy.set(frame);
  return copy.buffer;
}

/** Error surfaced when the attachment closes; `name` matches the contract's `ConnectionClosed`. */
export function connectionClosedError(message: string): Error {
  const error = new Error(message);
  error.name = "ConnectionClosed";
  return error;
}
