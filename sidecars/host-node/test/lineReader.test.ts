import { test } from "node:test";
import assert from "node:assert/strict";
import { PassThrough } from "node:stream";
import { readLines } from "../src/lineReader.ts";

function collect(maxLineBytes: number) {
  const stream = new PassThrough();
  const lines: string[] = [];
  const events: string[] = [];
  readLines(stream, {
    maxLineBytes,
    onLine: (line) => lines.push(line),
    onOverflow: () => events.push("overflow"),
    onEnd: () => events.push("end"),
    onError: () => events.push("error")
  });
  return { stream, lines, events };
}

test("joins lines split across chunks and skips blank lines", async () => {
  const { stream, lines, events } = collect(1024);
  stream.write('{"a":');
  stream.write('1}\n\n{"b":2}\r\n');
  stream.end();
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(lines, ['{"a":1}', '{"b":2}']);
  assert.deepEqual(events, ["end"]);
});

test("reports overflow once for a complete or pending oversized line", async () => {
  const complete = collect(8);
  complete.stream.write("123456789\nok\n");
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(complete.lines, []);
  assert.deepEqual(complete.events, ["overflow"]);

  const partial = collect(8);
  partial.stream.write("123456789");
  partial.stream.write("more");
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(partial.events, ["overflow"]);
});
