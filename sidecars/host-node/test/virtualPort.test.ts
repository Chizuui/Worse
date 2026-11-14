import { test } from "node:test";
import assert from "node:assert/strict";
import { createVirtualPort, type VirtualPortSink } from "../src/virtualPort.ts";

interface Recorded {
  binary: Uint8Array[];
  control: { t: "flow"; state: string }[];
  closes: { code: number; reason: string }[];
}

function createSink(): { sink: VirtualPortSink; recorded: Recorded } {
  const recorded: Recorded = { binary: [], control: [], closes: [] };
  const sink: VirtualPortSink = {
    sendBinary(bytes) {
      recorded.binary.push(new Uint8Array(bytes));
    },
    sendControl(frame) {
      recorded.control.push(frame);
    },
    closeSocket(code, reason) {
      recorded.closes.push({ code, reason });
    }
  };
  return { sink, recorded };
}

test("buffers inbound frames until start and flushes them asynchronously in order", async () => {
  const { sink } = createSink();
  const port = createVirtualPort(sink);
  const received: unknown[] = [];

  port.receive(new Uint8Array([3]));
  port.receive(new Uint8Array([1, 2]));

  // MessagePortProtocol calls start() in its constructor; ChannelServer subscribes afterwards.
  port.start();
  port.on("message", (event) => {
    received.push(event.data);
  });
  port.receive(new Uint8Array([4]));
  assert.deepEqual(received, []);

  await Promise.resolve();
  assert.deepEqual(
    received.map((bytes) => [...(bytes as Uint8Array)]),
    [[3], [1, 2], [4]]
  );

  port.receive(new Uint8Array([5]));
  assert.equal(received.length, 4);
});

test("sends the exact Uint8Array view as binary", () => {
  const { sink, recorded } = createSink();
  const port = createVirtualPort(sink);
  const backing = new Uint8Array([0, 1, 2, 3, 4]);
  port.postMessage(backing.subarray(1, 4));
  assert.equal(recorded.binary.length, 1);
  assert.deepEqual([...(recorded.binary[0] as Uint8Array)], [1, 2, 3]);
  assert.equal(recorded.binary[0]?.byteOffset, 0);
});

test("maps flow control objects to transport control frames", () => {
  const { sink, recorded } = createSink();
  const port = createVirtualPort(sink);
  port.postMessage({ __zcodeRpcControl: "connection-flow-v1", state: "saturated" });
  port.postMessage({ __zcodeRpcControl: "connection-flow-v1", state: "drained" });
  assert.deepEqual(recorded.control, [
    { t: "flow", state: "saturated" },
    { t: "flow", state: "drained" }
  ]);
  assert.equal(recorded.binary.length, 0);
});

test("drops unsupported outbound objects", () => {
  const { sink, recorded } = createSink();
  const port = createVirtualPort(sink);
  port.postMessage({ hello: "world" });
  assert.equal(recorded.binary.length, 0);
  assert.equal(recorded.control.length, 0);
});

test("close notifies close listeners once and closes the socket", () => {
  const { sink, recorded } = createSink();
  const port = createVirtualPort(sink);
  let closes = 0;
  port.once("close", () => {
    closes += 1;
  });

  port.close();
  port.notifyClosed();
  assert.equal(closes, 1);
  assert.deepEqual(recorded.closes, [{ code: 1000, reason: "host-closed" }]);
  assert.equal(port.isClosed, true);
});

test("remote close notifies listeners without closing the socket again", () => {
  const { sink, recorded } = createSink();
  const port = createVirtualPort(sink);
  let closes = 0;
  port.once("close", () => {
    closes += 1;
  });

  port.notifyClosed();
  assert.equal(closes, 1);
  assert.equal(recorded.closes.length, 0);

  port.postMessage(new Uint8Array([1]));
  assert.equal(recorded.binary.length, 0);
});

test("once close fires immediately for an already closed port", async () => {
  const { sink } = createSink();
  const port = createVirtualPort(sink);
  port.notifyClosed();
  await new Promise<void>((resolve) => {
    port.once("close", () => {
      resolve();
    });
  });
});
