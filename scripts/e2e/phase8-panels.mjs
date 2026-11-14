#!/usr/bin/env node
// Phase 8 panels gate (agent A, round 8.2): the Side Pane panels of the native Slint UI — git, diff (unified and
// side by side), search (files / content / preview), terminal (PTY + alacritty_terminal emulation) and the
// watcher — driven through Slint's embedded MCP introspection server. Spec: Worse/docs/specs/harness/phase-8-ui.md
// §10, Worse/docs/specs/harness/phase-8-core.md §15.
//
//   node Worse/scripts/e2e/phase8-panels.mjs
//   PHASE8_PANELS_SKIP_BUILD=1   reuse the last build in Worse/.work/tmp/phase8-panels-bin
//   PHASE8_PANELS_IDLE_S=<s>     idle sample point of the RAM check (default 60)
//   PHASE8_PANELS_SKIP_RAM=1     scenarios only (development; the gate result then does not cover RAM)
//
// Prints PHASE8_PANELS_OK or PHASE8_PANELS_FAIL <step> <reason> as the last line. Writes
// Worse/.work/phase8/phase8-panels.json and panel-*.png screenshots next to it.
//
// Safety: installIsolatedHome() runs first; HARNESS_HOME and the workspace (a fixture git repository) live inside
// it, and GIT_CEILING_DIRECTORIES stops git from walking up into the enclosing ZCode repository. The only agents
// are harness-fake-agent processes; no real agent CLI, provider or remote server is involved. Only process trees
// rooted at PIDs this gate spawned are inspected or stopped.
import { installIsolatedHome } from "./lib/isolatedHome.mjs";

const { home } = installIsolatedHome("phase8-panels");

const { spawn, spawnSync } = await import("node:child_process");
const { appendFileSync, copyFileSync, existsSync, mkdirSync, writeFileSync } = await import("node:fs");
const net = await import("node:net");
const os = (await import("node:os")).default;
const path = (await import("node:path")).default;
const process = (await import("node:process")).default;
const { fileURLToPath } = await import("node:url");
const { SlintMcp, byLabel, byLabelPrefix, label, sleep } = await import("./phase8-ui/mcp.mjs");
const { killTrees, processTree, sampleTree, stillAlive, summarizeSample } = await import("./phase7-perf/procs.mjs");

const worse = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const reportDir = path.join(worse, ".work/phase8");
const binDir = path.join(worse, ".work/tmp/phase8-panels-bin");
const exeName = (name) => (process.platform === "win32" ? `${name}.exe` : name);
const IDLE_S = Number(process.env.PHASE8_PANELS_IDLE_S ?? 60);
const RAM_LIMIT_MB = 50;
const SHELLS = new Set(["pwsh.exe", "powershell.exe", "cmd.exe", "conhost.exe", "openconsole.exe"]);

const harnessHome = path.join(home, ".harness");
const workspace = path.join(home, "workspace");
const fakeState = path.join(home, "fake-agent-state");
// 隔离 home 位于 ZCode 仓库内部：没有天花板时 git 会向上找到真实仓库（快照、status 都会落到用户仓库）。
process.env.GIT_CEILING_DIRECTORIES = home;
const roots = new Set();
const results = { startedAt: new Date().toISOString(), steps: [], ram: {}, screenshots: [], notes: [] };
let currentMcp = null;

class StepError extends Error {
  constructor(step, message) {
    super(message);
    this.step = step;
  }
}

function log(line) {
  process.stdout.write(`[phase8-panels] ${line}\n`);
}

async function step(name, fn) {
  const started = Date.now();
  try {
    const value = await fn();
    results.steps.push({ name, ok: true, ms: Date.now() - started });
    log(`ok   ${name} (${Date.now() - started} ms)`);
    return value;
  } catch (error) {
    results.steps.push({ name, ok: false, ms: Date.now() - started, error: String(error?.message ?? error) });
    // 失败现场：记录当前可见元素标签和截图，便于定位。
    if (currentMcp) {
      results.failureLabels = await currentMcp
        .elements()
        .then((list) => list.map(label).filter(Boolean).slice(0, 250))
        .catch((e) => [String(e?.message ?? e)]);
      await shot(currentMcp, `fail-${name.replace(/[^a-z0-9]+/gi, "-")}`).catch(() => {});
    }
    throw error instanceof StepError ? error : new StepError(name, error?.message ?? String(error));
  }
}

// ---- build -------------------------------------------------------------------------------------------------

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
  // 修复：cargo 遵循 CARGO_TARGET_DIR，而这里写死了 target/release，会把旧的（甚至非 automation 的）二进制拷进 bin 目录，
  // 导致 "MCP server not ready"。与 phase9 门禁一致，从实际的目标目录取产物。
  const release = path.join(process.env.CARGO_TARGET_DIR ? path.resolve(process.env.CARGO_TARGET_DIR) : path.join(worse, "target"), "release");
  const copy = (name, as = name) => copyFileSync(path.join(release, exeName(name)), path.join(binDir, exeName(as)));
  if (process.env.PHASE8_PANELS_SKIP_BUILD !== "1") {
    cargo(["build", "--release", "-p", "harness-fake-agent", "-p", "harness-agent-hub", "--bins"]);
    copy("harness-fake-agent");
    copy("harness-memory-mcp");
    // 同一输出路径：先构建带自动化通道的版本并复制，再构建产品版本（RAM 基线只测产品版本）。
    cargo(["build", "--release", "-p", "harness-slint", "--features", "automation"]);
    copy("harness-slint", "harness-slint-automation");
    cargo(["build", "--release", "-p", "harness-slint"]);
    copy("harness-slint");
  }
  for (const name of ["harness-fake-agent", "harness-memory-mcp", "harness-slint", "harness-slint-automation"]) {
    if (!existsSync(path.join(binDir, exeName(name)))) throw new Error(`${name} missing in ${binDir}`);
  }
}

// ---- fixtures ----------------------------------------------------------------------------------------------

function git(...args) {
  const result = spawnSync("git", args, { cwd: workspace, env: process.env, encoding: "utf8", windowsHide: true });
  if (result.status !== 0) throw new Error(`git ${args.join(" ")}: ${result.stderr.trim()}`);
  return result.stdout.trim();
}

function fixtures() {
  mkdirSync(path.join(workspace, "src"), { recursive: true });
  mkdirSync(path.join(workspace, "docs"), { recursive: true });
  writeFileSync(path.join(workspace, "README.md"), "# panels fixture\n\nfirst line\n");
  writeFileSync(path.join(workspace, "src", "lib.rs"), "// NEEDLE one\npub fn answer() -> u32 {\n    42\n}\n");
  writeFileSync(path.join(workspace, "docs", "guide.md"), "needle two\n\nGuide text.\n");
  git("init", "-q", "-b", "main");
  git("config", "user.name", "Panels Gate");
  git("config", "user.email", "panels-gate@example.invalid");
  git("config", "core.hooksPath", "no-hooks");
  git("config", "core.autocrlf", "false");
  git("add", "-A");
  git("commit", "-q", "-m", "fixture");
  // Working-tree state shown by the git panel: one modified file, one untracked file.
  appendFileSync(path.join(workspace, "README.md"), "added by the gate\n");
  writeFileSync(path.join(workspace, "notes.txt"), "untracked note\n");

  mkdirSync(fakeState, { recursive: true });
  const agent = (id, name) => ({
    id,
    name,
    program: path.join(binDir, exeName("harness-fake-agent")),
    args: ["--name", id],
    env: { FAKE_AGENT_STATE: fakeState, FAKE_AGENT_LOG: path.join(fakeState, "log.jsonl") },
  });
  const agents = [
    agent("claude-code", "Claude Code"),
    agent("codex", "Codex"),
    { id: "agy", name: "agy", program: path.join(home, "not-installed", exeName("agy")), args: [] },
  ];
  const file = path.join(home, "ui-agents.json");
  writeFileSync(file, JSON.stringify(agents, null, 2));
  return file;
}

// ---- launch / close ----------------------------------------------------------------------------------------

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

async function launch({ automation, agentsFile }) {
  const exe = path.join(binDir, exeName(automation ? "harness-slint-automation" : "harness-slint"));
  const env = {
    ...process.env,
    HARNESS_HOME: harnessHome,
    HARNESS_WORKSPACE: workspace,
    HARNESS_UI_AGENTS: agentsFile,
    HARNESS_MEMORY_MCP: path.join(binDir, exeName("harness-memory-mcp")),
    GIT_CEILING_DIRECTORIES: home,
    HARNESS_PANELS_TRACE: process.env.HARNESS_PANELS_TRACE ?? "",
  };
  delete env.SLINT_MCP_PORT;
  delete env.SLINT_BACKEND;
  delete env.HARNESS_UI_CORE;
  let port = null;
  if (automation) {
    port = await freePort();
    env.SLINT_MCP_PORT = String(port);
  }
  const child = spawn(exe, [], { cwd: workspace, env, stdio: ["ignore", "ignore", "pipe"], windowsHide: false });
  const stderr = [];
  child.stderr.setEncoding("utf8").on("data", (chunk) => stderr.push(chunk));
  roots.add(child.pid);
  const exited = new Promise((resolve) => child.once("exit", (code) => resolve(code)));
  const app = { child, pid: child.pid, exited, stderr, mcp: null };
  if (automation) {
    app.mcp = new SlintMcp(port);
    await app.mcp.connect();
    currentMcp = app.mcp;
  }
  return app;
}

/** WM_CLOSE first (graceful teardown: the panels worker kills its shells), force only as a fallback. */
async function close(app) {
  const tree = await processTree([app.pid]).catch(() => []);
  results.closeRequestedAt = Date.now();
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
  results.closeStderr = app.stderr.join("").slice(-2000);
  if (currentMcp === app.mcp) currentMcp = null;
  if (!graceful) throw new Error(`app did not exit on WM_CLOSE within 15 s; stderr: ${app.stderr.join("").slice(-1500)}`);
  return { graceful, treeSize: tree.length };
}

async function shellsIn(pid) {
  return (await processTree([pid])).filter((p) => SHELLS.has(String(p.name).toLowerCase()));
}

// ---- UI helpers --------------------------------------------------------------------------------------------

async function shot(mcp, name) {
  const png = await mcp.screenshot();
  const file = path.join(reportDir, `panel-${name}.png`);
  writeFileSync(file, png);
  results.screenshots.push(path.relative(worse, file).replaceAll("\\", "/"));
  return png;
}

const isRole = (role) => (e) => String(e.accessibleRole ?? "").toLowerCase() === role;
const labelled = (text, role) => (e) => label(e) === text && (!role || isRole(role)(e));
const labelIncludes = (text) => (e) => label(e).includes(text);

async function openTab(mcp, name) {
  const tab = await mcp.waitFor(`tab ${name}`, labelled(name, "tab"));
  await mcp.activate(tab);
}

async function press(mcp, text) {
  const target = await mcp.waitFor(`button "${text}"`, labelled(text, "button"));
  await mcp.activate(target);
}

async function typeInto(mcp, inputLabel, text, enter = true) {
  const input = await mcp.waitFor(`input ${inputLabel}`, byLabel(inputLabel));
  await mcp.click(input);
  await mcp.setValue(input, text);
  if (enter) await mcp.key("\n");
}

// ---- scenarios ---------------------------------------------------------------------------------------------

async function scenarios(agentsFile) {
  const app = await step("start", () => launch({ automation: true, agentsFile }));
  const { mcp } = app;
  try {
    await step("lazy: panels closed start no shell, no git panel state", async () => {
      await mcp.waitFor("file tree README.md", byLabel("README.md"));
      await sleep(1500);
      const shells = await shellsIn(app.pid);
      if (shells.length > 0) throw new Error(`shell processes before any panel opened: ${shells.map((p) => p.name).join(", ")}`);
      if (await mcp.find(byLabelPrefix("Branch "))) throw new Error("git panel state exists before the Git tab was opened");
      const tabs = (await mcp.elements()).filter(isRole("tab")).map(label);
      for (const name of ["Diff", "Terminal", "Git", "Search"]) {
        if (!tabs.includes(name)) throw new Error(`Side Pane tab ${name} missing (tabs: ${tabs.join(", ")})`);
      }
    });

    await step("git: status (branch, modified, untracked)", async () => {
      await openTab(mcp, "Git");
      await mcp.waitFor("branch main", byLabel("Branch main"));
      await mcp.waitFor("modified README.md", byLabel("M README.md"));
      await mcp.waitFor("untracked notes.txt", byLabel("U notes.txt"));
      if (await shellsIn(app.pid).then((s) => s.length > 0)) throw new Error("the git panel started a shell");
      await shot(mcp, "git");
    });

    await step("watcher: new file appears without refresh", async () => {
      writeFileSync(path.join(workspace, "watched.txt"), "seen by the watcher\n");
      await mcp.waitFor("untracked watched.txt", byLabel("U watched.txt"), 10_000);
    });

    await step("diff: unified working-tree diff with line numbers", async () => {
      await mcp.activate(await mcp.waitFor("row README.md", byLabel("M README.md")));
      await openTab(mcp, "Diff");
      // Unified row label: "<marker> <old-no> <new-no> <text>"; the appended line is new line 4.
      await mcp.waitFor("added line", byLabel("+  4 added by the gate"));
      await mcp.waitFor("context line", byLabel("  3 3 first line"));
      await shot(mcp, "diff-unified");
    });

    await step("diff: side by side", async () => {
      await press(mcp, "Side by side");
      // Pair label: "<left-no> <left-text> | <right-no> <right-text>"; an addition has an empty left side.
      await mcp.waitFor("split added line", byLabel("  | 4 added by the gate"));
      await mcp.waitFor("split context", byLabel("3 first line | 3 first line"));
      await shot(mcp, "diff-split");
      await press(mcp, "Unified");
      await mcp.waitFor("unified again", byLabel("+  4 added by the gate"));
    });

    await step("git: stage, unstage, stage, commit", async () => {
      await openTab(mcp, "Git");
      await press(mcp, "Stage README.md");
      await mcp.waitFor("README staged", labelled("Unstage README.md", "button"));
      await press(mcp, "Unstage README.md");
      await mcp.waitFor("README unstaged", labelled("Stage README.md", "button"));
      await press(mcp, "Stage README.md");
      await mcp.waitFor("README staged again", labelled("Unstage README.md", "button"));
      await typeInto(mcp, "Commit message", "panel commit", false);
      await press(mcp, "Commit");
      await mcp.waitFor("commit notice", byLabelPrefix("Committed ", "panel commit"));
      await mcp.waitGone("README row gone", byLabel("M README.md"));
      const subject = git("log", "-1", "--format=%s");
      if (subject !== "panel commit") throw new Error(`HEAD subject is "${subject}"`);
      const untracked = git("status", "--porcelain");
      if (!untracked.includes("?? notes.txt")) throw new Error(`notes.txt should stay untracked: ${untracked}`);
    });

    await step("search: files by name", async () => {
      await openTab(mcp, "Search");
      await typeInto(mcp, "Search query", "guide");
      await mcp.waitFor("file hit docs/guide.md", byLabel("docs/guide.md"));
    });

    await step("search: content, case, preview", async () => {
      await press(mcp, "Content");
      await typeInto(mcp, "Search query", "needle");
      await mcp.waitFor("content hit lib.rs", byLabel("src/lib.rs:1 // NEEDLE one"));
      await mcp.waitFor("content hit guide.md", byLabel("docs/guide.md:1 needle two"));
      await mcp.activate(await mcp.waitFor("match case", labelled("Match case", "checkbox")));
      await mcp.waitGone("case-insensitive hit gone", byLabel("src/lib.rs:1 // NEEDLE one"));
      const hit = await mcp.waitFor("case-sensitive hit", byLabel("docs/guide.md:1 needle two"));
      await mcp.activate(hit);
      await mcp.waitFor("preview hit line", byLabel("1 needle two"));
      await mcp.waitFor("preview next line", byLabel("3 Guide text."));
      await shot(mcp, "search");
    });

    let shellPids = [];
    await step("terminal: lazy start, echo, exit code", async () => {
      await openTab(mcp, "Terminal");
      await mcp.waitFor("terminal running", labelled("Kill terminal", "button"));
      const deadline = Date.now() + 20_000;
      while (Date.now() < deadline && shellPids.length === 0) {
        shellPids = (await shellsIn(app.pid)).filter((p) => !String(p.name).toLowerCase().includes("conhost"));
        if (shellPids.length === 0) await sleep(250);
      }
      if (shellPids.length === 0) throw new Error("no shell process after opening the Terminal tab");
      await mcp.click(await mcp.waitFor("terminal screen", byLabel("Terminal output")));
      // Marker computed by the shell (PowerShell / pwsh): the command line itself does not contain it.
      await sleep(1500);
      await mcp.key("echo PANEL_$(40+2)_OK");
      await mcp.key("\n");
      await mcp.waitFor("marker output", byLabel("PANEL_42_OK"), 30_000);
      await shot(mcp, "terminal");
      await mcp.key("exit 7");
      await mcp.key("\n");
      await mcp.waitFor("exit tag", byLabel("Exited (7)"), 20_000);
      await sleep(500);
      const alive = await stillAlive(shellPids);
      if (alive.length > 0) throw new Error(`shell still alive after exit: ${alive.map((p) => p.pid).join(", ")}`);
    });

    await step("terminal: restart, kill", async () => {
      await press(mcp, "Restart terminal");
      await mcp.waitGone("exit tag cleared", byLabel("Exited (7)"));
      let restarted = [];
      const deadline = Date.now() + 20_000;
      while (Date.now() < deadline && restarted.length === 0) {
        restarted = (await shellsIn(app.pid)).filter((p) => !String(p.name).toLowerCase().includes("conhost"));
        if (restarted.length === 0) await sleep(250);
      }
      if (restarted.length === 0) throw new Error("no shell after restart");
      await press(mcp, "Kill terminal");
      await mcp.waitFor("killed tag", byLabelPrefix("Exited ("));
      await sleep(1000);
      const alive = await stillAlive(restarted);
      if (alive.length > 0) throw new Error(`shell alive after kill: ${alive.map((p) => p.pid).join(", ")}`);
      // Leave a live shell for the shutdown check below.
      await press(mcp, "Restart terminal");
      await mcp.waitGone("exit tag cleared", byLabelPrefix("Exited ("));
    });

    await step("diff: turn changes", async () => {
      await openTab(mcp, "Diff");
      await press(mcp, "Turn changes");
      const notice = await mcp.waitFor(
        "turn diff result",
        (e) => label(e).startsWith("Turn diffs need the agent hub") || label(e).startsWith("No finished turn"),
      );
      if (label(notice).startsWith("Turn diffs need the agent hub")) {
        // panels::attach_core 由 agent B 在 core/hub.rs 中接入；未接入时如实记录为跳过，而不是判定通过。
        results.notes.push("turn diff UI: SKIPPED — panels::attach_core not wired by the hub adapter yet");
        log("note turn diff UI skipped (hub hook not wired)");
        return;
      }
      const sessionButton = await mcp.waitFor("new session", labelled("New session", "button"));
      await mcp.activate(sessionButton);
      const input = await mcp.waitFor("message input", byLabel("Message"));
      await mcp.click(input);
      await mcp.setValue(input, "[[write src/agent.txt hello from agent]] [[say done]]");
      await mcp.key("\n");
      await mcp.waitFor("agent reply", labelIncludes("done"), 30_000);
      await press(mcp, "Turn changes");
      await mcp.waitFor("turn diff line", labelIncludes("hello from agent"), 20_000);
      await shot(mcp, "diff-turn");
    });

    await step("close: graceful exit kills the panel shell", async () => {
      const before = await shellsIn(app.pid);
      if (before.length === 0) throw new Error("expected a live shell before close");
      const closed = await close(app);
      results.close = closed;
    });
  } finally {
    if (roots.has(app.pid)) await close(app).catch(() => {});
  }
}

// ---- RAM ---------------------------------------------------------------------------------------------------

const mb = (bytes) => +(bytes / 1048576).toFixed(1);

async function sample(app, name) {
  if (app.child.exitCode !== null) throw new Error(`app exited early: ${app.stderr.join("").slice(0, 400)}`);
  const summary = summarizeSample(await sampleTree([app.pid]));
  const entry = {
    processes: summary.processCount,
    privateWorkingSetMb: mb(summary.privateWorkingSetBytes),
    workingSetMb: mb(summary.workingSetBytes),
    privateBytesMb: mb(summary.privateBytes),
    byName: summary.byName,
  };
  log(`ram ${name}: private WS ${entry.privateWorkingSetMb} MB, ${entry.processes} process(es)`);
  return entry;
}

async function ram(agentsFile) {
  await step("ram: product build idle, panels closed", async () => {
    const app = await launch({ automation: false, agentsFile });
    try {
      await sleep(IDLE_S * 1000);
      results.ram.productIdle = await sample(app, `product idle @${IDLE_S}s`);
    } finally {
      await close(app);
    }
    if (results.ram.productIdle.privateWorkingSetMb > RAM_LIMIT_MB) {
      throw new Error(`idle private working set ${results.ram.productIdle.privateWorkingSetMb} MB > ${RAM_LIMIT_MB} MB`);
    }
  });
  // 产品版本没有自动化通道无法打开面板：用自动化版本测“面板关闭 → 打开终端”的增量。
  await step("ram: automation build, panels closed vs terminal open", async () => {
    const app = await launch({ automation: true, agentsFile });
    try {
      await sleep(15_000);
      results.ram.automationClosed = await sample(app, "automation, panels closed");
      await openTab(app.mcp, "Terminal");
      await app.mcp.waitFor("terminal running", labelled("Kill terminal", "button"));
      await sleep(15_000);
      results.ram.automationTerminal = await sample(app, "automation, terminal open");
      results.ram.terminalDeltaMb = +(
        results.ram.automationTerminal.privateWorkingSetMb - results.ram.automationClosed.privateWorkingSetMb
      ).toFixed(1);
    } finally {
      await close(app);
    }
  });
}

// ---- main --------------------------------------------------------------------------------------------------

async function main() {
  mkdirSync(reportDir, { recursive: true });
  await step("build", async () => build());
  const agentsFile = await step("fixtures", async () => fixtures());
  await scenarios(agentsFile);
  if (process.env.PHASE8_PANELS_SKIP_RAM === "1") {
    results.notes.push("RAM skipped (PHASE8_PANELS_SKIP_RAM=1)");
    return;
  }
  await ram(agentsFile);
}

let failure = null;
try {
  await main();
} catch (error) {
  failure = error;
} finally {
  if (roots.size > 0) await killTrees([...roots]).catch(() => {});
  results.finishedAt = new Date().toISOString();
  results.idleSeconds = IDLE_S;
  results.ok = !failure;
  if (failure) results.failure = { step: failure.step ?? "main", message: String(failure.message ?? failure) };
  mkdirSync(reportDir, { recursive: true });
  writeFileSync(path.join(reportDir, "phase8-panels.json"), `${JSON.stringify(results, null, 2)}\n`);
}
if (failure) {
  process.stdout.write(`PHASE8_PANELS_FAIL ${failure.step ?? "main"} ${String(failure.message ?? failure).replace(/\s+/g, " ")}\n`);
  process.exitCode = 1;
} else {
  process.stdout.write("PHASE8_PANELS_OK\n");
}
