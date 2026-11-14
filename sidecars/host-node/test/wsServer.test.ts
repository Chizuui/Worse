import { test } from "node:test";
import assert from "node:assert/strict";
import WebSocket from "ws";
import { createAttachmentServer } from "../src/wsServer.ts";

const ORIGIN = "http://test.local";
const TOKEN = "a".repeat(64);

function wsUrl(port: number): string {
  return `ws://127.0.0.1:${port}/v1/attach`;
}

function waitOpen(client: WebSocket): Promise<void> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("websocket open timeout")), 2000);
    const onOpen = (): void => {
      clearTimeout(timer);
      client.off("error", onError);
      resolve();
    };
    const onError = (error: Error): void => {
      clearTimeout(timer);
      reject(error);
    };
    client.once("open", onOpen);
    client.once("error", onError);
  });
}

function nextMessage(client: WebSocket): Promise<{ isBinary: boolean; text: string; bytes: Uint8Array }> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("websocket message timeout")), 2000);
    client.once("message", (data, isBinary) => {
      clearTimeout(timer);
      const buffer = Buffer.isBuffer(data) ? data : Buffer.from(data as ArrayBuffer);
      resolve({ isBinary, text: buffer.toString("utf8"), bytes: new Uint8Array(buffer) });
    });
  });
}

function nextClose(client: WebSocket): Promise<{ code: number; reason: string }> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("websocket close timeout")), 2000);
    client.once("close", (code, reason) => {
      clearTimeout(timer);
      resolve({ code, reason: reason.toString() });
    });
  });
}

function connect(port: number, origin = ORIGIN): WebSocket {
  return new WebSocket(wsUrl(port), { headers: { Origin: origin } });
}

test("accepts a hello, forwards an echo, and reports ws-closed", async () => {
  const server = await createAttachmentServer({ allowedOrigins: [ORIGIN], helloTimeoutMs: 1000 });
  try {
    let attachedId: string | null = null;
    const closed: { id: string; reason: string }[] = [];
    server.onAttached((attachmentId, port) => {
      attachedId = attachmentId;
      port.on("message", (event) => {
        port.postMessage(event.data as Uint8Array);
      });
      port.start();
    });
    server.onClosed((attachmentId, reason) => {
      closed.push({ id: attachmentId, reason });
    });

    server.registerPending({ attachmentId: "att-1", token: TOKEN, ttlMs: 5000 });
    const client = connect(server.port);
    try {
      await waitOpen(client);
      client.send(JSON.stringify({ t: "hello", attachmentId: "att-1", token: TOKEN }));

      const attached = await nextMessage(client);
      assert.equal(attached.isBinary, false);
      assert.equal(attached.text, '{"t":"attached"}');
      assert.equal(attachedId, "att-1");

      client.send(new Uint8Array([1, 2, 3, 250]));
      const echo = await nextMessage(client);
      assert.equal(echo.isBinary, true);
      assert.deepEqual([...echo.bytes], [1, 2, 3, 250]);
    } finally {
      client.close();
    }

    await new Promise((resolve) => setTimeout(resolve, 50));
    assert.deepEqual(closed, [{ id: "att-1", reason: "ws-closed" }]);
  } finally {
    await server.close();
  }
});

test("rejects a wrong token with 4401", async () => {
  const server = await createAttachmentServer({ allowedOrigins: [ORIGIN], helloTimeoutMs: 1000 });
  try {
    server.registerPending({ attachmentId: "att-2", token: TOKEN, ttlMs: 5000 });
    const client = connect(server.port);
    try {
      await waitOpen(client);
      client.send(JSON.stringify({ t: "hello", attachmentId: "att-2", token: "b".repeat(64) }));
      const error = await nextMessage(client);
      assert.equal(error.isBinary, false);
      assert.equal(JSON.parse(error.text).code, "ws_token_rejected");
      const closed = await nextClose(client);
      assert.equal(closed.code, 4401);
    } finally {
      client.terminate();
    }
  } finally {
    await server.close();
  }
});

test("rejects a disallowed origin with HTTP 403", async () => {
  const server = await createAttachmentServer({ allowedOrigins: [ORIGIN] });
  try {
    await new Promise<void>((resolve, reject) => {
      let settled = false;
      const client = new WebSocket(wsUrl(server.port), {
        headers: { Origin: "https://evil.example" }
      });
      client.on("unexpected-response", (_request, response) => {
        settled = true;
        try {
          assert.equal(response.statusCode, 403);
        } catch (error) {
          reject(error as Error);
          client.terminate();
          return;
        }
        response.resume();
        client.terminate();
        resolve();
      });
      client.on("error", (error) => {
        if (!settled) reject(error);
      });
    });
  } finally {
    await server.close();
  }
});

test("closes 4408 when the hello frame never arrives", async () => {
  const server = await createAttachmentServer({ allowedOrigins: [ORIGIN], helloTimeoutMs: 100 });
  try {
    const client = connect(server.port);
    try {
      await waitOpen(client);
      const closed = await nextClose(client);
      assert.equal(closed.code, 4408);
    } finally {
      client.terminate();
    }
  } finally {
    await server.close();
  }
});

test("rejects a second socket for an active attachment", async () => {
  const server = await createAttachmentServer({ allowedOrigins: [ORIGIN], helloTimeoutMs: 1000 });
  try {
    server.onAttached((_attachmentId, port) => {
      port.start();
    });
    server.registerPending({ attachmentId: "att-3", token: TOKEN, ttlMs: 5000 });

    const first = connect(server.port);
    try {
      await waitOpen(first);
      first.send(JSON.stringify({ t: "hello", attachmentId: "att-3", token: TOKEN }));
      const attached = await nextMessage(first);
      assert.equal(attached.text, '{"t":"attached"}');

      const second = connect(server.port);
      try {
        await waitOpen(second);
        second.send(JSON.stringify({ t: "hello", attachmentId: "att-3", token: TOKEN }));
        const error = await nextMessage(second);
        assert.equal(JSON.parse(error.text).code, "attach_duplicate");
        const closed = await nextClose(second);
        assert.equal(closed.code, 4401);
      } finally {
        second.terminate();
      }
    } finally {
      first.terminate();
    }
  } finally {
    await server.close();
  }
});

test("closes 1013 and reports backpressure", async () => {
  const server = await createAttachmentServer({
    allowedOrigins: [ORIGIN],
    helloTimeoutMs: 1000,
    maxBufferedBytes: 4
  });
  try {
    const closed: { id: string; reason: string }[] = [];
    server.onClosed((attachmentId, reason) => {
      closed.push({ id: attachmentId, reason });
    });
    server.onAttached((_attachmentId, port) => {
      port.start();
      port.postMessage(new Uint8Array(64));
    });
    server.registerPending({ attachmentId: "att-4", token: TOKEN, ttlMs: 5000 });

    const client = connect(server.port);
    try {
      await waitOpen(client);
      client.send(JSON.stringify({ t: "hello", attachmentId: "att-4", token: TOKEN }));
      const attached = await nextMessage(client);
      assert.equal(attached.text, '{"t":"attached"}');
      const closedFrame = await nextClose(client);
      assert.equal(closedFrame.code, 1013);
    } finally {
      client.terminate();
    }

    await new Promise((resolve) => setTimeout(resolve, 50));
    assert.deepEqual(closed, [{ id: "att-4", reason: "backpressure" }]);
  } finally {
    await server.close();
  }
});

test("reports expired pending attachments", async () => {
  const server = await createAttachmentServer({ allowedOrigins: [ORIGIN] });
  try {
    const closed: { id: string; reason: string }[] = [];
    server.onClosed((attachmentId, reason) => {
      closed.push({ id: attachmentId, reason });
    });
    server.registerPending({ attachmentId: "att-5", token: TOKEN, ttlMs: 30 });
    await new Promise((resolve) => setTimeout(resolve, 120));
    assert.deepEqual(closed, [{ id: "att-5", reason: "expired" }]);
  } finally {
    await server.close();
  }
});

test("rejects a binary first frame", async () => {
  const server = await createAttachmentServer({ allowedOrigins: [ORIGIN], helloTimeoutMs: 1000 });
  try {
    const client = connect(server.port);
    try {
      await waitOpen(client);
      client.send(new Uint8Array([1, 2, 3]));
      const closed = await nextClose(client);
      assert.equal(closed.code, 4401);
    } finally {
      client.terminate();
    }
  } finally {
    await server.close();
  }
});
