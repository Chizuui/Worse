#!/usr/bin/env node
// Phase 9.9 single-executable gate. Spec: Worse/docs/specs/harness/phase-9-parity.md §9.9 (Verification).
//
//   node Worse/scripts/e2e/phase9-single-exe.mjs
//   PHASE9_SINGLE_SKIP_BUILD=1   reuse the last build in Worse/.work/tmp/phase9-single-bin
//
// The app directory holds ONLY harness-slint.exe (the release automation build under the product name). No
// HARNESS_MEMORY_MCP / HARNESS_API_AGENT is set and PATH has no Harness build directory. The gate proves:
//   1. the memory and harness-delegate MCP entries Harness hands an agent point at harness-slint.exe itself;
//   2. those entries work when spawned exactly as given (GUI-subsystem exe, piped stdio): memory_write/read and
//      a delegate call;
//   3. the delegate call runs an API provider agent, which the hub starts as `harness-slint.exe --api-agent`
//      against a local mock OpenAI-compatible server;
//   4. no process from a standalone helper binary appears in the app's process tree.
// Prints PHASE9_SINGLE_EXE_OK or PHASE9_SINGLE_EXE_FAIL <step> <reason> as the last line; report in
// Worse/.work/phase9/phase9-single-exe.json.
//
// Safety: installIsolatedHome() runs first; HARNESS_HOME, the workspace and providers.json (dummy key) live in the
// isolated home. The parent agent is harness-fake-agent. The mock provider listens on 127.0.0.1 only. Only process
// trees rooted at PIDs this gate spawned are inspected or stopped. The delegate token is never printed.
import { installIsolatedHome } from "./lib/isolatedHome.mjs";

const { home } = installIsolatedHome("phase9-single-exe");

const { spawn, spawnSync } = await import("node:child_process");
const { copyFileSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } = await import("node:fs");
const http = await import("node:http");
const net = await import("node:net");
const os = (await import("node:os")).default;
const path = (await import("node:path")).default;
const process = (await import("node:process")).default;
const readline = await import("node:readline");
const { fileURLToPath } = await import("node:url");
const { SlintMcp, byLabelPrefix, label, sleep } = await import("./phase8-ui/mcp.mjs");
const { killTrees, processTree, stillAlive } = await import("./phase7-perf/procs.mjs");

const worse = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const reportDir = path.join(worse, ".work/phase9");
const binDir = path.join(worse, ".work/tmp/phase9-single-bin");
const appDir = path.join(binDir, "app");
const toolsDir = path.join(binDir, "tools");
const exeName = (name) => (process.platform === "win32" ? `${name}.exe` : name);
const appExe = path.join(appDir, exeName("harness-slint"));
const KEY_ENTER = "\n";
const MOCK_KEY = "gate-dummy-key";

const harnessHome = path.join(home, ".harness");
const workspace = path.join(home, "workspace");
const fakeState = path.join(home, "fake-agent-state");
const fakeLog = path.join(fakeState, "log.jsonl");
const roots = new Set();
const results = { startedAt: new Date().toISOString(), steps: [] };
let currentMcp = null;

class StepError extends Error {
  constructor(step, message) {
    super(message);
    this.step = step;
  }
}

const log = (line) => process.stdout.write(`[phase9-single] ${line}\n`);

async function step(name, fn) {
  const started = Date.now();
  try {
    const value = await fn();
    results.steps.push({ name, ok: true, ms: Date.now() - started });
    log(`ok   ${name} (${Date.now() - started} ms)`);
    return value;
  } catch (error) {
    results.steps.push({ name, ok: false, ms: Date.now() - started, error: String(error?.message ?? error) });
    if (currentMcp) {
      results.failureLabels = await currentMcp
        .elements()
        .then((list) => list.map((e) => `${e.accessibleRole ?? ""}:${label(e)}`).filter((l) => l.length > 1).slice(0, 200))
        .catch((e) => [String(e?.message ?? e)]);
    }
    throw error instanceof StepError ? error : new StepError(name, error?.message ?? String(error));
  }
}

// ---- build + fixtures --------------------------------------------------------------------------------------

function cargo(args) {
  // 隔离 home 改写了 USERPROFILE；cargo/rustup 需要真实账户下的工具链目录（只构建，不运行 Harness）。
  const accountHome = os.userInfo().homedir;
  const env = {
    ...process.env,
    CARGO_HOME: process.env.CARGO_HOME ?? path.join(accountHome, ".cargo"),
    RUSTUP_HOME: process.env.RUSTUP_HOME ?? path.join(accountHome, ".rustup"),
  };
  const result = spawnSync("cargo", args, { cwd: worse, env, stdio: ["ignore", "inherit", "inherit"], windowsHide: true });
  if (result.status !== 0) throw new Error(`cargo ${args.join(" ")} exited ${String(result.status)}`);
}

function build() {
  const release = path.join(process.env.CARGO_TARGET_DIR ? path.resolve(process.env.CARGO_TARGET_DIR) : path.join(worse, "target"), "release");
  if (process.env.PHASE9_SINGLE_SKIP_BUILD !== "1") {
    cargo(["build", "--release", "-p", "harness-fake-agent"]);
    cargo(["build", "--release", "-p", "harness-slint", "--features", "automation"]);
    // 应用目录只放一个文件：产品名 harness-slint.exe。
    rmSync(appDir, { recursive: true, force: true });
    mkdirSync(appDir, { recursive: true });
    mkdirSync(toolsDir, { recursive: true });
    copyFileSync(path.join(release, exeName("harness-slint")), appExe);
    copyFileSync(path.join(release, exeName("harness-fake-agent")), path.join(toolsDir, exeName("harness-fake-agent")));
  }
  const listing = readdirSync(appDir);
  if (listing.length !== 1 || listing[0] !== exeName("harness-slint")) throw new Error(`app dir must hold only harness-slint: ${listing.join(", ")}`);
  if (!existsSync(path.join(toolsDir, exeName("harness-fake-agent")))) throw new Error("harness-fake-agent missing");
  results.appDir = listing;
}

function fixtures(baseUrl) {
  mkdirSync(workspace, { recursive: true });
  writeFileSync(path.join(workspace, "README.md"), "# demo workspace\n");
  mkdirSync(fakeState, { recursive: true });
  mkdirSync(harnessHome, { recursive: true });
  writeFileSync(path.join(harnessHome, "settings.json"), `${JSON.stringify({ appearance: { theme: "light" }, setup: { firstRunDone: true } }, null, 2)}\n`);
  writeFileSync(
    path.join(harnessHome, "providers.json"),
    `${JSON.stringify({ providers: [{ id: "api-mock", label: "Mock", baseUrl, model: "mock-model", apiKey: MOCK_KEY }] }, null, 2)}\n`,
  );
  const agent = (id, name) => ({
    id,
    name,
    program: path.join(toolsDir, exeName("harness-fake-agent")),
    args: ["--name", id],
    env: { FAKE_AGENT_STATE: fakeState, FAKE_AGENT_LOG: fakeLog },
  });
  const file = path.join(home, "ui-agents.json");
  writeFileSync(file, JSON.stringify([agent("claude-code", "Claude Code"), agent("codex", "Codex")], null, 2));
  return file;
}

/** Local OpenAI-compatible mock: every chat completion streams API-ANSWER. */
async function mockProvider() {
  const calls = [];
  const server = http.createServer((req, res) => {
    let body = "";
    req.on("data", (d) => (body += d));
    req.on("end", () => {
      calls.push({ url: req.url, authorized: req.headers.authorization === `Bearer ${MOCK_KEY}` });
      if (req.url !== "/v1/chat/completions") return res.writeHead(404).end("nope");
      if (req.headers.authorization !== `Bearer ${MOCK_KEY}`) return res.writeHead(401).end('{"error":{"message":"bad key"}}');
      res.writeHead(200, { "content-type": "text/event-stream" });
      res.write(`data: ${JSON.stringify({ choices: [{ index: 0, delta: { content: "API-ANSWER from mock" }, finish_reason: "stop" }] })}\n\n`);
      res.end("data: [DONE]\n\n");
    });
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return { server, calls, baseUrl: `http://127.0.0.1:${server.address().port}/v1` };
}

function freePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      server.close(() => resolve(port));
    });
  });
}

/** PATH without any directory that holds a Harness build (target/, .work/tmp). */
function cleanPath() {
  const parts = String(process.env.PATH ?? "").split(path.delimiter);
  const bad = (p) => /[\\/](target|\.work)[\\/]?/i.test(p) || existsSync(path.join(p, exeName("harness-memory-mcp"))) || existsSync(path.join(p, exeName("harness-api-agent")));
  return parts.filter((p) => p && !bad(p)).join(path.delimiter);
}

async function launch(agentsFile) {
  const env = { ...process.env, HARNESS_HOME: harnessHome, HARNESS_WORKSPACE: workspace, HARNESS_UI_AGENTS: agentsFile, GIT_CEILING_DIRECTORIES: home, PATH: cleanPath() };
  for (const key of ["SLINT_MCP_PORT", "SLINT_BACKEND", "HARNESS_UI_CORE", "HARNESS_UI_WINDOW_SIZE", "HARNESS_MEMORY_MCP", "HARNESS_API_AGENT", "HARNESS_NPM"]) delete env[key];
  const port = await freePort();
  env.SLINT_MCP_PORT = String(port);
  // Live update: the gate tracks this PID and its stderr, so run in place (no shadow copy, no parent exit).
  env.HARNESS_NO_SHADOW = "1";
  const child = spawn(appExe, [], { cwd: workspace, env, stdio: ["ignore", "ignore", "pipe"], windowsHide: false });
  const stderr = [];
  child.stderr.setEncoding("utf8").on("data", (chunk) => stderr.push(chunk));
  roots.add(child.pid);
  const exited = new Promise((resolve) => child.once("exit", (code) => resolve(code)));
  const app = { child, pid: child.pid, exited, stderr, env, mcp: new SlintMcp(port) };
  await app.mcp.connect();
  currentMcp = app.mcp;
  return app;
}

async function close(app) {
  const tree = await processTree([app.pid]).catch(() => []);
  spawnSync("taskkill", ["/PID", String(app.pid)], { stdio: "ignore", windowsHide: true });
  const graceful = await Promise.race([app.exited.then(() => true), sleep(15_000).then(() => false)]);
  if (!graceful) await killTrees([app.pid]);
  await sleep(500);
  const alive = await stillAlive(tree);
  if (alive.length > 0) {
    await killTrees(alive.map((p) => p.pid));
    throw new Error(`processes left after close: ${alive.map((p) => `${p.name}#${p.pid}`).join(", ")}`);
  }
  roots.delete(app.pid);
  if (currentMcp === app.mcp) currentMcp = null;
  return { graceful, treeSize: tree.length };
}

const isRole = (e, role) => String(e.accessibleRole ?? "").toLowerCase() === role;
const isButton = (text) => (e) => label(e) === text && isRole(e, "button");
const press = async (mcp, text) => mcp.activate(await mcp.waitFor(`button "${text}"`, isButton(text)));

async function send(mcp, text) {
  const input = await mcp.waitFor("message input", (e) => label(e) === "Message" && isRole(e, "textinput"));
  await mcp.click(input);
  await mcp.setValue(input, text);
  await sleep(200);
  await mcp.key(KEY_ENTER);
  return mcp.waitFor(`reply to ${text}`, byLabelPrefix("Claude Code: ", `reply to: ${text}`), 30_000);
}

// ---- MCP entries as an agent would run them --------------------------------------------------------------------

function mcpServers() {
  if (!existsSync(fakeLog)) return [];
  const lines = readFileSync(fakeLog, "utf8").split("\n").filter((l) => l.trim());
  for (const line of lines.reverse()) {
    try {
      const entry = JSON.parse(line);
      if (Array.isArray(entry.mcpServers) && entry.mcpServers.length > 0) return entry.mcpServers;
    } catch {
      // ignore partial lines
    }
  }
  return [];
}

const samePath = (a, b) => path.resolve(String(a)).toLowerCase() === path.resolve(String(b)).toLowerCase();

/** Spawns one MCP server entry exactly as given (command, args, env) with piped stdio. */
class McpClient {
  constructor(entry, baseEnv) {
    const env = { ...baseEnv, ...Object.fromEntries((entry.env ?? []).map((v) => [v.name, v.value])) };
    this.child = spawn(entry.command, entry.args ?? [], { env, stdio: ["pipe", "pipe", "pipe"], windowsHide: true });
    this.stderr = [];
    this.child.stderr.setEncoding("utf8").on("data", (c) => this.stderr.push(c));
    roots.add(this.child.pid);
    this.next = 1;
    this.waiting = new Map();
    readline.createInterface({ input: this.child.stdout }).on("line", (line) => {
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        return;
      }
      const resolve = this.waiting.get(msg.id);
      if (resolve) {
        this.waiting.delete(msg.id);
        resolve(msg);
      }
    });
  }

  request(method, params, timeoutMs = 30_000) {
    const id = this.next++;
    const reply = new Promise((resolve, reject) => {
      this.waiting.set(id, resolve);
      setTimeout(() => reject(new Error(`${method} timed out; stderr: ${this.stderr.join("").slice(0, 300)}`)), timeoutMs).unref();
    });
    this.child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
    return reply;
  }

  async init() {
    const reply = await this.request("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "gate", version: "1" } });
    this.child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" })}\n`);
    return reply;
  }

  async call(name, args, timeoutMs) {
    const reply = await this.request("tools/call", { name, arguments: args }, timeoutMs);
    return { text: reply.result?.content?.[0]?.text ?? "", isError: reply.result?.isError === true };
  }

  async stop() {
    this.child.stdin.end();
    const code = await Promise.race([new Promise((r) => this.child.once("exit", (c) => r(c))), sleep(5000).then(() => "timeout")]);
    roots.delete(this.child.pid);
    return code;
  }
}

// ---- scenario ------------------------------------------------------------------------------------------------

async function scenario(agentsFile, mock) {
  const app = await step("start harness-slint.exe alone", () => launch(agentsFile));
  const { mcp } = app;
  try {
    await step("parent session on Claude Code", async () => {
      await press(mcp, "New session");
      await send(mcp, "hello single exe");
    });
    const servers = await step("MCP entries point at harness-slint.exe", async () => {
      const list = mcpServers();
      const memory = list.find((s) => s.name === "harness-memory");
      const delegate = list.find((s) => s.name === "harness-delegate");
      if (!memory || !delegate) throw new Error(`mcpServers: ${list.map((s) => s.name).join(", ")}`);
      for (const [entry, flag] of [[memory, "--memory-mcp"], [delegate, "--delegate"]]) {
        if (!samePath(entry.command, appExe)) throw new Error(`${entry.name} command ${entry.command}`);
        if ((entry.args ?? [])[0] !== flag) throw new Error(`${entry.name} args ${JSON.stringify(entry.args)}`);
      }
      results.entries = { memory: { command: path.basename(memory.command), args: memory.args.map((a) => (a.startsWith("-") ? a : "<path>")) }, delegate: { command: path.basename(delegate.command), args: delegate.args } };
      return { memory, delegate };
    });
    await step("memory MCP via harness-slint.exe --memory-mcp", async () => {
      const client = new McpClient(servers.memory, app.env);
      try {
        const init = await client.init();
        if (init.result?.serverInfo?.name !== "harness-memory") throw new Error(`initialize: ${JSON.stringify(init).slice(0, 200)}`);
        const write = await client.call("memory_write", { scope: "project", content: "single-exe-note", mode: "append" });
        if (write.isError) throw new Error(`memory_write: ${write.text}`);
        const read = await client.call("memory_read", { scope: "project" });
        if (read.isError || !read.text.includes("single-exe-note")) throw new Error(`memory_read: ${read.text.slice(0, 200)}`);
      } finally {
        results.memoryExit = await client.stop();
      }
      if (!existsSync(path.join(workspace, ".harness", "MEMORY.md"))) throw new Error("project MEMORY.md not written");
    });
    await step("delegate MCP runs the API provider agent", async () => {
      const client = new McpClient(servers.delegate, app.env);
      try {
        await client.init();
        const agents = await client.call("list_agents", {});
        if (agents.isError || !agents.text.includes("api-mock")) throw new Error(`list_agents: ${agents.text.slice(0, 300)}`);
        const pending = client.call("delegate", { task: "say the answer", agent: "api-mock", title: "API child" }, 90_000);
        // 运行期间检查进程树：API 代理必须是 harness-slint.exe --api-agent，且没有独立 helper 二进制。
        let sawApiAgent = false;
        const deadline = Date.now() + 30_000;
        while (!sawApiAgent && Date.now() < deadline) {
          const tree = await processTree([app.pid]).catch(() => []);
          const helpers = tree.filter((p) => /harness-(memory-mcp|api-agent)/i.test(String(p.name)));
          if (helpers.length > 0) throw new Error(`standalone helper in tree: ${helpers.map((p) => p.name).join(", ")}`);
          sawApiAgent = tree.some((p) => p.pid !== app.pid && p.path && samePath(p.path, appExe));
          if (!sawApiAgent) await sleep(300);
        }
        const result = await pending;
        if (result.isError || !result.text.includes("API-ANSWER")) throw new Error(`delegate: ${result.text.slice(0, 300)}`);
        if (!sawApiAgent) {
          // 代理可能在两次采样之间已经应答；此时用 mock 的调用记录证明它确实运行过。
          const tree = await processTree([app.pid]).catch(() => []);
          sawApiAgent = tree.some((p) => p.pid !== app.pid && p.path && samePath(p.path, appExe));
        }
        if (!sawApiAgent) throw new Error("no harness-slint.exe child process (API agent) seen in the app's process tree");
        if (!mock.calls.some((c) => c.url === "/v1/chat/completions" && c.authorized)) throw new Error("mock provider received no authorized call");
        results.apiAgent = { childOfApp: true, mockCalls: mock.calls.length };
      } finally {
        results.delegateExit = await client.stop();
      }
    });
    await step("process tree holds no standalone helper binaries", async () => {
      const tree = await processTree([app.pid]).catch(() => []);
      results.tree = tree.map((p) => p.name);
      const helpers = tree.filter((p) => /harness-(memory-mcp|api-agent)/i.test(String(p.name)));
      if (helpers.length > 0) throw new Error(`standalone helper in tree: ${helpers.map((p) => p.name).join(", ")}`);
    });
  } finally {
    results.close = await step("close", () => close(app));
  }
}

// ---- main ----------------------------------------------------------------------------------------------------

async function main() {
  mkdirSync(reportDir, { recursive: true });
  await step("build", async () => build());
  const mock = await mockProvider();
  try {
    const agentsFile = fixtures(mock.baseUrl);
    await scenario(agentsFile, mock);
  } finally {
    mock.server.close();
  }
}

let exitCode = 0;
try {
  await main();
  results.ok = true;
} catch (error) {
  exitCode = 1;
  results.ok = false;
  results.failure = { step: error?.step ?? "?", message: String(error?.message ?? error) };
} finally {
  if (roots.size > 0) await killTrees([...roots]).catch(() => {});
  results.finishedAt = new Date().toISOString();
  writeFileSync(path.join(reportDir, "phase9-single-exe.json"), `${JSON.stringify(results, null, 2)}\n`);
}
if (exitCode === 0) {
  process.stdout.write("PHASE9_SINGLE_EXE_OK\n");
} else {
  process.stdout.write(`PHASE9_SINGLE_EXE_FAIL ${results.failure.step} ${results.failure.message}\n`);
}
process.exit(exitCode);
