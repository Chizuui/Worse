import { test } from "node:test";
import assert from "node:assert/strict";
import {
  MAX_LINE_BYTES,
  PROTOCOL_VERSION,
  SidecarProtocolError,
  parseShellLine,
  parseSidecarLine,
  serializeShellLine,
  serializeSidecarLine,
  type SidecarLine
} from "../src/protocol.ts";

function expectCode(fn: () => unknown, code: string): void {
  try {
    fn();
  } catch (error) {
    assert.ok(error instanceof SidecarProtocolError, "expected a SidecarProtocolError");
    assert.equal(error.code, code);
    return;
  }
  assert.fail(`expected SidecarProtocolError with code ${code}`);
}

test("protocol constants match the contract", () => {
  assert.equal(PROTOCOL_VERSION, 1);
  assert.equal(MAX_LINE_BYTES, 16 * 1024 * 1024);
});

test("parses every shell line kind", () => {
  const attachOpen = parseShellLine(
    JSON.stringify({
      v: 1,
      t: "attach.open",
      requestId: "req-1",
      attachmentId: "att-1",
      token: "a".repeat(64),
      ttlMs: 30000,
      hostMessage: { type: "init-local", databaseStartupId: "startup-1" }
    })
  );
  assert.equal(attachOpen.t, "attach.open");
  if (attachOpen.t !== "attach.open") return;
  assert.equal(attachOpen.requestId, "req-1");
  assert.equal(attachOpen.attachmentId, "att-1");
  assert.equal(attachOpen.ttlMs, 30000);
  assert.equal(attachOpen.hostMessage.type, "init-local");

  const host = parseShellLine(JSON.stringify({ v: 1, t: "host", message: { type: "dispose" } }));
  assert.deepEqual(host, { v: 1, t: "host", message: { type: "dispose" } });

  const ping = parseShellLine(JSON.stringify({ v: 1, t: "ping", id: "ping-1" }));
  assert.deepEqual(ping, { v: 1, t: "ping", id: "ping-1" });
});

test("round-trips shell lines", () => {
  const line = {
    v: 1 as const,
    t: "attach.open" as const,
    requestId: "req-2",
    attachmentId: "att-2",
    token: "b".repeat(64),
    ttlMs: 1000,
    hostMessage: { type: "attach-service-port" }
  };
  assert.deepEqual(parseShellLine(serializeShellLine(line)), line);
});

test("parses every sidecar line kind", () => {
  const lines: SidecarLine[] = [
    { v: 1, t: "ready", hostPid: 42, wsPort: 5555, protocol: 1 },
    { v: 1, t: "attach.opened", requestId: "req-1", attachmentId: "att-1" },
    { v: 1, t: "attach.active", attachmentId: "att-1", messageType: "init-local" },
    { v: 1, t: "attach.closed", attachmentId: "att-1", reason: "ws-closed" },
    { v: 1, t: "attach.failed", requestId: "req-1", code: "attach_duplicate", message: "dup" },
    { v: 1, t: "host", message: { type: "database-startup-state", state: { phase: "ready" } } },
    { v: 1, t: "fatal", code: "internal", message: "boom" },
    { v: 1, t: "log", level: "warn", message: "careful" },
    { v: 1, t: "pong", id: "ping-1" }
  ];
  for (const line of lines) {
    assert.deepEqual(parseSidecarLine(serializeSidecarLine(line)), line);
  }
});

test("rejects malformed shell lines", () => {
  expectCode(() => parseShellLine("{"), "bad_line");
  expectCode(() => parseShellLine("[]"), "bad_line");
  expectCode(() => parseShellLine("null"), "bad_line");
  expectCode(() => parseShellLine(JSON.stringify({ t: "ping", id: "x" })), "bad_line");
  expectCode(() => parseShellLine(JSON.stringify({ v: 2, t: "ping", id: "x" })), "unsupported_version");
  expectCode(() => parseShellLine(JSON.stringify({ v: 1, t: "nope" })), "unknown_kind");
});

test("rejects missing and invalid fields", () => {
  expectCode(
    () =>
      parseShellLine(
        JSON.stringify({ v: 1, t: "attach.open", requestId: "r", attachmentId: "a", ttlMs: 1, hostMessage: {} })
      ),
    "bad_line"
  );
  expectCode(
    () =>
      parseSidecarLine(
        JSON.stringify({ v: 1, t: "attach.closed", attachmentId: "a", reason: "because" })
      ),
    "bad_line"
  );
  expectCode(
    () => parseSidecarLine(JSON.stringify({ v: 1, t: "fatal", code: "made-up", message: "x" })),
    "bad_line"
  );
  expectCode(
    () => parseSidecarLine(JSON.stringify({ v: 1, t: "attach.active", attachmentId: "a", messageType: "other" })),
    "bad_line"
  );
});
