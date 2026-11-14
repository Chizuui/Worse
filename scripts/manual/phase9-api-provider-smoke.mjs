// Manual smoke of harness-api-agent against a REAL OpenAI-compatible provider (phase-9-parity.md §9.7).
// The key comes only from the env var HARNESS_PROVIDER_API_KEY and is never printed.
// Usage: HARNESS_PROVIDER_API_KEY=... node scripts/manual/phase9-api-provider-smoke.mjs <baseUrl> <model>
// Uses a throw-away project dir and state dir that are deleted at the end; every permission is allowed.
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const [base, model] = process.argv.slice(2);
if (!base || !model || !process.env.HARNESS_PROVIDER_API_KEY) {
  console.error("usage: HARNESS_PROVIDER_API_KEY=... node phase9-api-provider-smoke.mjs <baseUrl> <model>");
  process.exit(2);
}
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const exe = path.join(root, "target", "debug", process.platform === "win32" ? "harness-api-agent.exe" : "harness-api-agent");
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "harness-provider-smoke-"));
const cwd = path.join(tmp, "project");
fs.mkdirSync(cwd);
fs.writeFileSync(path.join(cwd, "notes.txt"), "The secret word is PINEAPPLE.\n");

const child = spawn(exe, [], {
  env: { ...process.env, HARNESS_PROVIDER_BASE_URL: base, HARNESS_PROVIDER_MODEL: model, HARNESS_PROVIDER_LABEL: "Smoke", HARNESS_PROVIDER_STATE: path.join(tmp, "state") },
  stdio: ["pipe", "pipe", "pipe"],
  windowsHide: true,
});
let buf = "";
let id = 0;
const pending = new Map();
const updates = [];
child.stderr.on("data", (d) => process.stderr.write("[agent stderr] " + d));
child.stdout.on("data", (d) => {
  buf += d;
  let nl;
  while ((nl = buf.indexOf("\n")) >= 0) {
    const msg = JSON.parse(buf.slice(0, nl));
    buf = buf.slice(nl + 1);
    if (msg.method === "session/update") {
      const u = msg.params.update;
      updates.push(u);
      if (u.sessionUpdate === "tool_call") console.log(`  [tool] ${u.title}`);
      if (u.sessionUpdate === "tool_call_update" && (u.status === "completed" || u.status === "failed")) console.log(`  [tool] -> ${u.status}`);
    } else if (msg.method === "session/request_permission") {
      console.log(`  [permission] ${msg.params.toolCall.title} -> allow`);
      child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: msg.id, result: { outcome: { outcome: "selected", optionId: "allow" } } }) + "\n");
    } else if (pending.has(msg.id)) {
      pending.get(msg.id)(msg);
      pending.delete(msg.id);
    }
  }
});
const send = (method, params) =>
  new Promise((resolve) => {
    const mid = ++id;
    pending.set(mid, resolve);
    child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: mid, method, params }) + "\n");
  });
const reply = () => updates.filter((u) => u.sessionUpdate === "agent_message_chunk").map((u) => u.content.text).join("");

let ok = true;
try {
  await send("initialize", { protocolVersion: 1, clientCapabilities: {} });
  const sessionId = (await send("session/new", { cwd, mcpServers: [] })).result.sessionId;

  const t1 = Date.now();
  const r1 = await send("session/prompt", { sessionId, prompt: [{ type: "text", text: "Reply with exactly the word READY and nothing else." }] });
  console.log(`chat: ${JSON.stringify(r1.result ?? r1.error)} in ${Date.now() - t1} ms, reply=${JSON.stringify(reply().slice(0, 200))}`);
  if (r1.error || !/READY/i.test(reply())) ok = false;

  updates.length = 0;
  const t2 = Date.now();
  const r2 = await send("session/prompt", {
    sessionId,
    prompt: [{ type: "text", text: "Use your tools: read notes.txt, then create a file answer.txt whose only content is the secret word from notes.txt. Then reply DONE." }],
  });
  const answer = fs.existsSync(path.join(cwd, "answer.txt")) ? fs.readFileSync(path.join(cwd, "answer.txt"), "utf8").trim() : null;
  console.log(`tools: ${JSON.stringify(r2.result ?? r2.error)} in ${Date.now() - t2} ms, answer.txt=${JSON.stringify(answer)}, reply=${JSON.stringify(reply().slice(0, 200))}`);
  if (r2.error || !answer || !/PINEAPPLE/i.test(answer)) ok = false;
} catch (e) {
  console.error(e);
  ok = false;
} finally {
  child.stdin.end();
  setTimeout(() => {
    try { child.kill(); } catch {}
    fs.rmSync(tmp, { recursive: true, force: true });
    console.log(ok ? "PROVIDER_SMOKE_OK" : "PROVIDER_SMOKE_FAIL");
    process.exit(ok ? 0 : 1);
  }, 300);
}
