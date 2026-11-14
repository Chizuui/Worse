import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import path from "node:path";
import { test } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import { FrameReader, FrameWriter, decodeValue, encodeValue } from "../src/rpc/codec.ts";
import {
  decodeFrame,
  encodeCancelRequest,
  encodeError,
  encodePromiseRequest,
  encodeSuccess,
  readInboundHeader,
  readRequestArgs
} from "../src/rpc/frames.ts";

interface ZcodeRpcBuffer {
  buffer: Uint8Array;
}
interface ZcodeRpc {
  BufferWriter: new () => { buffer: ZcodeRpcBuffer; write(buffer: ZcodeRpcBuffer): void };
  BufferReader: new (buffer: ZcodeRpcBuffer) => unknown;
  VSBuffer: { wrap(bytes: Uint8Array): ZcodeRpcBuffer };
  serialize(writer: unknown, data: unknown): void;
  deserialize(reader: unknown): unknown;
}

const testDir = path.dirname(fileURLToPath(import.meta.url));
const rpcDist = path.resolve(testDir, "../../../../packages/rpc/dist/index.js");

const SAMPLES: unknown[] = [
  undefined,
  null,
  true,
  0,
  127,
  128,
  2_147_483_647,
  -1,
  -2_147_483_648,
  1.5,
  "",
  "plain",
  "unicode 中文 😀 \t\n\\",
  [],
  [100, 7, "file", "readdir"],
  [{ path: "C:\\w\\x", includeHidden: true }],
  { nested: { list: [1, "a", null] }, flag: false },
  new Uint8Array([0, 1, 2, 255]),
  { bytes: new Uint8Array([9, 8, 7]) }
];

function encodeOurs(value: unknown): Uint8Array {
  const writer = new FrameWriter();
  encodeValue(writer, value);
  return writer.toBytes();
}

test("codec round-trips every sample through itself", () => {
  for (const sample of SAMPLES) {
    assert.deepEqual(decodeValue(new FrameReader(encodeOurs(sample))), sample);
  }
});

test("codec is byte-identical to @zcode/rpc serialize and decodes its output", { skip: !existsSync(rpcDist) }, async () => {
  const rpc = (await import(pathToFileURL(rpcDist).href)) as ZcodeRpc;
  for (const sample of SAMPLES) {
    const writer = new rpc.BufferWriter();
    rpc.serialize(writer, sample);
    const theirs = writer.buffer.buffer;
    assert.deepEqual(encodeOurs(sample), theirs, `encoding differs for ${String(JSON.stringify(sample))}`);
    const decodedByThem = rpc.deserialize(new rpc.BufferReader(rpc.VSBuffer.wrap(encodeOurs(sample))));
    const decodedByUs = decodeValue(new FrameReader(theirs));
    assert.deepEqual(decodedByUs, decodedByThem);
  }
});

test("readInboundHeader picks Promise and PromiseCancel requests only", () => {
  const request = encodePromiseRequest(42, "file", "stat", [{ path: "a" }]);
  const header = readInboundHeader(request);
  assert.ok(header !== null && header.kind === "promise");
  assert.equal(header.id, 42);
  assert.equal(header.channel, "file");
  assert.equal(header.method, "stat");
  assert.deepEqual(readRequestArgs(request, header.bodyOffset), [{ path: "a" }]);

  assert.deepEqual(readInboundHeader(encodeCancelRequest(42)), { kind: "cancel", id: 42 });
  assert.equal(readInboundHeader(encodeSuccess(42, "x")), null, "responses are not requests");
  assert.equal(readInboundHeader(Uint8Array.of(4, 4, 6, 100)), null, "truncated frames are ignored");
  assert.equal(readInboundHeader(new Uint8Array()), null);
});

test("readRequestArgs returns null for a non-array body", () => {
  const writer = new FrameWriter();
  encodeValue(writer, [100, 1, "file", "stat"]);
  encodeValue(writer, { path: "a" });
  const frame = writer.toBytes();
  const header = readInboundHeader(frame);
  assert.ok(header !== null && header.kind === "promise");
  assert.equal(readRequestArgs(frame, header.bodyOffset), null);
});

test("response frames match the ChannelServer layout", () => {
  assert.deepEqual(decodeFrame(encodeSuccess(3, [1, 2])), [[201, 3], [1, 2]]);
  const [header, body] = decodeFrame(encodeError(4, { message: "m", name: "Error", stack: undefined, code: "ENOENT" }));
  assert.deepEqual(header, [202, 4]);
  assert.deepEqual(body, { message: "m", name: "Error", code: "ENOENT" });
});
