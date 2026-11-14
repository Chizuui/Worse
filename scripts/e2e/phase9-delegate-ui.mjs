#!/usr/bin/env node
// Phase 9.8 orchestration gate, UI half. Spec: Worse/docs/specs/harness/phase-9-parity.md §9.8 (UI, Verification).
// The core half is the `delegate` step of harness-core-gate (phase9-settings.mjs).
//
//   node Worse/scripts/e2e/phase9-delegate-ui.mjs
//   PHASE9_DELEGATE_SKIP_BUILD=1   reuse the last build in Worse/.work/tmp/phase9-delegate-bin
//
// The parent agent is played by this gate: it reads the `harness-delegate` MCP entry Harness passed to the parent's
// native session (fake agent log) and runs `harness-memory-mcp --delegate` with it, exactly as an agent would.
// Prints PHASE9_DELEGATE_UI_OK or PHASE9_DELEGATE_UI_FAIL <step> <reason> as the last line; report in
// Worse/.work/phase9/phase9-delegate-ui.json, screenshots next to it.
//
// Safety: installIsolatedHome() runs first. HARNESS_HOME and the workspace live in the isolated home and hold dummy
// fixtures only. Agents are harness-fake-agent processes; no real agent CLI is involved. Only process trees rooted
// at PIDs this gate spawned are inspected or stopped. The delegate token is read from the fake log and never printed.
import { installIsolatedHome } from "./lib/isolatedHome.mjs";

const { home } = installIsolatedHome("phase9-delegate-ui");

const { spawn, spawnSync } = await import("node:child_process");
const { copyFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } = await import("node:fs");
const net = await import("node:net");
const os = (await import("node:os")).default;
const path = (await import("node:path")).default;
const process = (await import("node:process")).default;
const readline = await import("node:readline");
const { fileURLToPath } = await import("node:url");
const { SlintMcp, byLabel, byLabelPrefix, label, sleep } = await import("./phase8-ui/mcp.mjs");
const { killTrees, processTree, stillAlive } = await import("./phase7-perf/procs.mjs");

const worse = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const reportDir = path.join(worse, ".work/phase9");
const binDir = path.join(worse, ".work/tmp/phase9-delegate-bin");
const exeName = (name) => (process.platform === "win32" ? `${name}.exe` : name);
const KEY_ENTER = "\n";

const harnessHome = path.join(home, ".harness");
const workspace = path.join(home, "workspace");
const fakeState = path.join(home, "fake-agent-state");
const fakeLog = path.join(fakeState, "log.jsonl");
const roots = new Set();
const results = { startedAt: new Date().toISOString(), steps: [], screenshots: [] };
let currentMcp = null;

class StepError extends Error {
  constructor(step, message) {
    super(message);
    this.step = step;
  }
}

const log = (line) => process.stdout.write(`[phase9-delegate] ${line}\n`);

async function step(name, fn) {
  const started = Date.now();
  try {
    const value = await fn();
    results.steps.push({ name, ok: true, ms: Date.now() - started });
    log(`ok   ${name} (${Date.now() - started} ms)`);
    return value;
  } catch (error) {
    results.steps.push({ name, ok: false, ms: Date.now() - started, error: String(error?.message ?? error) });
    // 失败现场：记录可见元素标签和截图。
    if (currentMcp) {
      results.failureLabels = await currentMcp
        .elements()
        .then((list) => list.map((e) => `${e.accessibleRole ?? ""}:${label(e)}`).filter((l) => l.length > 1).slice(0, 250))
        .catch((e) => [String(e?.message ?? e)]);
      await shot(currentMcp, `delegate-fail-${name.replace(/[^a-z0-9]+/gi, "-")}`).catch(() => {});
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
  mkdirSync(binDir, { recursive: true });
  const release = path.join(process.env.CARGO_TARGET_DIR ? path.resolve(process.env.CARGO_TARGET_DIR) : path.join(worse, "target"), "release");
  const copy = (name, as = name) => copyFileSync(path.join(release, exeName(name)), path.join(binDir, exeName(as)));
  if (process.env.PHASE9_DELEGATE_SKIP_BUILD !== "1") {
    cargo(["build", "--release", "-p", "harness-fake-agent", "-p", "harness-agent-hub", "--bins"]);
    copy("harness-fake-agent");
    copy("harness-memory-mcp");
    cargo(["build", "--release", "-p", "harness-slint", "--features", "automation"]);
    copy("harness-slint", "harness-slint-automation");
  }
  for (const name of ["harness-fake-agent", "harness-memory-mcp", "harness-slint-automation"]) {
    if (!existsSync(path.join(binDir, exeName(name)))) throw new Error(`${name} missing in ${binDir}`);
  }
}

function fixtures() {
  mkdirSync(workspace, { recursive: true });
  writeFileSync(path.join(workspace, "README.md"), "# demo workspace\n");
  mkdirSync(fakeState, { recursive: true });
  mkdirSync(harnessHome, { recursive: true });
  writeFileSync(path.join(harnessHome, "settings.json"), `${JSON.stringify({ appearance: { theme: "light" } }, null, 2)}\n`);
  const agent = (id, name) => ({
    id,
    name,
    program: path.join(binDir, exeName("harness-fake-agent")),
    args: ["--name", id],
    env: { FAKE_AGENT_STATE: fakeState, FAKE_AGENT_LOG: fakeLog },
  });
  const file = path.join(home, "ui-agents.json");
  writeFileSync(file, JSON.stringify([agent("claude-code", "Claude Code"), agent("codex", "Codex")], null, 2));
  return file;
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

async function launch(agentsFile) {
  const exe = path.join(binDir, exeName("harness-slint-automation"));
  const env = {
    ...process.env,
    HARNESS_HOME: harnessHome,
    HARNESS_WORKSPACE: workspace,
    HARNESS_UI_AGENTS: agentsFile,
    HARNESS_MEMORY_MCP: path.join(binDir, exeName("harness-memory-mcp")),
    GIT_CEILING_DIRECTORIES: home,
  };
  for (const key of ["SLINT_MCP_PORT", "SLINT_BACKEND", "HARNESS_UI_CORE", "HARNESS_UI_WINDOW_SIZE", "HARNESS_DELEGATE"]) delete env[key];
  const port = await freePort();
  env.SLINT_MCP_PORT = String(port);
  const child = spawn(exe, [], { cwd: workspace, env, stdio: ["ignore", "ignore", "pipe"], windowsHide: false });
  const stderr = [];
  child.stderr.setEncoding("utf8").on("data", (chunk) => stderr.push(chunk));
  roots.add(child.pid);
  const exited = new Promise((resolve) => child.once("exit", (code) => resolve(code)));
  const app = { child, pid: child.pid, exited, stderr, mcp: new SlintMcp(port) };
  await app.mcp.connect();
  currentMcp = app.mcp;
  return app;
}

/** WM_CLOSE first (graceful teardown), force only as a fallback. */
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

async function shot(mcp, name) {
  const png = await mcp.screenshot();
  const file = path.join(reportDir, `ui-${name}.png`);
  writeFileSync(file, png);
  results.screenshots.push(path.relative(worse, file).replaceAll("\\", "/"));
  return png;
}

const isRole = (e, role) => String(e.accessibleRole ?? "").toLowerCase() === role;
const isButton = (text) => (e) => label(e) === text && isRole(e, "button");
const button = (mcp, text, timeoutMs) => mcp.waitFor(`button "${text}"`, isButton(text), timeoutMs);
const press = async (mcp, text) => mcp.activate(await button(mcp, text));

async function send(mcp, text) {
  const input = await mcp.waitFor("message input", (e) => label(e) === "Message" && isRole(e, "textinput"));
  await mcp.click(input);
  await mcp.setValue(input, text);
  await sleep(200);
  await mcp.key(KEY_ENTER);
  return mcp.waitFor(`reply to ${text}`, byLabelPrefix("Claude Code: ", `reply to: ${text}`), 30_000);
}

// ---- the parent agent's side: harness-delegate MCP client ---------------------------------------------------

function logEntries() {
  if (!existsSync(fakeLog)) return [];
  return readFileSync(fakeLog, "utf8")
    .split("\n")
    .filter((l) => l.trim())
    .map((l) => {
      try {
        return JSON.parse(l);
      } catch {
        return {};
      }
    });
}

/** env of the newest `harness-delegate` entry Harness passed to a native session (the parent's). */
function delegateEnv() {
  for (const entry of logEntries().reverse()) {
    const server = (entry.mcpServers ?? []).find((s) => s.name === "harness-delegate");
    if (server) return Object.fromEntries((server.env ?? []).map((v) => [v.name, v.value]));
  }
  throw new Error("Harness passed no harness-delegate server to the parent session");
}

class DelegateClient {
  constructor(env) {
    this.child = spawn(path.join(binDir, exeName("harness-memory-mcp")), ["--delegate"], {
      env: { ...process.env, ...env },
      stdio: ["pipe", "pipe", "ignore"],
      windowsHide: true,
    });
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

  request(method, params, timeoutMs = 60_000) {
    const id = this.next++;
    const reply = new Promise((resolve, reject) => {
      this.waiting.set(id, resolve);
      setTimeout(() => reject(new Error(`${method} timed out`)), timeoutMs).unref();
    });
    this.child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
    return reply;
  }

  async call(name, args, timeoutMs) {
    const reply = await this.request("tools/call", { name, arguments: args }, timeoutMs);
    return { text: reply.result?.content?.[0]?.text ?? "", isError: reply.result?.isError === true };
  }

  async stop() {
    this.child.stdin.end();
    await Promise.race([new Promise((r) => this.child.once("exit", r)), sleep(5000)]);
    roots.delete(this.child.pid);
  }
}

// ---- scenario ------------------------------------------------------------------------------------------------

const subRow = (title) => (e) => label(e) === title && isRole(e, "button");

async function scenario(agentsFile) {
  const app = await step("start", () => launch(agentsFile));
  const { mcp } = app;
  let client = null;
  try {
    await step("parent session on Claude Code", async () => {
      await press(mcp, "New session");
      await send(mcp, "plan the work");
    });
    client = await step("parent agent connects to harness-delegate", async () => {
      const c = new DelegateClient(delegateEnv());
      await c.request("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "gate", version: "1" } });
      const agents = await c.call("list_agents", {});
      if (agents.isError || !agents.text.includes("codex")) throw new Error(`list_agents: ${agents.text}`);
      return c;
    });
    const pending = client.call("delegate", { task: "[[tools]] [[slow 120]] [[say SUB-ANSWER]]", agent: "codex", title: "Review module" }, 60_000);
    await step("panel shows the running subagent with activity and timer", async () => {
      await mcp.waitFor("Subagents panel", byLabel("Subagents"), 15_000);
      const row = await mcp.waitFor("running row", (e) => subRow("Review module")(e) && String(e.accessibleDescription ?? "").startsWith("running"), 15_000);
      if (!String(row.accessibleDescription).includes("codex")) throw new Error(`row description ${row.accessibleDescription}`);
      await mcp.waitFor("running count", byLabel("1 running"), 5000);
      await sleep(1500);
      await shot(mcp, "delegate-running");
    });
    await step("delegate returns the subagent answer", async () => {
      const result = await pending;
      if (result.isError || !result.text.includes("SUB-ANSWER") || !result.text.includes(" done")) throw new Error(`delegate result: ${result.text.slice(0, 200)}`);
    });
    await step("panel shows done with the answer preview", async () => {
      await mcp.waitFor("done row", (e) => subRow("Review module")(e) && String(e.accessibleDescription ?? "").startsWith("done"), 10_000);
      await mcp.waitGone("running count", byLabel("1 running"), 5000);
      await shot(mcp, "delegate-done");
    });
    await step("subagent session is hidden from the sidebar", async () => {
      const list = await mcp.elements();
      const inSidebar = list.filter((e) => label(e) === "Review module" && isRole(e, "listitem"));
      if (inSidebar.length > 0) throw new Error("child session listed in the sidebar");
    });
    await step("row opens the subagent session with the parent bar", async () => {
      await mcp.activate(await mcp.waitFor("row", subRow("Review module"), 5000));
      await button(mcp, "Back to main session", 15_000);
      await mcp.waitFor("child transcript", byLabelPrefix("Codex: ", "SUB-ANSWER"), 15_000);
      await mcp.waitGone("Subagents panel", byLabel("Subagents"), 5000);
      await shot(mcp, "delegate-child");
    });
    await step("back returns to the parent with its panel", async () => {
      await press(mcp, "Back to main session");
      await mcp.waitFor("Subagents panel", byLabel("Subagents"), 15_000);
      await mcp.waitFor("parent transcript", byLabelPrefix("Claude Code: ", "reply to: plan the work"), 15_000);
      await mcp.waitGone("parent bar", isButton("Back to main session"), 5000);
    });
    await step("collapse and expand the panel", async () => {
      await press(mcp, "Collapse subagents");
      await mcp.waitGone("row", subRow("Review module"), 5000);
      await press(mcp, "Expand subagents");
      await mcp.waitFor("row", subRow("Review module"), 5000);
    });
  } finally {
    if (client) await client.stop();
    const closed = await step("close", () => close(app));
    results.close = closed;
  }
}

// ---- main ----------------------------------------------------------------------------------------------------

async function main() {
  mkdirSync(reportDir, { recursive: true });
  await step("build", async () => build());
  const agentsFile = fixtures();
  await scenario(agentsFile);
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
  writeFileSync(path.join(reportDir, "phase9-delegate-ui.json"), `${JSON.stringify(results, null, 2)}\n`);
}
if (exitCode === 0) {
  process.stdout.write("PHASE9_DELEGATE_UI_OK\n");
} else {
  process.stdout.write(`PHASE9_DELEGATE_UI_FAIL ${results.failure.step} ${results.failure.message}\n`);
}
process.exit(exitCode);
