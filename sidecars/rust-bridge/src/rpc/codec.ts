/**
 * Binary value codec compatible with `@zcode/rpc` `serialization.ts`.
 *
 * Layout per value: one tag byte, a VQL length/int where needed, then data.
 * Only what the bridge needs is implemented: decoding request frames and
 * encoding response frames. The nested-Uint8Array JSON marker is honoured in
 * both directions so Object payloads round-trip exactly like the TS codec.
 */

export const DataType = {
  Undefined: 0,
  String: 1,
  Buffer: 2,
  VSBuffer: 3,
  Array: 4,
  Object: 5,
  Int: 6
} as const;

const NESTED_MARKER = "__zcode_rpc_nested_uint8array_v1";
const NESTED_BASE64_KEY = "base64";

const encoder = new TextEncoder();
const decoder = new TextDecoder();

export class RpcDecodeError extends Error {
  override readonly name = "RpcDecodeError";
}

/** Sequential reader over one frame. */
export class FrameReader {
  private pos = 0;
  private readonly bytes: Uint8Array;

  constructor(bytes: Uint8Array) {
    this.bytes = bytes;
  }

  get offset(): number {
    return this.pos;
  }

  readByte(): number {
    const value = this.bytes[this.pos];
    if (value === undefined) throw new RpcDecodeError("unexpected end of frame");
    this.pos += 1;
    return value;
  }

  readBytes(length: number): Uint8Array {
    const end = this.pos + length;
    if (end > this.bytes.byteLength) throw new RpcDecodeError("unexpected end of frame");
    const slice = this.bytes.subarray(this.pos, end);
    this.pos = end;
    return slice;
  }

  /** Unsigned VQL, identical to `readIntVQL` (7 data bits per byte, high bit = more). */
  readVql(): number {
    let value = 0;
    for (let shift = 0; shift < 35; shift += 7) {
      const next = this.readByte();
      value |= (next & 0x7f) << shift;
      if ((next & 0x80) === 0) return value;
    }
    throw new RpcDecodeError("VQL integer too long");
  }
}

function decodeJsonValue(_key: string, value: unknown): unknown {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return value;
  const record = value as Record<string, unknown>;
  const base64 = record[NESTED_BASE64_KEY];
  if (record[NESTED_MARKER] === true && typeof base64 === "string" && Object.keys(record).length === 2) {
    return new Uint8Array(Buffer.from(base64, "base64"));
  }
  return value;
}

export function decodeValue(reader: FrameReader): unknown {
  const tag = reader.readByte();
  switch (tag) {
    case DataType.Undefined:
      return undefined;
    case DataType.String:
      return decoder.decode(reader.readBytes(reader.readVql()));
    case DataType.Buffer:
    case DataType.VSBuffer:
      return reader.readBytes(reader.readVql()).slice();
    case DataType.Array: {
      const length = reader.readVql();
      const items: unknown[] = [];
      for (let index = 0; index < length; index += 1) items.push(decodeValue(reader));
      return items;
    }
    case DataType.Object:
      return JSON.parse(decoder.decode(reader.readBytes(reader.readVql())), decodeJsonValue) as unknown;
    case DataType.Int:
      return reader.readVql();
    default:
      throw new RpcDecodeError(`unknown data tag ${tag}`);
  }
}

/** Collects encoded chunks and concatenates once. */
export class FrameWriter {
  private readonly chunks: Uint8Array[] = [];
  private length = 0;

  push(chunk: Uint8Array): void {
    this.chunks.push(chunk);
    this.length += chunk.byteLength;
  }

  writeByte(value: number): void {
    this.push(Uint8Array.of(value));
  }

  /** Mirrors `writeInt32VQL` (unsigned shift, so negative ints use 5 bytes). */
  writeVql(input: number): void {
    if (input === 0) {
      this.writeByte(0);
      return;
    }
    const bytes: number[] = [];
    let value = input;
    while (value !== 0) {
      let byte = value & 0x7f;
      value = value >>> 7;
      if (value !== 0) byte |= 0x80;
      bytes.push(byte);
    }
    this.push(Uint8Array.from(bytes));
  }

  toBytes(): Uint8Array {
    const out = new Uint8Array(this.length);
    let offset = 0;
    for (const chunk of this.chunks) {
      out.set(chunk, offset);
      offset += chunk.byteLength;
    }
    return out;
  }
}

function encodeJsonValue(this: unknown, key: string, value: unknown): unknown {
  const original = (this as Record<string, unknown>)[key];
  if (original instanceof Uint8Array) {
    return { [NESTED_MARKER]: true, [NESTED_BASE64_KEY]: Buffer.from(original).toString("base64") };
  }
  return value;
}

export function encodeValue(writer: FrameWriter, data: unknown): void {
  if (data === undefined) {
    writer.writeByte(DataType.Undefined);
  } else if (typeof data === "string") {
    const bytes = encoder.encode(data);
    writer.writeByte(DataType.String);
    writer.writeVql(bytes.byteLength);
    writer.push(bytes);
  } else if (data instanceof Uint8Array) {
    writer.writeByte(DataType.Buffer);
    writer.writeVql(data.byteLength);
    writer.push(data);
  } else if (Array.isArray(data)) {
    writer.writeByte(DataType.Array);
    writer.writeVql(data.length);
    for (const item of data) encodeValue(writer, item);
  } else if (typeof data === "number" && (data | 0) === data) {
    writer.writeByte(DataType.Int);
    writer.writeVql(data);
  } else {
    const bytes = encoder.encode(JSON.stringify(data, encodeJsonValue));
    writer.writeByte(DataType.Object);
    writer.writeVql(bytes.byteLength);
    writer.push(bytes);
  }
}
