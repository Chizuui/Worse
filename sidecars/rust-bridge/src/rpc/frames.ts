import { DataType, FrameReader, FrameWriter, RpcDecodeError, decodeValue, encodeValue } from "./codec.ts";

/** `@zcode/rpc` RequestType / ResponseType values (const enums upstream). */
export const RequestType = {
  Promise: 100,
  PromiseCancel: 101,
  EventListen: 102,
  EventDispose: 103
} as const;

export const ResponseType = {
  PromiseSuccess: 201,
  PromiseError: 202,
  EventFire: 204,
  EventError: 205
} as const;

export interface PromiseRequestHeader {
  kind: "promise";
  id: number;
  channel: string;
  method: string;
  /** Offset of the body (the serialized argument array) inside the frame. */
  bodyOffset: number;
}

export interface CancelRequestHeader {
  kind: "cancel";
  id: number;
}

/** EventListen: `[102, id, channel, event]` followed by the single listen argument. */
export interface ListenRequestHeader {
  kind: "listen";
  id: number;
  channel: string;
  event: string;
  bodyOffset: number;
}

export interface EventDisposeHeader {
  kind: "eventDispose";
  id: number;
}

export type InboundHeader = PromiseRequestHeader | CancelRequestHeader | ListenRequestHeader | EventDisposeHeader;

/** Header of a Host -> renderer Promise response (`[201|202, id]`). */
export interface OutboundResponseHeader {
  type: typeof ResponseType.PromiseSuccess | typeof ResponseType.PromiseError;
  id: number;
  bodyOffset: number;
}

/** Error payload shape the TS ChannelServer sends with PromiseError / EventError. */
export interface RpcErrorPayload {
  message: string;
  name: string;
  stack: string[] | undefined;
  code?: string;
}

const INBOUND_TYPES: ReadonlySet<number> = new Set([
  RequestType.Promise,
  RequestType.PromiseCancel,
  RequestType.EventListen,
  RequestType.EventDispose
]);

/**
 * Reads only the header of an inbound frame. Returns null for anything that is
 * not a request (responses to the Host's reverse client, flow control), so those
 * stay untouched and cheap.
 */
export function readInboundHeader(frame: Uint8Array): InboundHeader | null {
  // Header is always an array whose first element is an Int request/response type.
  if (frame[0] !== DataType.Array || frame[2] !== DataType.Int) return null;
  const typeByte = frame[3];
  if (typeByte === undefined || !INBOUND_TYPES.has(typeByte)) return null;
  try {
    const reader = new FrameReader(frame);
    const header = decodeValue(reader);
    if (!Array.isArray(header) || typeof header[1] !== "number") return null;
    if (header[0] === RequestType.PromiseCancel) return { kind: "cancel", id: header[1] };
    if (header[0] === RequestType.EventDispose) return { kind: "eventDispose", id: header[1] };
    const [, id, channel, name] = header as [number, number, unknown, unknown];
    if (typeof channel !== "string" || typeof name !== "string") return null;
    if (header[0] === RequestType.EventListen) return { kind: "listen", id, channel, event: name, bodyOffset: reader.offset };
    if (header[0] !== RequestType.Promise) return null;
    return { kind: "promise", id, channel, method: name, bodyOffset: reader.offset };
  } catch (error) {
    if (error instanceof RpcDecodeError || error instanceof SyntaxError) return null;
    throw error;
  }
}

/**
 * Reads the header of a frame the Host sends to the renderer. Returns null for
 * anything but a Promise success/error response (events, Host requests, Initialize).
 */
export function readOutboundResponseHeader(frame: Uint8Array): OutboundResponseHeader | null {
  if (frame[0] !== DataType.Array || frame[2] !== DataType.Int) return null;
  // 201/202 >= 128, so their VQL is two bytes: 0xC9|0xCA followed by 0x01.
  if ((frame[3] !== 0xc9 && frame[3] !== 0xca) || frame[4] !== 0x01) return null;
  try {
    const reader = new FrameReader(frame);
    const header = decodeValue(reader);
    if (!Array.isArray(header) || typeof header[1] !== "number") return null;
    const type = header[0];
    if (type !== ResponseType.PromiseSuccess && type !== ResponseType.PromiseError) return null;
    return { type, id: header[1], bodyOffset: reader.offset };
  } catch (error) {
    if (error instanceof RpcDecodeError || error instanceof SyntaxError) return null;
    throw error;
  }
}

/** Decodes the value that starts at `bodyOffset` (request args, listen arg, response body). */
export function readBody(frame: Uint8Array, bodyOffset: number): unknown {
  return decodeValue(new FrameReader(frame.subarray(bodyOffset)));
}

/**
 * Decodes the argument array of a Promise request (ProxyChannel passes `[params]`).
 * Returns null when the body is not an array; such calls are left to the TS Host.
 */
export function readRequestArgs(frame: Uint8Array, bodyOffset: number): unknown[] | null {
  const body = readBody(frame, bodyOffset);
  return Array.isArray(body) ? body : null;
}

function encodeFrame(header: unknown[], body: unknown): Uint8Array {
  const writer = new FrameWriter();
  encodeValue(writer, header);
  encodeValue(writer, body);
  return writer.toBytes();
}

export function encodeSuccess(id: number, data: unknown): Uint8Array {
  return encodeFrame([ResponseType.PromiseSuccess, id], data);
}

export function encodeError(id: number, payload: RpcErrorPayload): Uint8Array {
  return encodeFrame([ResponseType.PromiseError, id], payload);
}

export function encodeEventFire(id: number, data: unknown): Uint8Array {
  return encodeFrame([ResponseType.EventFire, id], data);
}

/** Failed subscription (ChannelServer `onEventListen` catch path). */
export function encodeEventError(id: number, payload: RpcErrorPayload): Uint8Array {
  return encodeFrame([ResponseType.EventError, id], payload);
}

/** Builds a request frame; used by tests, the differential script and injected Host calls. */
export function encodePromiseRequest(id: number, channel: string, method: string, args: unknown[]): Uint8Array {
  return encodeFrame([RequestType.Promise, id, channel, method], args);
}

export function encodeCancelRequest(id: number): Uint8Array {
  return encodeFrame([RequestType.PromiseCancel, id], undefined);
}

export function encodeEventListen(id: number, channel: string, event: string, arg: unknown): Uint8Array {
  return encodeFrame([RequestType.EventListen, id, channel, event], arg);
}

export function encodeEventDispose(id: number): Uint8Array {
  return encodeFrame([RequestType.EventDispose, id], undefined);
}

/** Decodes any frame as `[header, body]`; used by tests. */
export function decodeFrame(frame: Uint8Array): [unknown, unknown] {
  const reader = new FrameReader(frame);
  const header = decodeValue(reader);
  return [header, decodeValue(reader)];
}
