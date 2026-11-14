// Local Anthropic-Messages stand-in for the Phase 7 performance gate (loopback only; never forwards
// anything). Same SSE wire format as ZCode's e2e replay server and the Phase 7 parity mock.
//
// A user prompt containing P7PERFQ<id> is answered by a stream whose first text delta is
// "P7PERFA<id> " (written together with message_start/content_block_start, timestamp recorded as
// firstDeltaAt), then more deltas GAP_MS apart, ending with "P7PERFZ<id>". Tokens have no
// underscores so Markdown emphasis cannot split them in the rendered timeline.
// Title requests and anything else get a short default text.
import { EventEmitter } from "node:events";
import http from "node:http";

const DELTAS = 4;
const GAP_MS = 30;

const now = () => performance.timeOrigin + performance.now();
const sse = (event, data) => `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`;
const usage = (input, output) => ({ input_tokens: input, cache_creation_input_tokens: 0, cache_read_input_tokens: 0, output_tokens: output });

function head(id) {
  return (
    sse("message_start", {
      type: "message_start",
      message: { id, type: "message", role: "assistant", model: "phase7-model", content: [], stop_reason: null, stop_sequence: null, usage: usage(32, 0) },
    }) + sse("content_block_start", { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } })
  );
}

const delta = (text) => sse("content_block_delta", { type: "content_block_delta", index: 0, delta: { type: "text_delta", text } });

function tail(outputTokens) {
  return (
    sse("content_block_stop", { type: "content_block_stop", index: 0 }) +
    sse("message_delta", { type: "message_delta", delta: { stop_reason: "end_turn", stop_sequence: null }, usage: usage(32, outputTokens) }) +
    sse("message_stop", { type: "message_stop" })
  );
}

function blockText(content) {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content.map((block) => (block?.type === "text" ? (block.text ?? "") : "")).join("\n");
}

function findMarker(messages) {
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (message?.role !== "user") continue;
    const matches = blockText(message.content).match(/P7PERFQ[A-Za-z0-9]+/g);
    if (matches?.length) return matches.at(-1);
  }
  return null;
}

export async function startMockProvider() {
  const events = new EventEmitter();
  const requests = [];
  const sockets = new Set();

  const server = http.createServer((request, response) => {
    const chunks = [];
    const arrivedAt = now();
    request.on("data", (chunk) => chunks.push(chunk));
    request.on("end", () => {
      const raw = Buffer.concat(chunks).toString("utf8");
      const record = { arrivedAt, bodyAt: now(), path: (request.url ?? "").split("?")[0], kind: "other", marker: null };
      requests.push(record);
      if (request.method !== "POST" || !record.path.includes("/messages")) {
        record.kind = "unhandled";
        response.writeHead(404, { "content-type": "application/json" });
        response.end('{"type":"error","error":{"type":"not_found_error","message":"phase7 perf mock"}}');
        return;
      }
      if (record.path.endsWith("/count_tokens")) {
        record.kind = "count_tokens";
        response.writeHead(200, { "content-type": "application/json" });
        response.end('{"input_tokens":32}');
        return;
      }
      let body = {};
      try {
        body = JSON.parse(raw);
      } catch {
        // 非 JSON 请求按默认回复处理。
      }
      const messages = Array.isArray(body.messages) ? body.messages : [];
      const isTitle = raw.includes("Generate a concise title");
      const marker = isTitle ? null : findMarker(messages);
      record.marker = marker;
      record.kind = isTitle ? "title" : marker ? "perf" : "default";
      const id = `msg_p7perf_${requests.length}`;
      const promptId = marker?.slice("P7PERFQ".length);
      const parts = marker
        ? [`P7PERFA${promptId} `, ...Array.from({ length: DELTAS - 1 }, (_, i) => `part${i} `), `P7PERFZ${promptId}`]
        : [isTitle ? JSON.stringify({ title: "Phase7 perf" }) : "P7PERF default reply"];

      if (body.stream === false) {
        response.writeHead(200, { "content-type": "application/json" });
        response.end(
          JSON.stringify({ id, type: "message", role: "assistant", model: "phase7-model", content: [{ type: "text", text: parts.join("") }], stop_reason: "end_turn", stop_sequence: null, usage: usage(32, 8) }),
        );
        record.completedAt = now();
        events.emit("completed", record);
        return;
      }
      response.writeHead(200, { "content-type": "text/event-stream; charset=utf-8", "cache-control": "no-cache", connection: "keep-alive" });
      response.write(head(id) + delta(parts[0]));
      record.firstDeltaAt = now();
      events.emit("first-delta", record);
      let index = 1;
      const next = () => {
        if (response.destroyed) return;
        if (index < parts.length) {
          response.write(delta(parts[index]));
          index += 1;
          setTimeout(next, GAP_MS);
          return;
        }
        response.end(tail(parts.length));
        record.completedAt = now();
        events.emit("completed", record);
      };
      setTimeout(next, GAP_MS);
    });
  });
  server.on("connection", (socket) => {
    sockets.add(socket);
    socket.once("close", () => sockets.delete(socket));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address();

  function waitFor(eventName, predicate, timeoutMs, label) {
    const existing = requests.find((record) => predicate(record) && (eventName === "completed" ? record.completedAt : record.firstDeltaAt));
    if (existing) return Promise.resolve(existing);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        events.off(eventName, onEvent);
        reject(new Error(`mock provider: no ${eventName} for ${label} within ${timeoutMs} ms`));
      }, timeoutMs);
      const onEvent = (record) => {
        if (!predicate(record)) return;
        clearTimeout(timer);
        events.off(eventName, onEvent);
        resolve(record);
      };
      events.on(eventName, onEvent);
    });
  }

  return {
    port,
    baseUrl: `http://127.0.0.1:${port}`,
    requests,
    waitFor,
    close() {
      for (const socket of sockets) socket.destroy();
      return new Promise((resolve) => server.close(() => resolve()));
    },
  };
}

/** Loopback stand-in for the ZCode cloud: every request gets 404; paths only are recorded. */
export async function startDeadCloud() {
  const paths = [];
  const sockets = new Set();
  const server = http.createServer((request, response) => {
    paths.push(`${request.method} ${request.url?.split("?")[0]}`);
    response.writeHead(404, { "content-type": "application/json" });
    response.end('{"code":404,"msg":"phase7 perf local fixture"}');
  });
  server.on("upgrade", (_request, socket) => socket.destroy());
  server.on("connection", (socket) => {
    sockets.add(socket);
    socket.once("close", () => sockets.delete(socket));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return {
    port: server.address().port,
    paths,
    close() {
      for (const socket of sockets) socket.destroy();
      return new Promise((resolve) => server.close(() => resolve()));
    },
  };
}
