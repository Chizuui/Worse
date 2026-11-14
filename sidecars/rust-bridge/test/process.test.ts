import assert from "node:assert/strict";
import { test } from "node:test";
import { RustCallError, RustCancelledError, RustTransportError } from "../src/process/jsonRpcConnection.ts";
import { createFakeSupervisor, waitFor } from "./helpers.ts";

test("handshake advertises methods and calls correlate results, errors and progress", async () => {
  const supervisor = createFakeSupervisor();
  try {
    const host = await supervisor.acquire();
    assert.ok(host !== null);
    assert.equal(supervisor.state.kind, "ready");
    assert.ok(host.methods.has("file.stat"));
    assert.deepEqual(await host.connection.call("file.stat", { path: "a" }), {
      path: "a",
      type: "file",
      size: 3,
      mtimeMs: 1.5
    });
    await assert.rejects(host.connection.call("file.stat", { path: "missing" }), (error: unknown) => {
      assert.ok(error instanceof RustCallError);
      assert.equal(error.rpcError.code, 1);
      return true;
    });
    const progress: unknown[] = [];
    await host.connection.call("file.readdir", { path: "a" }, { onProgress: (value) => progress.push(value) });
    assert.deepEqual(progress, [{ n: 1 }]);
    assert.equal(await supervisor.acquire(), host, "a ready process is reused");
  } finally {
    await supervisor.dispose();
  }
});

test("abort sends $/cancel and the call rejects as cancelled", async () => {
  const supervisor = createFakeSupervisor();
  try {
    const host = await supervisor.acquire();
    assert.ok(host !== null);
    const controller = new AbortController();
    const pending = host.connection.call("slow.method", {}, { signal: controller.signal });
    controller.abort();
    await assert.rejects(pending, RustCancelledError);
  } finally {
    await supervisor.dispose();
  }
});

test("timeouts reject with a transport error without killing the process", async () => {
  const supervisor = createFakeSupervisor();
  try {
    const host = await supervisor.acquire();
    assert.ok(host !== null);
    await assert.rejects(host.connection.call("slow.method", {}, { timeoutMs: 50 }), (error: unknown) => {
      assert.ok(error instanceof RustTransportError);
      assert.equal(error.reason, "timeout");
      return true;
    });
    assert.equal(host.connection.isClosed, false);
  } finally {
    await supervisor.dispose();
  }
});

test("a crash closes pending calls and puts the supervisor into backoff", async () => {
  const supervisor = createFakeSupervisor();
  try {
    const host = await supervisor.acquire();
    assert.ok(host !== null);
    await assert.rejects(host.connection.call("crash.method", {}), RustTransportError);
    await waitFor(() => supervisor.state.kind === "backoff");
    assert.equal(await supervisor.acquire(), null, "no respawn during backoff");
  } finally {
    await supervisor.dispose();
  }
});

test("a protocol version mismatch disables Rust for the process lifetime", async () => {
  const supervisor = createFakeSupervisor("bad-version");
  try {
    assert.equal(await supervisor.acquire(), null);
    assert.equal(supervisor.state.kind, "disabled");
  } finally {
    await supervisor.dispose();
  }
});

test("a missing binary disables Rust without spawning", async () => {
  const supervisor = createFakeSupervisor("normal", "Z:/definitely/missing/harness-host.exe");
  assert.equal(await supervisor.acquire(), null);
  assert.equal(supervisor.state.kind, "disabled");
  await supervisor.dispose();
});

test("a crash during the handshake counts as a failure and backs off", async () => {
  const supervisor = createFakeSupervisor("crash-on-init");
  try {
    assert.equal(await supervisor.acquire(), null);
    assert.equal(supervisor.state.kind, "backoff");
  } finally {
    await supervisor.dispose();
  }
});
