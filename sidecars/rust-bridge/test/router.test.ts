import assert from "node:assert/strict";
import { test } from "node:test";
import { createInterceptingPort, type PortLike } from "../src/interceptingPort.ts";
import type { RustHostSupervisor } from "../src/process/supervisor.ts";
import { createAttachmentRouter } from "../src/routing/attachmentRouter.ts";
import type { RustService } from "../src/routing/routeTable.ts";
import { createBridgeStats, type BridgeStats } from "../src/routing/stats.ts";
import { decodeFrame, encodeCancelRequest, encodePromiseRequest } from "../src/rpc/frames.ts";
import { createFakeSupervisor, silentLogger, waitFor } from "./helpers.ts";

type Listener = (event: { data: unknown; ports: [] }) => void;

/** Inner port double: `inject` simulates a renderer frame, `sent` records replies. */
function createInnerPort() {
  const listeners = new Set<Listener>();
  const sent: unknown[] = [];
  const port: PortLike & { inject(data: unknown): void; sent: unknown[] } = {
    on: (_event, listener) => listeners.add(listener),
    off: (_event, listener) => listeners.delete(listener),
    once: () => undefined,
    postMessage: (data) => sent.push(data),
    start: () => undefined,
    close: () => undefined,
    inject: (data) => {
      for (const listener of listeners) listener({ data, ports: [] });
    },
    sent
  };
  return port;
}

function setup(supervisor: RustHostSupervisor, services: RustService[] = ["fs", "search", "git"]) {
  const inner = createInnerPort();
  const stats: BridgeStats = createBridgeStats();
  const hostSeen: unknown[] = [];
  const wrapped = createInterceptingPort(
    inner,
    (io) => createAttachmentRouter({ supervisor, services: new Set(services), stats, logger: silentLogger, trace: false }, io),
    silentLogger
  );
  wrapped.on("message", (event) => hostSeen.push(event.data));
  return { inner, wrapped, stats, hostSeen };
}

test("routed requests are answered by Rust and never reach the Host", async () => {
  const supervisor = createFakeSupervisor();
  try {
    const { inner, hostSeen, stats } = setup(supervisor);
    inner.inject(encodePromiseRequest(1, "file", "stat", [{ path: "a" }]));
    inner.inject(encodePromiseRequest(2, "file", "readFileRange", [{ path: "a", offset: 0, length: 3 }]));
    await waitFor(() => inner.sent.length === 2);
    const replies = new Map(inner.sent.map((frame) => {
      const [header, body] = decodeFrame(frame as Uint8Array);
      return [(header as number[])[1], { header, body }];
    }));
    assert.deepEqual(replies.get(1), { header: [201, 1], body: { path: "a", type: "file", size: 3, mtimeMs: 1.5 } });
    assert.deepEqual(replies.get(2)?.body, new Uint8Array(Buffer.from("hey")));
    assert.equal(hostSeen.length, 0);
    assert.equal(stats.snapshot()["file.stat"]?.rust, 1);
  } finally {
    await supervisor.dispose();
  }
});

test("IoError is surfaced as a Node-style PromiseError", async () => {
  const supervisor = createFakeSupervisor();
  try {
    const { inner, hostSeen } = setup(supervisor);
    inner.inject(encodePromiseRequest(5, "file", "stat", [{ path: "missing" }]));
    await waitFor(() => inner.sent.length === 1);
    const [header, body] = decodeFrame(inner.sent[0] as Uint8Array);
    assert.deepEqual(header, [202, 5]);
    assert.equal((body as { message: string }).message, "ENOENT: no such file or directory, stat 'missing'");
    assert.equal((body as { code: string }).code, "ENOENT");
    assert.equal(hostSeen.length, 0);
  } finally {
    await supervisor.dispose();
  }
});

test("internal errors and unadvertised methods fall back to the Host with the original bytes", async () => {
  const supervisor = createFakeSupervisor();
  try {
    const { inner, hostSeen, stats } = setup(supervisor);
    const internal = encodePromiseRequest(7, "file", "stat", [{ path: "internal" }]);
    const unadvertised = encodePromiseRequest(8, "git", "getDiff", [{ workspacePath: "w", path: "p" }]);
    inner.inject(internal);
    inner.inject(unadvertised);
    await waitFor(() => hostSeen.length === 2);
    assert.deepEqual(new Set(hostSeen), new Set([internal, unadvertised]));
    assert.equal(inner.sent.length, 0, "the bridge does not answer fallbacks itself");
    assert.equal(stats.snapshot()["file.stat"]?.fallback, 1);
  } finally {
    await supervisor.dispose();
  }
});

test("non-routed traffic passes through synchronously and in order", async () => {
  const supervisor = createFakeSupervisor();
  try {
    const { inner, hostSeen } = setup(supervisor, ["fs"]);
    const settings = encodePromiseRequest(1, "setting", "get", []);
    const gitRead = encodePromiseRequest(2, "git", "refresh", [{ workspacePath: "w" }]);
    const mutation = encodePromiseRequest(3, "git", "commit", [{ workspacePath: "w", message: "m" }]);
    const flow = { __zcodeRpcControl: "connection-flow-v1", state: "drained" };
    inner.inject(settings);
    inner.inject(gitRead);
    inner.inject(mutation);
    inner.inject(flow);
    assert.deepEqual(hostSeen, [settings, gitRead, mutation, flow], "git flag off, mutation never routed");
  } finally {
    await supervisor.dispose();
  }
});

test("cancel of a routed request is consumed and the late answer is dropped", async () => {
  const supervisor = createFakeSupervisor();
  try {
    const { inner, hostSeen, stats } = setup(supervisor);
    // Both frames arrive in the same tick, so the cancel lands while the request is still in flight.
    await supervisor.acquire();
    inner.inject(encodePromiseRequest(11, "file", "stat", [{ path: "a" }]));
    inner.inject(encodeCancelRequest(11));
    await new Promise((resolve) => setTimeout(resolve, 200));
    assert.equal(inner.sent.length, 0, "no answer after cancel");
    assert.equal(hostSeen.length, 0, "cancel frame not forwarded");
    assert.equal(stats.snapshot()["file.stat"]?.cancelled, 1);
    inner.inject(encodeCancelRequest(99));
    assert.equal(hostSeen.length, 1, "cancel for an unknown id belongs to the Host");
  } finally {
    await supervisor.dispose();
  }
});

test("a disabled supervisor makes the router fully transparent", async () => {
  const supervisor = createFakeSupervisor("normal", "Z:/missing/harness-host.exe");
  await supervisor.acquire();
  const { inner, hostSeen } = setup(supervisor);
  const frame = encodePromiseRequest(1, "file", "stat", [{ path: "a" }]);
  inner.inject(frame);
  assert.deepEqual(hostSeen, [frame]);
  await supervisor.dispose();
});
