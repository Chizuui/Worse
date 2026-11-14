// Local Anthropic-Messages compatible LLM stand-in for the Phase 7 parity gate.
//
// It speaks the same SSE wire format as ZCode's own e2e replay server
// (packages/desktop/test/e2e/helpers/upstream-replay-server.ts: message_start, content_block_*,
// message_delta, message_stop) and answers by a marker in the conversation, so Harness and ZCode get
// byte-identical responses. Loopback only; never forwards anything to a real provider.
//
// Markers (in the user prompt):
//   P7_HELLO_<id>   streamed text in several timed SSE events ("PHASE7_REPLY_<id> ...")
//   P7_SLOW_<id>    long stream (one event every SLOW_INTERVAL_MS) meant to be stopped mid-stream
//   P7_BUSY_<id>    ~3 s stream ending in "PHASE7_BUSY_DONE_<id>" (keeps the session busy for queueing)
//   P7_QUEUED_<id>  quick text reply ("PHASE7_QUEUED_DONE_<id>")
//   P7_MD_<id>      a markdown reply (heading, list, inline code, fenced Rust code, table) for visual references
//   P7_WRITE_<id>   a Write tool_use planned with plan(); after the tool_result: "PHASE7_WRITE_DONE_<id> error=<bool>"
// Title requests ("Generate a concise title") get {"title": ...}; anything else a short default text.
import { EventEmitter } from "node:events";
import http from "node:http";

const SLOW_INTERVAL_MS = 150;
const SLOW_EVENTS = 400;
const BUSY_INTERVAL_MS = 100;
const BUSY_EVENTS = 50;
const HELLO_PARTS = ["PHASE7_REPLY_", "{id} ", "streamed ", "in ", "five ", "parts."];
const HELLO_GAP_MS = 60;
// Visual reference reply (phase8-ui compare screenshots): the same text the Harness fake agent renders.
const MARKDOWN_REPLY = [
  "## Plan\n\nI checked `src/main.rs` and the build. Steps:\n\n",
  "1. Parse the config\n2. Start the **hub**\n3. Report the result\n\n",
  "```rust\nfn main() {\n    // start the hub\n    let port = 8080;\n    println!(\"listening on {port}\");\n}\n```\n\n",
  "| Agent | State | Resets |\n| --- | --- | --- |\n| Claude Code | available | - |\n| Codex | limit reached | 18:00 |\n",
];

function sse(event, data) {
  return `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`;
}

const usage = (input, output) => ({ input_tokens: input, cache_creation_input_tokens: 0, cache_read_input_tokens: 0, output_tokens: output });

function messageStart(id) {
  return sse("message_start", {
    type: "message_start",
    message: { id, type: "message", role: "assistant", model: "phase7-model", content: [], stop_reason: null, stop_sequence: null, usage: usage(32, 0) },
  });
}

function messageEnd(stopReason, outputTokens) {
  return [
    sse("message_delta", { type: "message_delta", delta: { stop_reason: stopReason, stop_sequence: null }, usage: usage(32, outputTokens) }),
    sse("message_stop", { type: "message_stop" }),
  ];
}

function textEvents(id, parts) {
  return [
    messageStart(id),
    sse("content_block_start", { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } }),
    ...parts.map((text) => sse("content_block_delta", { type: "content_block_delta", index: 0, delta: { type: "text_delta", text } })),
    sse("content_block_stop", { type: "content_block_stop", index: 0 }),
    ...messageEnd("end_turn", parts.length),
  ];
}

function toolUseEvents(id, toolUse) {
  return [
    messageStart(id),
    sse("content_block_start", { type: "content_block_start", index: 0, content_block: { type: "tool_use", id: toolUse.id, name: toolUse.name, input: {} } }),
    sse("content_block_delta", { type: "content_block_delta", index: 0, delta: { type: "input_json_delta", partial_json: JSON.stringify(toolUse.input) } }),
    sse("content_block_stop", { type: "content_block_stop", index: 0 }),
    ...messageEnd("tool_use", 16),
  ];
}

function nonStreamText(id, text) {
  return { id, type: "message", role: "assistant", model: "phase7-model", content: [{ type: "text", text }], stop_reason: "end_turn", stop_sequence: null, usage: usage(32, 8) };
}

function blockText(content) {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content.map((block) => (block?.type === "text" ? block.text ?? "" : "")).join("\n");
}

function toolResults(content) {
  if (!Array.isArray(content)) return [];
  return content.filter((block) => block?.type === "tool_result");
}

/** The marker the conversation is about: the newest P7_* token in a user text block. */
function findMarker(messages) {
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (message?.role !== "user") continue;
    const matches = blockText(message.content).match(/P7_[A-Z]+_[A-Za-z0-9]+/g);
    if (matches?.length) return matches.at(-1);
  }
  return null;
}

export async function startMockProvider() {
  const events = new EventEmitter();
  const requests = [];
  const plans = new Map();
  const sockets = new Set();

  const server = http.createServer((request, response) => {
    const chunks = [];
    request.on("data", (chunk) => chunks.push(chunk));
    request.on("end", () => {
      const raw = Buffer.concat(chunks).toString("utf8");
      const record = { at: Date.now(), method: request.method, path: (request.url ?? "").split("?")[0], marker: null, kind: "other", aborted: false, completed: false };
      requests.push(record);
      if (request.method !== "POST" || !record.path.includes("/messages")) {
        record.kind = "unhandled";
        response.writeHead(404, { "content-type": "application/json" });
        response.end('{"type":"error","error":{"type":"not_found_error","message":"phase7 mock"}}');
        return;
      }
      if (record.path.endsWith("/count_tokens")) {
        record.kind = "count_tokens";
        response.writeHead(200, { "content-type": "application/json" });
        response.end('{"input_tokens":32}');
        return;
      }
      let body;
      try {
        body = JSON.parse(raw);
      } catch {
        body = {};
      }
      const messages = Array.isArray(body.messages) ? body.messages : [];
      const marker = findMarker(messages);
      record.marker = marker;
      const last = messages.at(-1);
      const results = last?.role === "user" ? toolResults(last.content) : [];
      const isTitle = raw.includes("Generate a concise title");
      const stream = body.stream !== false;
      const id = `msg_phase7_${requests.length}`;

      let plan;
      if (isTitle) {
        record.kind = "title";
        plan = { text: [JSON.stringify({ title: marker ? `Phase7 ${marker}` : "Phase7" })] };
      } else if (results.length > 0) {
        const result = results.at(-1);
        record.kind = "tool_result";
        record.toolUseId = result.tool_use_id;
        record.toolResultIsError = Boolean(result.is_error);
        const doneId = marker?.replace(/^P7_[A-Z]+_/, "") ?? "unknown";
        plan = { text: [`PHASE7_WRITE_DONE_${doneId} error=${record.toolResultIsError}`] };
      } else if (marker?.startsWith("P7_HELLO_")) {
        record.kind = "hello";
        const helloId = marker.slice("P7_HELLO_".length);
        plan = { text: HELLO_PARTS.map((part) => part.replace("{id}", helloId)), gapMs: HELLO_GAP_MS };
      } else if (marker?.startsWith("P7_SLOW_")) {
        record.kind = "slow";
        const slowId = marker.slice("P7_SLOW_".length);
        plan = { text: Array.from({ length: SLOW_EVENTS }, (_, index) => `slow${slowId}-${index} `), gapMs: SLOW_INTERVAL_MS };
      } else if (marker?.startsWith("P7_BUSY_")) {
        record.kind = "busy";
        const busyId = marker.slice("P7_BUSY_".length);
        plan = { text: [...Array.from({ length: BUSY_EVENTS }, (_, index) => `busy-${index} `), `PHASE7_BUSY_DONE_${busyId}`], gapMs: BUSY_INTERVAL_MS };
      } else if (marker?.startsWith("P7_MD_")) {
        record.kind = "markdown";
        plan = { text: MARKDOWN_REPLY, gapMs: HELLO_GAP_MS };
      } else if (marker?.startsWith("P7_QUEUED_")) {
        record.kind = "queued";
        plan = { text: [`PHASE7_QUEUED_DONE_${marker.slice("P7_QUEUED_".length)}`] };
      } else if (marker && plans.has(marker)) {
        record.kind = "tool_use";
        plan = { toolUse: plans.get(marker) };
      } else {
        record.kind = "default";
        plan = { text: ["PHASE7_DEFAULT_REPLY"] };
      }
      events.emit("request", record);

      if (!stream) {
        response.writeHead(200, { "content-type": "application/json" });
        response.end(JSON.stringify(nonStreamText(id, (plan.text ?? ["PHASE7_DEFAULT_REPLY"]).join(""))));
        record.completed = true;
        events.emit("completed", record);
        return;
      }
      const sseEvents = plan.toolUse ? toolUseEvents(id, plan.toolUse) : textEvents(id, plan.text);
      response.writeHead(200, { "content-type": "text/event-stream; charset=utf-8", "cache-control": "no-cache", connection: "keep-alive" });
      let index = 0;
      let timer;
      const finish = () => {
        clearTimeout(timer);
        if (!record.completed) {
          record.completed = true;
          record.completedAt = Date.now();
          events.emit("completed", record);
        }
        response.end();
      };
      response.on("close", () => {
        clearTimeout(timer);
        if (!record.completed) {
          record.aborted = true;
          record.abortedAt = Date.now();
          record.abortedAfterEvents = index;
          record.totalEvents = sseEvents.length;
          events.emit("aborted", record);
        }
      });
      const writeNext = () => {
        if (response.destroyed) return;
        // 头 3 个事件（message_start + content_block_start + 首个 delta）立即写出，之后按 gap 节奏输出。
        response.write(sseEvents[index]);
        index += 1;
        if (index === 3) {
          record.streaming = true;
          events.emit("streaming", record);
        }
        if (index >= sseEvents.length) {
          finish();
          return;
        }
        const gap = plan.gapMs && index >= 3 && index < sseEvents.length - 3 ? plan.gapMs : 0;
        if (gap > 0) timer = setTimeout(writeNext, gap);
        else setImmediate(writeNext);
      };
      writeNext();
    });
  });
  server.on("connection", (socket) => {
    sockets.add(socket);
    socket.once("close", () => sockets.delete(socket));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address();

  /** Resolves with the first request record (past or future) matching `predicate`. */
  function waitFor(eventName, predicate, timeoutMs, label) {
    const reached = { request: () => true, streaming: (record) => record.streaming, completed: (record) => record.completed, aborted: (record) => record.aborted }[eventName];
    // 事件可能在调用方开始等待之前就已发生（例如 send() 返回时首批 SSE 已写出），先查已记录的状态。
    const existing = requests.find((record) => predicate(record) && reached(record));
    if (existing) return Promise.resolve(existing);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        events.off(eventName, onEvent);
        reject(new Error(`mock provider: timed out after ${timeoutMs} ms waiting for ${eventName} ${label}`));
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
    events,
    /** Registers the tool_use answered for `marker` (e.g. a Write that needs permission). */
    plan(marker, toolUse) {
      plans.set(marker, toolUse);
    },
    waitFor,
    /** Requests that belong to the main conversation (no title / count_tokens side calls). */
    mainRequests() {
      return requests.filter((record) => !["title", "count_tokens", "unhandled", "other"].includes(record.kind));
    },
    close() {
      for (const socket of sockets) socket.destroy();
      return new Promise((resolve) => server.close(() => resolve()));
    },
  };
}
