import type { IncomingMessage } from "node:http";
import type { Duplex } from "node:stream";
import type { RawData } from "ws";
import type { RpcFlowState } from "./virtualPort.ts";

export const HOST_WS_PATH = "/v1/attach";

export interface HelloFrame {
  attachmentId: string;
  token: string;
}

/** True only for a GET upgrade on the attach path (query string ignored). */
export function isAttachRequest(request: IncomingMessage): boolean {
  if (request.method !== "GET") return false;
  const rawUrl = request.url ?? "";
  const queryIndex = rawUrl.indexOf("?");
  const pathname = queryIndex === -1 ? rawUrl : rawUrl.slice(0, queryIndex);
  return pathname === HOST_WS_PATH;
}

/** Answers a refused upgrade with a bare HTTP status and drops the socket. */
export function rejectUpgrade(socket: Duplex, status: 403 | 404): void {
  const label = status === 404 ? "Not Found" : "Forbidden";
  try {
    socket.write(`HTTP/1.1 ${status} ${label}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n`);
  } catch {
    // The peer may already be gone; destroying the socket below is enough.
  }
  socket.destroy();
}

export function toBytes(data: RawData): Uint8Array {
  if (Buffer.isBuffer(data)) return new Uint8Array(data);
  if (Array.isArray(data)) return new Uint8Array(Buffer.concat(data));
  return new Uint8Array(data);
}

function toText(data: RawData): string {
  if (Buffer.isBuffer(data)) return data.toString("utf8");
  if (Array.isArray(data)) return Buffer.concat(data).toString("utf8");
  return Buffer.from(data).toString("utf8");
}

/** Parses a JSON object text frame; null for anything else. */
export function parseTextRecord(data: RawData): Record<string, unknown> | null {
  let value: unknown;
  try {
    value = JSON.parse(toText(data));
  } catch {
    return null;
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) return null;
  return value as Record<string, unknown>;
}

/** Parses the mandatory first client frame; null means the frame is invalid. */
export function parseHello(data: RawData): HelloFrame | null {
  const record = parseTextRecord(data);
  if (record === null || record.t !== "hello") return null;
  const { attachmentId, token } = record;
  if (typeof attachmentId !== "string" || attachmentId.length === 0) return null;
  if (typeof token !== "string" || token.length === 0) return null;
  return { attachmentId, token };
}

/** Reads a `{"t":"flow"}` control frame; other text frames are ignored. */
export function readFlowState(data: RawData): RpcFlowState | null {
  const record = parseTextRecord(data);
  return record === null ? null : readFlowRecord(record);
}

/** Flow state of an already parsed text frame; null unless it is a valid flow frame. */
export function readFlowRecord(record: Record<string, unknown>): RpcFlowState | null {
  if (record.t !== "flow") return null;
  return record.state === "saturated" || record.state === "drained" ? record.state : null;
}
