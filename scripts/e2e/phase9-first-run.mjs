#!/usr/bin/env node
// Phase 9.10 first-run setup gate. Spec: Worse/docs/specs/harness/phase-9-parity.md §9.10 (Gate).
//
//   node Worse/scripts/e2e/phase9-first-run.mjs
//   PHASE9_FIRST_RUN_SKIP_BUILD=1   reuse the last build in Worse/.work/tmp/phase9-first-run-bin
//
// The built-in agents are used (no HARNESS_UI_AGENTS). PATH is replaced by a gate directory holding dummy
// `claude.cmd` / `codex.cmd` plus the Windows system directories: no npm, node or ACP adapter is on PATH.
// HARNESS_NPM points to a fake npm (.cmd → node script) that logs its calls, prints progress and, on
// `install -g <pkg>`, writes `<prefix>/<adapter>.cmd` into a prefix directory that is NOT on PATH, so the gate
// also proves the hub's re-probe through its adapter search directories. The written adapter starts
// harness-fake-agent, so the installed agent really answers a chat turn without a restart.
// Prints PHASE9_FIRST_RUN_OK or PHASE9_FIRST_RUN_FAIL <step> <reason> as the last line; report in
// Worse/.work/phase9/phase9-first-run.json, screenshots next to it.
//
// Safety: installIsolatedHome() runs first; HARNESS_HOME, the npm prefix and every fixture live in the isolated home.
// The real npm is never run (HARNESS_NPM always points to the fake or to a missing file; PATH has no npm). Only
// process trees rooted at PIDs this gate spawned are inspected or stopped.
import { installIsolatedHome } from "./lib/isolatedHome.mjs";

const { home } = installIsolatedHome("phase9-first-run");

const { spawn, spawnSync } = await import("node:child_process");
const { copyFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } = await import("node:fs");
const net = await import("node:net");
const os = (await import("node:os")).default;
const path = (await import("node:path")).default;
const process = (await import("node:process")).default;
const { fileURLToPath } = await import("node:url");
const { SlintMcp, byLabel, byLabelPrefix, label, sleep } = await import("./phase8-ui/mcp.mjs");
const { killTrees, processTree, stillAlive } = await import("./phase7-perf/procs.mjs");

const worse = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const reportDir = path.join(worse, ".work/phase9");
const binDir = path.join(worse, ".work/tmp/phase9-first-run-bin");
const exeName = (name) => (process.platform === "win32" ? `${name}.exe` : name);
const KEY_ENTER = "\n";
const CLAUDE_PKG = "@agentclientprotocol/claude-agent-acp";
const CODEX_PKG = "@agentclientprotocol/codex-acp";

const harnessHome = path.join(home, ".harness");
const workspace = path.join(home, "workspace");
const gateTools = path.join(home, "gate-path");
const npmDir = path.join(home, "fake-npm");
const npmPrefix = path.join(home, "npm-global");
const npmLog = path.join(npmDir, "calls.jsonl");
const fakeState = path.join(home, "fake-agent-state");
const roots = new Set();
const results = { startedAt: new Date().toISOString(), steps: [], screenshots: [] };
let currentMcp = null;

class StepError extends Error {
  constructor(step, message) {
    super(message);
    this.step = step;
  }
}

const log = (line) => process.stdout.write(`[phase9-first-run] ${line}\n`);

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
        .then((list) => list.map((e) => `${e.accessibleRole ?? ""}:${label(e)}`).filter((l) => l.length > 1).slice(0, 250))
        .catch((e) => [String(e?.message ?? e)]);
      await shot(currentMcp, `first-run-fail-${name.replace(/[^a-z0-9]+/gi, "-")}`).catch(() => {});
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
  if (process.env.PHASE9_FIRST_RUN_SKIP_BUILD !== "1") {
    cargo(["build", "--release", "-p", "harness-fake-agent"]);
    copy("harness-fake-agent");
    cargo(["build", "--release", "-p", "harness-slint", "--features", "automation"]);
    copy("harness-slint", "harness-slint-automation");
  }
  for (const name of ["harness-fake-agent", "harness-slint-automation"]) {
    if (!existsSync(path.join(binDir, exeName(name)))) throw new Error(`${name} missing in ${binDir}`);
  }
}

const FAKE_NPM = String.raw`// Fake npm for phase9-first-run (never touches the real npm).
import { appendFileSync, mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
const [prefix, logFile, fakeAgent, fakeState] = process.env.GATE_NPM_CONFIG.split("|");
const args = process.argv.slice(2);
appendFileSync(logFile, JSON.stringify({ args }) + "\n");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const bins = { "@agentclientprotocol/claude-agent-acp": ["claude-agent-acp", "claude-code"], "@agentclientprotocol/codex-acp": ["codex-acp", "codex"] };
if (args[0] === "prefix" && args[1] === "-g") {
  console.log(prefix);
  process.exit(0);
}
if (args[0] === "install" && args[1] === "-g" && bins[args[2]]) {
  const [bin, name] = bins[args[2]];
  console.log("npm fake: resolving " + args[2]);
  await sleep(400);
  console.error("npm warn fake: deprecated nothing");
  await sleep(400);
  mkdirSync(prefix, { recursive: true });
  writeFileSync(path.join(prefix, bin + ".cmd"), "@echo off\r\nset FAKE_AGENT_STATE=" + fakeState + "\r\n\"" + fakeAgent + "\" --name " + name + " %*\r\n");
  console.log("added 1 package in 1s (" + args[2] + ")");
  await sleep(300);
  process.exit(0);
}
console.error("fake npm: unsupported " + args.join(" "));
process.exit(1);
`;

function fixtures() {
  mkdirSync(workspace, { recursive: true });
  writeFileSync(path.join(workspace, "README.md"), "# demo workspace\n");
  mkdirSync(harnessHome, { recursive: true });
  writeFileSync(path.join(harnessHome, "settings.json"), `${JSON.stringify({ appearance: { theme: "light" } }, null, 2)}\n`);
  mkdirSync(gateTools, { recursive: true });
  // 仅用于 PATH 探测的 CLI 占位：Harness 永远不会运行 CLI 本身。
  for (const cli of ["claude", "codex"]) writeFileSync(path.join(gateTools, `${cli}.cmd`), "@echo off\r\nexit /b 0\r\n");
  mkdirSync(npmDir, { recursive: true });
  mkdirSync(fakeState, { recursive: true });
  writeFileSync(path.join(npmDir, "fake-npm.mjs"), FAKE_NPM);
  writeFileSync(path.join(npmDir, "npm.cmd"), `@"${process.execPath}" "${path.join(npmDir, "fake-npm.mjs")}" %*\r\n`);
  writeFileSync(npmLog, "");
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

function controlledPath() {
  const root = process.env.SystemRoot ?? "C:\\Windows";
  return [gateTools, path.join(root, "System32"), root, path.join(root, "System32", "Wbem")].join(path.delimiter);
}

async function launch({ harness = harnessHome, npm = path.join(npmDir, "npm.cmd") } = {}) {
  const exe = path.join(binDir, exeName("harness-slint-automation"));
  const env = {
    ...process.env,
    PATH: controlledPath(),
    HARNESS_HOME: harness,
    HARNESS_WORKSPACE: workspace,
    HARNESS_NPM: npm,
    GATE_NPM_CONFIG: [npmPrefix, npmLog, path.join(binDir, exeName("harness-fake-agent")), fakeState].join("|"),
    GIT_CEILING_DIRECTORIES: home,
  };
  for (const key of ["SLINT_MCP_PORT", "SLINT_BACKEND", "HARNESS_UI_CORE", "HARNESS_UI_WINDOW_SIZE", "HARNESS_UI_AGENTS", "HARNESS_MEMORY_MCP", "HARNESS_API_AGENT", "Path"]) {
    if (key !== "PATH") delete env[key];
  }
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

/** Scrolls the settings content until the element appears (Slint exposes only laid-out rows). */
async function reveal(mcp, what, predicate, { x = 700, y = 420, steps = 24 } = {}) {
  let hit = await mcp.find(predicate);
  for (let i = 0; !hit && i < steps * 3; i++) {
    await mcp.scrollAt(x, y, i < steps ? -120 : 120);
    await sleep(120);
    hit = await mcp.find(predicate);
  }
  if (!hit) throw new Error(`${what} not found after scrolling`);
  return hit;
}

async function press(mcp, text, timeoutMs) {
  await mcp.activate(await button(mcp, text, timeoutMs));
}

async function chord(mcp, modifiers, key) {
  const codes = { ctrl: "\u0011", alt: "\u0012", shift: "\u0010" };
  for (const m of modifiers) await mcp.key(codes[m], "Press");
  await mcp.key(key);
  for (const m of [...modifiers].reverse()) await mcp.key(codes[m], "Release");
}

function npmCalls() {
  return readFileSync(npmLog, "utf8")
    .split("\n")
    .filter((l) => l.trim())
    .map((l) => JSON.parse(l).args);
}

async function waitFlag(harness, expected) {
  const file = path.join(harness, "settings.json");
  const deadline = Date.now() + 10_000;
  let last = null;
  while (Date.now() < deadline) {
    try {
      last = JSON.parse(readFileSync(file, "utf8"))?.setup?.firstRunDone ?? null;
      if (last === expected) return;
    } catch {
      // being written
    }
    await sleep(200);
  }
  throw new Error(`setup.firstRunDone is ${JSON.stringify(last)}, expected ${expected}`);
}

const DIALOG = (e) => isButton("Install missing adapters")(e);
const disabled = (e) => e.accessibleEnabled === false || e.enabled === false;

// ---- scenario ------------------------------------------------------------------------------------------------

async function firstLaunch() {
  const app = await step("first launch", () => launch());
  const { mcp } = app;
  try {
    await step("dialog opens with adapter-missing and CLI-missing rows", async () => {
      await mcp.waitFor("setup dialog", DIALOG, 30_000);
      await button(mcp, `Install ${CLAUDE_PKG}`, 10_000);
      await button(mcp, `Install ${CODEX_PKG}`, 10_000);
      await mcp.waitFor("agy CLI missing", byLabel("CLI not found (agy)"), 5000);
      const list = await mcp.elements();
      const missing = list.filter((e) => label(e) === "Adapter missing").length;
      const notInstalled = list.filter((e) => label(e) === "Not installed").length;
      results.dialogTags = { adapterMissing: missing, notInstalled };
      if (missing < 2 || notInstalled < 1) throw new Error(`tags: ${missing} adapter missing, ${notInstalled} not installed`);
      await shot(mcp, "first-run-dialog");
    });
    await step("Skip for now closes the dialog and stores the flag", async () => {
      await press(mcp, "Skip for now");
      await mcp.waitGone("setup dialog", DIALOG, 5000);
      await waitFlag(harnessHome, true);
      if (npmCalls().some((a) => a[0] === "install")) throw new Error("npm install ran without a click");
    });
    await step("Agents card: Adapter missing → Install adapter → Installed", async () => {
      await chord(mcp, ["ctrl"], ",");
      await mcp.waitFor("settings shell", byLabel("Back to workspace"), 10_000);
      await press(mcp, "Agents");
      await mcp.waitFor("Agents page", (e) => label(e) === "Agents" && !isRole(e, "button"));
      const install = await reveal(mcp, "Install adapter for Claude Code", isButton("Install adapter for Claude Code"));
      await shot(mcp, "first-run-card-missing");
      await mcp.activate(install);
      await mcp.waitGone("Claude Code install button", isButton("Install adapter for Claude Code"), 30_000);
      await reveal(mcp, "Installed tag", (e) => String(label(e)).endsWith("Installed") && !String(label(e)).startsWith("Not"));
      // Codex 仍缺适配器：它的按钮还在。
      await reveal(mcp, "Codex install button", isButton("Install adapter for Codex"));
      await shot(mcp, "first-run-card-installed");
    });
    await step("Set up agents… → Install missing adapters installs Codex with visible output", async () => {
      const open = await reveal(mcp, "Set up agents…", isButton("Set up agents…"));
      await mcp.activate(open);
      await mcp.waitFor("setup dialog", DIALOG, 10_000);
      await mcp.waitGone("Claude row install", isButton(`Install ${CLAUDE_PKG}`), 10_000);
      await press(mcp, "Install missing adapters");
      const output = await mcp.waitFor("install output", (e) => label(e) === "Install output" || String(label(e)).includes("npm fake: resolving @agentclientprotocol/codex-acp"), 15_000);
      results.outputSeen = label(output);
      await shot(mcp, "first-run-installing");
      await mcp.waitGone("Codex row install", isButton(`Install ${CODEX_PKG}`), 30_000);
      await button(mcp, "Done", 10_000);
      await shot(mcp, "first-run-installed");
      await press(mcp, "Done");
      await mcp.waitGone("setup dialog", DIALOG, 5000);
    });
    await step("fake npm log has exactly the two adapter packages", async () => {
      const installs = npmCalls().filter((a) => a[0] === "install");
      results.npmInstalls = installs;
      const pkgs = installs.map((a) => a[2]).sort();
      if (JSON.stringify(pkgs) !== JSON.stringify([CLAUDE_PKG, CODEX_PKG].sort())) throw new Error(`installs: ${JSON.stringify(installs)}`);
      if (installs.some((a) => a[1] !== "-g")) throw new Error("install without -g");
    });
    await step("installed adapter answers a chat turn without a restart", async () => {
      await mcp.key("\u001b");
      await mcp.waitGone("settings shell", byLabel("Back to workspace"), 10_000);
      await press(mcp, "New session");
      const input = await mcp.waitFor("message input", (e) => label(e) === "Message" && isRole(e, "textinput"));
      await mcp.click(input);
      await mcp.setValue(input, "hello after setup");
      await sleep(200);
      await mcp.key(KEY_ENTER);
      await mcp.waitFor("reply", byLabelPrefix("Claude Code: ", "reply to: hello after setup"), 30_000);
    });
  } finally {
    results.close1 = await step("close", () => close(app));
  }
}

async function restart() {
  const app = await step("restart", () => launch());
  try {
    await step("no dialog after the first run", async () => {
      await app.mcp.waitFor("message input", (e) => label(e) === "Message" && isRole(e, "textinput"), 20_000);
      await sleep(4000);
      if (await app.mcp.find(DIALOG)) throw new Error("setup dialog opened again");
    });
  } finally {
    results.close2 = await step("close restart", () => close(app));
  }
}

async function noNpm() {
  const home2 = path.join(home, "harness-no-npm");
  mkdirSync(home2, { recursive: true });
  writeFileSync(path.join(home2, "settings.json"), `${JSON.stringify({ appearance: { theme: "light" } }, null, 2)}\n`);
  const before = npmCalls().length;
  const app = await step("launch without npm", () => launch({ harness: home2, npm: path.join(npmDir, "missing-npm.cmd") }));
  try {
    await step("Node.js steps shown, Install disabled", async () => {
      await app.mcp.waitFor("setup dialog", DIALOG, 30_000);
      await app.mcp.waitFor("npm steps", byLabelPrefix("npm was not found", "nodejs.org"), 10_000);
      const install = await button(app.mcp, `Install ${CODEX_PKG}`, 5000);
      const all = await button(app.mcp, "Install missing adapters", 5000);
      results.disabledProps = Object.keys(install).filter((k) => /enabled/i.test(k));
      if (!disabled(install) || !disabled(all)) throw new Error(`install buttons not disabled: ${JSON.stringify({ row: install, all })}`);
      await app.mcp.activate(all);
      await sleep(1000);
      if (npmCalls().length !== before) throw new Error("npm ran although it is missing");
      await shot(app.mcp, "first-run-no-npm");
    });
  } finally {
    results.close3 = await step("close no-npm", () => close(app));
  }
}

async function main() {
  mkdirSync(reportDir, { recursive: true });
  await step("build", async () => build());
  fixtures();
  await firstLaunch();
  await restart();
  await noNpm();
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
  writeFileSync(path.join(reportDir, "phase9-first-run.json"), `${JSON.stringify(results, null, 2)}\n`);
}
if (exitCode === 0) {
  process.stdout.write("PHASE9_FIRST_RUN_OK\n");
} else {
  process.stdout.write(`PHASE9_FIRST_RUN_FAIL ${results.failure.step} ${results.failure.message}\n`);
}
process.exit(exitCode);
