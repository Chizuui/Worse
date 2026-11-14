// Gate for harness-api-agent (phase-9-parity.md §9.7): a local mock OpenAI-compatible server drives the agent over
// ACP stdio. Covers streamed text + reasoning, write_file with a permission request, edit diff, run_command,
// readOnly refusal, MCP-free tool loop, 429/401/500 error mapping, cancel, and session/load after a restart.
// Usage: node scripts/e2e/phase9-api-agent.mjs [--no-build]
import { spawn, spawnSync } from "node:child_process";
import http from "node:http";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const exe = path.join(root, "target", "debug", process.platform === "win32" ? "harness-api-agent.exe" : "harness-api-agent");
const KEY = "test-key-0123456789";
const log = (...a) => console.log("[api-agent]", ...a);
const fail = (msg) => {
  console.error("[api-agent] FAIL", msg);
  process.exitCode = 1;
  throw new Error(msg);
};

if (!process.argv.includes("--no-build")) {
  const b = spawnSync("cargo", ["build", "-p", "harness-api-agent"], { cwd: root, stdio: "inherit" });
  if (b.status !== 0) fail("cargo build failed");
}

// ---- mock provider -------------------------------------------------------------------------------------------
const requests = [];
function sse(res, chunks) {
  res.writeHead(200, { "content-type": "text/event-stream" });
  for (const c of chunks) res.write(`data: ${JSON.stringify(c)}\n\n`);
  res.end("data: [DONE]\n\n");
}
const delta = (d, finish = null) => ({ choices: [{ index: 0, delta: d, finish_reason: finish }] });
function toolCall(name, args) {
  const json = JSON.stringify(args);
  const half = Math.floor(json.length / 2);
  // 参数分两段流式发送，验证累加。
  return [
    delta({ tool_calls: [{ index: 0, id: `call_${name}`, type: "function", function: { name, arguments: json.slice(0, half) } }] }),
    delta({ tool_calls: [{ index: 0, function: { arguments: json.slice(half) } }] }, "tool_calls"),
  ];
}
const server = http.createServer((req, res) => {
  let body = "";
  req.on("data", (d) => (body += d));
  req.on("end", () => {
    const parsed = JSON.parse(body || "{}");
    requests.push({ url: req.url, auth: req.headers.authorization, body: parsed });
    if (req.url !== "/v1/chat/completions") return res.writeHead(404).end("nope");
    if (req.headers.authorization !== `Bearer ${KEY}`) return res.writeHead(401).end('{"error":{"message":"bad key"}}');
    const msgs = parsed.messages;
    const last = msgs[msgs.length - 1];
    const lastUser = [...msgs].reverse().find((m) => m.role === "user")?.content ?? "";
    if (last.role === "tool") {
      return sse(res, [delta({ content: `tool said: ${String(last.content).slice(0, 200)}` }, "stop")]);
    }
    if (lastUser.includes("[hello]")) {
      return sse(res, [delta({ reasoning_content: "thinking..." }), delta({ content: "Hello " }), delta({ content: "world" }, "stop"), { choices: [], usage: { prompt_tokens: 11, completion_tokens: 2 } }]);
    }
    if (lastUser.includes("[write]")) return sse(res, toolCall("write_file", { path: "out/hello.txt", content: "one\ntwo\n" }));
    if (lastUser.includes("[edit]")) return sse(res, toolCall("edit_file", { path: "out/hello.txt", old_string: "two", new_string: "TWO" }));
    if (lastUser.includes("[run]")) return sse(res, toolCall("run_command", { command: "echo harness-run-ok" }));
    if (lastUser.includes("[read]")) return sse(res, toolCall("read_file", { path: "out/hello.txt" }));
    if (lastUser.includes("[search]")) return sse(res, toolCall("search", { pattern: "TWO" }));
    if (lastUser.includes("[quota]")) return res.writeHead(429).end('{"error":{"message":"quota"}}');
    if (lastUser.includes("[ratelimit]")) return res.writeHead(429, { "retry-after": "7" }).end("slow down");
    if (lastUser.includes("[server]")) return res.writeHead(503).end("upstream down");
    if (lastUser.includes("[nostream]")) {
      res.writeHead(200, { "content-type": "application/json" });
      return res.end(JSON.stringify({ choices: [{ message: { role: "assistant", content: "plain json reply" } }] }));
    }
    if (lastUser.includes("[slow]")) {
      res.writeHead(200, { "content-type": "text/event-stream" });
      let i = 0;
      const t = setInterval(() => {
        if (res.destroyed || i > 200) return clearInterval(t), res.end();
        res.write(`data: ${JSON.stringify(delta({ content: `s${i++} ` }))}\n\n`);
      }, 50);
      req.on("close", () => clearInterval(t));
      return;
    }
    return sse(res, [delta({ content: "ok" }, "stop")]);
  });
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const base = `http://127.0.0.1:${server.address().port}/v1`;

// ---- ACP driver ----------------------------------------------------------------------------------------------
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "harness-api-agent-"));
const cwd = path.join(tmp, "project");
const state = path.join(tmp, "state");
fs.mkdirSync(cwd, { recursive: true });

function startAgent(key = KEY) {
  const child = spawn(exe, [], {
    env: { ...process.env, HARNESS_PROVIDER_BASE_URL: base, HARNESS_PROVIDER_MODEL: "mock-model", HARNESS_PROVIDER_API_KEY: key, HARNESS_PROVIDER_LABEL: "Mock", HARNESS_PROVIDER_STATE: state },
    stdio: ["pipe", "pipe", "pipe"],
    windowsHide: true,
  });
  let buf = "";
  let stderr = "";
  const pending = new Map();
  const updates = [];
  const permissions = [];
  let id = 0;
  const agent = {
    child,
    updates,
    permissions,
    permissionAnswer: "allow",
    stderr: () => stderr,
    send(method, params) {
      const mid = ++id;
      child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: mid, method, params }) + "\n");
      return new Promise((resolve) => pending.set(mid, resolve));
    },
    notify(method, params) {
      child.stdin.write(JSON.stringify({ jsonrpc: "2.0", method, params }) + "\n");
    },
  };
  child.stderr.on("data", (d) => (stderr += d));
  child.stdout.on("data", (d) => {
    buf += d;
    let nl;
    while ((nl = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, nl);
      buf = buf.slice(nl + 1);
      if (!line.trim()) continue;
      const msg = JSON.parse(line);
      if (msg.method === "session/update") updates.push(msg.params.update);
      else if (msg.method === "session/request_permission") {
        permissions.push(msg.params);
        const answer = agent.permissionAnswer;
        child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: msg.id, result: { outcome: { outcome: "selected", optionId: answer } } }) + "\n");
      } else if (msg.id !== undefined && pending.has(msg.id)) {
        pending.get(msg.id)(msg);
        pending.delete(msg.id);
      }
    }
  });
  return agent;
}

const step = async (name, fn) => {
  const t = Date.now();
  await fn();
  log(`ok   ${name} (${Date.now() - t} ms)`);
};
const textOf = (updates, kind = "agent_message_chunk") => updates.filter((u) => u.sessionUpdate === kind).map((u) => u.content.text).join("");
const prompt = (agent, sessionId, text) => agent.send("session/prompt", { sessionId, prompt: [{ type: "text", text }] });

let agent = startAgent();
let sessionId;
try {
  await step("initialize + session/new", async () => {
    const init = await agent.send("initialize", { protocolVersion: 1, clientCapabilities: {} });
    if (!init.result?.agentCapabilities?.loadSession) fail("initialize: " + JSON.stringify(init));
    const created = await agent.send("session/new", { cwd, mcpServers: [] });
    sessionId = created.result?.sessionId;
    if (!sessionId) fail("session/new: " + JSON.stringify(created));
    if (created.result.models.currentModelId !== "mock-model") fail("model not advertised");
  });

  await step("streamed text, reasoning, usage, bearer key", async () => {
    agent.updates.length = 0;
    const r = await prompt(agent, sessionId, "[hello]");
    if (r.result?.stopReason !== "end_turn") fail("hello: " + JSON.stringify(r));
    if (textOf(agent.updates) !== "Hello world") fail("text: " + textOf(agent.updates));
    if (textOf(agent.updates, "agent_thought_chunk") !== "thinking...") fail("reasoning missing");
    if (r.result.usage?.inputTokens !== 11) fail("usage: " + JSON.stringify(r.result.usage));
    const req = requests.at(-1);
    if (req.auth !== `Bearer ${KEY}` || req.body.stream !== true || req.body.model !== "mock-model") fail("request shape");
    if (!req.body.tools?.some((t) => t.function.name === "write_file")) fail("tools not sent");
    if (req.body.messages[0].role !== "system" || !req.body.messages[0].content.includes(cwd)) fail("system prompt");
  });

  await step("write_file asks permission, writes, reports diff", async () => {
    agent.updates.length = 0;
    agent.permissions.length = 0;
    const r = await prompt(agent, sessionId, "[write]");
    if (r.result?.stopReason !== "end_turn") fail("write: " + JSON.stringify(r));
    if (agent.permissions.length !== 1) fail("expected 1 permission request, got " + agent.permissions.length);
    if (fs.readFileSync(path.join(cwd, "out", "hello.txt"), "utf8") !== "one\ntwo\n") fail("file not written");
    const done = agent.updates.find((u) => u.sessionUpdate === "tool_call_update" && u.status === "completed");
    if (!done?.content?.some((c) => c.type === "diff" && c.newText === "one\ntwo\n")) fail("diff missing");
    const toolMsg = requests.at(-1).body.messages.at(-1);
    if (toolMsg.role !== "tool" || toolMsg.tool_call_id !== "call_write_file") fail("tool result not sent back");
    if (!textOf(agent.updates).includes("tool said: wrote")) fail("final text");
  });

  await step("denied edit is not applied", async () => {
    agent.permissionAnswer = "deny";
    await prompt(agent, sessionId, "[edit]");
    if (fs.readFileSync(path.join(cwd, "out", "hello.txt"), "utf8") !== "one\ntwo\n") fail("denied edit applied");
    if (!String(requests.at(-1).body.messages.at(-1).content).includes("denied")) fail("denial not reported to model");
    agent.permissionAnswer = "allow";
  });

  await step("acceptEdits: edit inside cwd without asking", async () => {
    const m = await agent.send("session/set_mode", { sessionId, modeId: "acceptEdits" });
    if (m.error) fail("set_mode: " + JSON.stringify(m));
    agent.permissions.length = 0;
    await prompt(agent, sessionId, "[edit]");
    if (agent.permissions.length !== 0) fail("acceptEdits still asked");
    if (fs.readFileSync(path.join(cwd, "out", "hello.txt"), "utf8") !== "one\nTWO\n") fail("edit not applied");
  });

  await step("run_command asks (allow always) and returns output", async () => {
    agent.permissions.length = 0;
    agent.permissionAnswer = "allow-always";
    await prompt(agent, sessionId, "[run]");
    if (agent.permissions.length !== 1) fail("run did not ask");
    if (!String(requests.at(-1).body.messages.at(-1).content).includes("harness-run-ok")) fail("command output: " + requests.at(-1).body.messages.at(-1).content);
    agent.permissions.length = 0;
    await prompt(agent, sessionId, "[run]");
    if (agent.permissions.length !== 0) fail("always-allow not remembered");
    agent.permissionAnswer = "allow";
  });

  await step("read_file + search", async () => {
    await prompt(agent, sessionId, "[read]");
    if (!String(requests.at(-1).body.messages.at(-1).content).includes("2\tTWO")) fail("read output");
    await prompt(agent, sessionId, "[search]");
    if (!/hello\.txt:2: TWO/.test(String(requests.at(-1).body.messages.at(-1).content))) fail("search output: " + requests.at(-1).body.messages.at(-1).content);
  });

  await step("readOnly hides and refuses write tools", async () => {
    await agent.send("session/set_mode", { sessionId, modeId: "readOnly" });
    await prompt(agent, sessionId, "[write]");
    const req = requests.at(-2);
    if (req.body.tools.some((t) => t.function.name === "write_file")) fail("write tool offered in readOnly");
    if (!String(requests.at(-1).body.messages.at(-1).content).includes("read-only")) fail("readOnly not refused");
    await agent.send("session/set_mode", { sessionId, modeId: "default" });
  });

  await step("non-streamed JSON reply", async () => {
    agent.updates.length = 0;
    await prompt(agent, sessionId, "[nostream]");
    if (textOf(agent.updates) !== "plain json reply") fail("nostream: " + textOf(agent.updates));
  });

  await step("error mapping: 429 quota, 429 short, 503, 401", async () => {
    const q = await prompt(agent, sessionId, "[quota]");
    if (q.error?.data?.harnessErrorKind !== "quota_exhausted" || !(q.error.data.resetsAt > Date.now() / 1000)) fail("quota: " + JSON.stringify(q));
    const s = await prompt(agent, sessionId, "[ratelimit]");
    if (s.error?.data?.harnessErrorKind !== "transient" || s.error.data.retryAfterSecs !== 7) fail("ratelimit: " + JSON.stringify(s));
    const u = await prompt(agent, sessionId, "[server]");
    if (u.error?.data?.harnessErrorKind !== "transient") fail("server: " + JSON.stringify(u));
    const bad = startAgent("wrong-key-000000");
    await bad.send("initialize", { protocolVersion: 1, clientCapabilities: {} });
    const s2 = (await bad.send("session/new", { cwd, mcpServers: [] })).result.sessionId;
    const a = await prompt(bad, s2, "[hello]");
    if (a.error?.code !== -32000) fail("auth: " + JSON.stringify(a));
    if (JSON.stringify(a).includes("wrong-key-000000")) fail("key echoed in error");
    bad.child.stdin.end();
  });

  await step("cancel stops the stream", async () => {
    const p = prompt(agent, sessionId, "[slow]");
    await new Promise((r) => setTimeout(r, 400));
    agent.notify("session/cancel", { sessionId });
    const r = await p;
    if (r.result?.stopReason !== "cancelled") fail("cancel: " + JSON.stringify(r));
  });

  await step("session/load after restart replays the conversation", async () => {
    agent.child.stdin.end();
    await new Promise((r) => agent.child.on("exit", r));
    agent = startAgent();
    await agent.send("initialize", { protocolVersion: 1, clientCapabilities: {} });
    const loaded = await agent.send("session/load", { sessionId, cwd, mcpServers: [] });
    if (loaded.error) fail("load: " + JSON.stringify(loaded));
    if (!agent.updates.some((u) => u.sessionUpdate === "user_message_chunk" && u.content.text === "[hello]")) fail("user text not replayed");
    if (!textOf(agent.updates).includes("Hello world")) fail("agent text not replayed");
    const bad = await agent.send("session/load", { sessionId: "../../evil", cwd, mcpServers: [] });
    if (!bad.error) fail("path traversal id accepted");
  });

  await step("session file never contains the key", async () => {
    const dir = path.join(state, "sessions");
    for (const f of fs.readdirSync(dir)) if (fs.readFileSync(path.join(dir, f), "utf8").includes(KEY)) fail("key in " + f);
  });
  console.log("PHASE9_API_AGENT_OK");
} catch (e) {
  if (!process.exitCode) {
    console.error("[api-agent] FAIL", e?.stack || e);
    process.exitCode = 1;
  }
  console.error("[api-agent] agent stderr:", agent.stderr());
} finally {
  agent.child.stdin.end();
  server.close();
  setTimeout(() => {
    try { agent.child.kill(); } catch {}
    fs.rmSync(tmp, { recursive: true, force: true });
    process.exit(process.exitCode ?? 0);
  }, 300);
}
