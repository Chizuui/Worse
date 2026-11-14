import assert from "node:assert/strict";
import { test } from "node:test";
import { createInterceptingPort, type PortLike } from "../src/interceptingPort.ts";
import type { RustHostSupervisor } from "../src/process/supervisor.ts";
import { createAttachmentRouter } from "../src/routing/attachmentRouter.ts";
import { parseRustServiceFlags } from "../src/routing/flags.ts";
import { mapRustResult } from "../src/routing/resultMapping.ts";
import { findRoute, type RustService } from "../src/routing/routeTable.ts";
import { createBridgeStats } from "../src/routing/stats.ts";
import { attachmentServices } from "../src/routing/storeScope.ts";
import { decodeFrame, encodePromiseRequest } from "../src/rpc/frames.ts";
import { createFakeSupervisor, silentLogger, waitFor } from "./helpers.ts";

type Listener = (event: { data: unknown; ports: [] }) => void;

function setup(supervisor: RustHostSupervisor, services: RustService[]) {
  const listeners = new Set<Listener>();
  const sent: unknown[] = [];
  const inner: PortLike & { inject(data: unknown): void } = {
    on: (_event, listener) => listeners.add(listener),
    off: (_event, listener) => listeners.delete(listener),
    once: () => undefined,
    postMessage: (data) => sent.push(data),
    start: () => undefined,
    close: () => undefined,
    inject: (data) => {
      for (const listener of listeners) listener({ data, ports: [] });
    }
  };
  const stats = createBridgeStats();
  const hostSeen: unknown[] = [];
  const wrapped = createInterceptingPort(
    inner,
    (io) => createAttachmentRouter({ supervisor, services: new Set(services), stats, logger: silentLogger, trace: false }, io),
    silentLogger
  );
  wrapped.on("message", (event) => hostSeen.push(event.data));
  return { inner, sent, hostSeen, stats };
}

test("store is opt-in: not in the default set, enabled by name or all", () => {
  assert.equal(parseRustServiceFlags(undefined).services.has("store"), false);
  assert.equal(parseRustServiceFlags("store").services.has("store"), true);
  assert.equal(parseRustServiceFlags("all").services.has("store"), true);
  assert.deepEqual(findRoute("project", "readTree"), {
    service: "store",
    rustMethod: "store.readTree",
    timeoutMs: 35_000,
    result: "jsonText",
    allowNoArgs: true
  });
  assert.equal(findRoute("task-groups", "read")?.rustMethod, "store.readGroups");
  assert.equal(findRoute("project", "create"), undefined, "mutations stay on TS");
  assert.equal(findRoute("task-groups", "createGroup"), undefined, "mutations stay on TS");
});

test("jsonText keeps the text's key order and rejects malformed results", () => {
  const mapped = mapRustResult("jsonText", { json: '{"b":1,"a":{"z":0,"y":[1.5,null]}}' });
  assert.equal(mapped.ok, true);
  assert.equal(JSON.stringify(mapped.ok ? mapped.value : null), '{"b":1,"a":{"z":0,"y":[1.5,null]}}');
  assert.equal(mapRustResult("jsonText", { json: "{" }).ok, false);
  assert.equal(mapRustResult("jsonText", { other: 1 }).ok, false);
  assert.equal(mapRustResult("jsonText", "x").ok, false);
});

test("store is stripped for non-desktop attachments only", () => {
  const all = new Set<RustService>(["git", "store"]);
  assert.equal(attachmentServices(all, { type: "init-local" }).has("store"), true);
  assert.equal(
    attachmentServices(all, { type: "attach-service-port", scope: { kind: "local" }, clientMode: "desktop-continuous" }).has("store"),
    true
  );
  const phone = attachmentServices(all, { type: "attach-service-port", scope: { kind: "local" }, clientMode: "web-remote-replayable" });
  assert.deepEqual(Array.from(phone), ["git"]);
  const noStore = new Set<RustService>(["git"]);
  assert.equal(attachmentServices(noStore, { type: "attach-service-port" }), noStore);
});

test("zero-arg and object calls are routed; jsonText results and fallbacks are honoured", async () => {
  const supervisor = createFakeSupervisor();
  try {
    const { inner, sent, hostSeen, stats } = setup(supervisor, ["store"]);
    inner.inject(encodePromiseRequest(1, "task-groups", "read", []));
    inner.inject(encodePromiseRequest(2, "project", "readTree", [{ includeDeleted: true }]));
    const fallback = encodePromiseRequest(3, "project", "readTree", [{ includeDeleted: "fallback" }]);
    const badText = encodePromiseRequest(4, "project", "readTree", [{ includeDeleted: "bad-text" }]);
    const twoArgs = encodePromiseRequest(5, "project", "readTree", [{}, {}]);
    const notRouted = encodePromiseRequest(6, "project", "create", [{ name: "x" }]);
    inner.inject(fallback);
    inner.inject(badText);
    inner.inject(twoArgs);
    inner.inject(notRouted);
    await waitFor(() => sent.length === 2 && hostSeen.length === 4);
    const replies = new Map(
      sent.map((frame) => {
        const [header, body] = decodeFrame(frame as Uint8Array);
        return [(header as number[])[1], body];
      })
    );
    assert.equal(JSON.stringify(replies.get(1)), '{"groups":[{"id":"g","b":1,"a":null}],"members":[],"order":[]}');
    assert.deepEqual(replies.get(2), { projects: [], echo: { includeDeleted: true }, tasks: [] });
    assert.deepEqual(new Set(hostSeen), new Set([fallback, badText, twoArgs, notRouted]));
    assert.equal(stats.snapshot()["store.readTree"]?.fallback, 2);
    assert.equal(stats.snapshot()["store.readGroups"]?.rust, 1);
  } finally {
    await supervisor.dispose();
  }
});

test("zero-arg calls of routes without allowNoArgs still go to the Host", async () => {
  const supervisor = createFakeSupervisor();
  try {
    const { inner, hostSeen } = setup(supervisor, ["git"]);
    const frame = encodePromiseRequest(1, "git", "refresh", []);
    inner.inject(frame);
    assert.deepEqual(hostSeen, [frame]);
  } finally {
    await supervisor.dispose();
  }
});
