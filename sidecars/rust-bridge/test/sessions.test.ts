import assert from "node:assert/strict";
import { test } from "node:test";
import { createInterceptingPort, type PortLike } from "../src/interceptingPort.ts";
import type { RustHostSupervisor } from "../src/process/supervisor.ts";
import { createAttachmentRouter } from "../src/routing/attachmentRouter.ts";
import { createBridgeStats } from "../src/routing/stats.ts";
import {
  decodeFrame,
  encodeEventDispose,
  encodeEventListen,
  encodePromiseRequest,
  encodeSuccess,
  readInboundHeader,
  readRequestArgs
} from "../src/rpc/frames.ts";
import { HOST_CALL_ID_BASE, createHostCaller } from "../src/sessions/hostCall.ts";
import { createOutputBuffer } from "../src/sessions/outputBuffer.ts";
import { HOST_LOST_EXIT_CODE, createPtyRouter, createPtySessionStore, type PtySessionStore } from "../src/sessions/ptyRouter.ts";
import { createSessionDirectory } from "../src/sessions/sessionDirectory.ts";
import type { TerminalPolicy, TerminalSettings } from "../src/sessions/terminalPolicy.ts";
import type { SessionDeps, SessionRouter } from "../src/sessions/types.ts";
import { createWatchRouter } from "../src/sessions/watchRouter.ts";
import { createFakeSupervisor, silentLogger, waitFor } from "./helpers.ts";

type Listener = (event: { data: unknown; ports: [] }) => void;

function createInnerPort() {
  const listeners = new Set<Listener>();
  const closeListeners: Array<() => void> = [];
  const sent: Uint8Array[] = [];
  const port: PortLike & { inject(data: Uint8Array): void; fireClose(): void; sent: Uint8Array[] } = {
    on: (_event, listener) => listeners.add(listener),
    off: (_event, listener) => listeners.delete(listener),
    once: (_event, listener) => closeListeners.push(listener),
    postMessage: (data) => {
      if (data instanceof Uint8Array) sent.push(data);
    },
    start: () => undefined,
    close: () => undefined,
    inject: (data) => {
      for (const listener of listeners) listener({ data, ports: [] });
    },
    fireClose: () => {
      for (const listener of closeListeners) listener();
    },
    sent
  };
  return port;
}

const seenSettings: TerminalSettings[] = [];
const fakePolicy: TerminalPolicy = {
  resolveShell: () => "fake-shell",
  resolveCwd: (cwd) => cwd ?? "/home",
  resolveEnv: () => ({ TERM: "xterm-256color" }),
  resolveFontProfile: (settings) => {
    seenSettings.push(settings);
    return { fontFamily: settings.terminalFontFamily ?? "mono", source: settings.terminalFontFamily ? "custom" : "fallback" };
  },
  resolveWindowsPtyInfo: () => undefined
};

interface Harness {
  supervisor: RustHostSupervisor;
  deps: SessionDeps;
  store: PtySessionStore;
}

function createHarness(): Harness {
  const supervisor = createFakeSupervisor();
  const deps: SessionDeps = { supervisor, directory: createSessionDirectory(), stats: createBridgeStats(), logger: silentLogger, trace: false };
  return { supervisor, deps, store: createPtySessionStore(deps) };
}

/**
 * One attachment wired like `bridge.ts`. The fake TS Host answers injected
 * `setting.get` calls through the wrapped port, as the real ChannelServer would.
 */
function attach(harness: Harness, channels: Array<"pty" | "watch">) {
  const inner = createInnerPort();
  const hostSeen: Uint8Array[] = [];
  const wrapped = createInterceptingPort(
    inner,
    (io) => {
      const hostCaller = createHostCaller(io.injectToHost);
      const sessionIo = { reply: io.reply, forward: io.forward };
      const sessionRouters: SessionRouter[] = [];
      if (channels.includes("watch")) sessionRouters.push(createWatchRouter(harness.deps, sessionIo));
      if (channels.includes("pty")) {
        sessionRouters.push(createPtyRouter({ ...harness.deps, store: harness.store, hostCaller, loadPolicy: () => Promise.resolve(fakePolicy) }, sessionIo));
      }
      const router = createAttachmentRouter(
        { ...harness.deps, services: new Set(), sessionRouters, handleOutbound: hostCaller.handleOutbound },
        io
      );
      return { ...router, dispose: () => (router.dispose(), hostCaller.dispose()) };
    },
    silentLogger
  );
  wrapped.on("message", (event) => {
    const frame = event.data as Uint8Array;
    const header = readInboundHeader(frame);
    if (header?.kind === "promise" && header.channel === "setting" && header.method === "get") {
      assert.ok(header.id >= HOST_CALL_ID_BASE);
      wrapped.postMessage(encodeSuccess(header.id, { terminalFontFamily: "Cascadia", other: true }));
      return;
    }
    hostSeen.push(frame);
  });
  const replies = () => inner.sent.map((frame) => decodeFrame(frame));
  return { inner, wrapped, hostSeen, replies };
}

const bodyOf = (replies: Array<[unknown, unknown]>, type: number, id: number): unknown[] =>
  replies.filter(([header]) => {
    const h = header as number[];
    return h[0] === type && h[1] === id;
  }).map(([, body]) => body);

test("output buffer keeps the newest 1 MiB of characters in order", () => {
  const buffer = createOutputBuffer(5);
  buffer.push("abc");
  buffer.push("defg");
  assert.deepEqual(buffer.drain(), ["defg"]);
  assert.deepEqual(buffer.drain(), []);
});

test("terminal create runs on Rust with the Host's settings; buffered output is replayed, echo and exit flow", async () => {
  const harness = createHarness();
  try {
    const a = attach(harness, ["pty"]);
    a.inner.inject(encodePromiseRequest(1, "terminal", "create", [{ cols: 80, rows: 24, cwd: "/w" }]));
    await waitFor(() => bodyOf(a.replies(), 201, 1).length === 1);
    const created = bodyOf(a.replies(), 201, 1)[0] as Record<string, unknown>;
    assert.match(String(created.id), /^rpty-\d+$/);
    // undefined 值的字段在编码时被省略（TS Host 同一编码器，行为一致）；其余字段保持 TS 顺序。
    assert.deepEqual(Object.keys(created), ["id", "shell", "fontFamily", "fontFamilySource"]);
    assert.equal(created.shell, "fake-shell");
    assert.equal(created.fontFamily, "Cascadia");
    assert.equal(seenSettings.at(-1)?.terminalFontFamily, "Cascadia");
    // setting.get 的应答属于 bridge，渲染端不能看到。
    assert.equal(a.inner.sent.some((frame) => (decodeFrame(frame)[0] as number[])[1]! >= HOST_CALL_ID_BASE), false);

    a.inner.inject(encodeEventListen(2, "terminal", "onDynamicData", created.id));
    a.inner.inject(encodeEventListen(3, "terminal", "onDynamicExit", created.id));
    await waitFor(() => bodyOf(a.replies(), 204, 2).length === 1);
    assert.deepEqual(bodyOf(a.replies(), 204, 2), ["hello 80x24"]);

    a.inner.inject(encodePromiseRequest(4, "terminal", "write", [{ id: created.id, data: "ü" }]));
    await waitFor(() => bodyOf(a.replies(), 204, 2).length === 2);
    assert.deepEqual(bodyOf(a.replies(), 204, 2)[1], "echo:ü");
    a.inner.inject(encodePromiseRequest(5, "terminal", "resize", [{ id: created.id, cols: 100, rows: 30 }]));
    a.inner.inject(encodePromiseRequest(6, "terminal", "write", [{ id: created.id, data: "exit 7" }]));
    await waitFor(() => bodyOf(a.replies(), 204, 3).length === 1);
    assert.deepEqual(bodyOf(a.replies(), 204, 3), [7]);
    assert.equal(harness.store.size(), 0);
    assert.equal(a.hostSeen.length, 0);

    // 会话结束后 id 不再属于 bridge，交给 TS Host（它会回答 Terminal not found）。
    a.inner.inject(encodePromiseRequest(7, "terminal", "write", [{ id: created.id, data: "x" }]));
    await waitFor(() => a.hostSeen.length === 1);
  } finally {
    await harness.supervisor.dispose();
  }
});

test("attachment close keeps the Host-global terminal; another attachment re-subscribes and dispose kills it", async () => {
  const harness = createHarness();
  try {
    const a = attach(harness, ["pty"]);
    a.inner.inject(encodePromiseRequest(1, "terminal", "create", [{ cols: 10, rows: 5 }]));
    await waitFor(() => bodyOf(a.replies(), 201, 1).length === 1);
    const id = (bodyOf(a.replies(), 201, 1)[0] as { id: string }).id;
    a.inner.inject(encodeEventListen(2, "terminal", "onDynamicData", id));
    await waitFor(() => bodyOf(a.replies(), 204, 2).length === 1);
    a.inner.fireClose();
    assert.equal(harness.store.size(), 1);

    const b = attach(harness, ["pty"]);
    b.inner.inject(encodePromiseRequest(1, "terminal", "write", [{ id, data: "after" }]));
    await waitFor(() => bodyOf(b.replies(), 201, 1).length === 1);
    await waitFor(() => harness.store.get(id)?.output !== undefined);
    b.inner.inject(encodeEventListen(2, "terminal", "onDynamicData", id));
    await waitFor(() => bodyOf(b.replies(), 204, 2).length === 1);
    assert.deepEqual(bodyOf(b.replies(), 204, 2), ["echo:after"]);
    b.inner.inject(encodeEventDispose(2));

    b.inner.inject(encodePromiseRequest(3, "terminal", "dispose", [{ id }]));
    await waitFor(() => bodyOf(b.replies(), 201, 3).length === 1);
    assert.equal(harness.store.size(), 0);
    const host = await harness.supervisor.acquire();
    assert.ok(host);
    const killed = (await host.connection.call("test.killed", {})) as string[];
    assert.equal(killed.length, 1);
  } finally {
    await harness.supervisor.dispose();
  }
});

test("harness-host crash ends live terminals with exit -1 instead of hanging", async () => {
  const harness = createHarness();
  try {
    const a = attach(harness, ["pty"]);
    a.inner.inject(encodePromiseRequest(1, "terminal", "create", [{ cols: 10, rows: 5 }]));
    await waitFor(() => bodyOf(a.replies(), 201, 1).length === 1);
    const id = (bodyOf(a.replies(), 201, 1)[0] as { id: string }).id;
    a.inner.inject(encodeEventListen(2, "terminal", "onDynamicExit", id));
    a.inner.inject(encodePromiseRequest(3, "terminal", "write", [{ id, data: "crash" }]));
    await waitFor(() => bodyOf(a.replies(), 204, 2).length === 1);
    assert.deepEqual(bodyOf(a.replies(), 204, 2), [HOST_LOST_EXIT_CODE]);
    assert.equal(harness.store.size(), 0);
  } finally {
    await harness.supervisor.dispose();
  }
});

test("terminal frames for ids the bridge does not own go to the TS Host", async () => {
  const harness = createHarness();
  try {
    const a = attach(harness, ["pty"]);
    a.inner.inject(encodePromiseRequest(1, "terminal", "write", [{ id: "3", data: "x" }]));
    a.inner.inject(encodeEventListen(2, "terminal", "onDynamicData", "3"));
    a.inner.inject(encodeEventDispose(2));
    assert.equal(a.hostSeen.length, 3);
    const args = readRequestArgs(a.hostSeen[0]!, readInboundHeader(a.hostSeen[0]!)!.bodyOffset);
    assert.deepEqual(args, [{ id: "3", data: "x" }]);
  } finally {
    await harness.supervisor.dispose();
  }
});

test("file-watcher: watch, event, unwatch on Rust; AppError surfaced; attachment close unsubscribes", async () => {
  const harness = createHarness();
  try {
    const a = attach(harness, ["watch"]);
    a.inner.inject(encodePromiseRequest(1, "file-watcher", "watch", [{ path: "/d" }]));
    await waitFor(() => bodyOf(a.replies(), 201, 1).length === 1);
    const id = (bodyOf(a.replies(), 201, 1)[0] as { id: string }).id;
    assert.match(id, /^rw-\d+$/);
    a.inner.inject(encodeEventListen(2, "file-watcher", "onDynamicChange", id));
    const host = await harness.supervisor.acquire();
    assert.ok(host);
    assert.equal(harness.deps.directory.size(), 1);

    a.inner.inject(encodePromiseRequest(3, "file-watcher", "watch", [{ path: "missing" }]));
    await waitFor(() => bodyOf(a.replies(), 202, 3).length === 1);
    assert.equal((bodyOf(a.replies(), 202, 3)[0] as { message: string }).message, "无法监视目录 'missing': ENOENT");

    a.inner.inject(encodePromiseRequest(4, "file-watcher", "unwatch", [{ id }]));
    await waitFor(() => bodyOf(a.replies(), 201, 4).length === 1);
    assert.equal(harness.deps.directory.size(), 0);

    a.inner.inject(encodePromiseRequest(5, "file-watcher", "watch", [{ path: "/e", recursive: true }]));
    await waitFor(() => bodyOf(a.replies(), 201, 5).length === 1);
    assert.equal(harness.deps.directory.size(), 1);
    a.inner.fireClose();
    await waitFor(() => harness.deps.directory.size() === 0);
    const killed = (await host.connection.call("test.killed", {})) as string[];
    assert.equal(killed.length, 2);
    assert.equal(a.hostSeen.length, 0);
  } finally {
    await harness.supervisor.dispose();
  }
});

test("file-watcher events reach listeners; host loss sends a final {dirPath}", async () => {
  const harness = createHarness();
  try {
    const a = attach(harness, ["watch"]);
    a.inner.inject(encodePromiseRequest(1, "file-watcher", "watch", [{ path: "/d" }]));
    await waitFor(() => bodyOf(a.replies(), 201, 1).length === 1);
    const id = (bodyOf(a.replies(), 201, 1)[0] as { id: string }).id;
    a.inner.inject(encodeEventListen(2, "file-watcher", "onDynamicChange", id));
    const host = await harness.supervisor.acquire();
    assert.ok(host);
    // fixture watch ids are `w<n>`; the only live one is the one we subscribed.
    for (let n = 1; n < 10; n++) {
      await host.connection.call("test.fireWatch", { watchId: `w${n}`, event: { dirPath: "/d", changedPath: "/d/a" } });
    }
    await waitFor(() => bodyOf(a.replies(), 204, 2).length === 1);
    assert.deepEqual(bodyOf(a.replies(), 204, 2), [{ dirPath: "/d", changedPath: "/d/a" }]);
    harness.supervisor.killNow();
    await waitFor(() => bodyOf(a.replies(), 204, 2).length === 2);
    assert.deepEqual(bodyOf(a.replies(), 204, 2)[1], { dirPath: "/d" });
    assert.equal(harness.deps.directory.size(), 0);
  } finally {
    await harness.supervisor.dispose();
  }
});
