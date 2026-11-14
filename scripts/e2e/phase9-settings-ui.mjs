#!/usr/bin/env node
// Phase 9.1 settings gate, UI half (agent B). Spec: Worse/docs/specs/harness/phase-9-parity.md §6.7.
// The core half is phase9-settings.mjs (agent A, PHASE9_SETTINGS_CORE_OK).
//
//   node Worse/scripts/e2e/phase9-settings-ui.mjs
//   PHASE9_UI_SKIP_BUILD=1   reuse the last build in Worse/.work/tmp/phase9-ui-bin
//   PHASE9_UI_SKIP_RAM=1     scenarios only (development; the result then does not cover RAM)
//   PHASE9_UI_IDLE_S=<s>     idle period before each RAM sample (default 60)
//
// Prints PHASE9_SETTINGS_UI_OK or PHASE9_SETTINGS_UI_FAIL <step> <reason> as the last line; report in
// Worse/.work/phase9/phase9-settings-ui.json, screenshots next to it.
//
// Safety: installIsolatedHome() runs first. HARNESS_HOME, CLAUDE_CONFIG_DIR, CODEX_HOME and the workspace live in the
// isolated home and hold dummy fixtures only (no credentials). Agents are harness-fake-agent processes; no real agent
// CLI is involved. Only process trees rooted at PIDs this gate spawned are inspected or stopped.
import { installIsolatedHome } from "./lib/isolatedHome.mjs";

const { home } = installIsolatedHome("phase9-settings-ui");

const { spawn, spawnSync } = await import("node:child_process");
const { copyFileSync, existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync } = await import("node:fs");
const net = await import("node:net");
const os = (await import("node:os")).default;
const path = (await import("node:path")).default;
const process = (await import("node:process")).default;
const { fileURLToPath } = await import("node:url");
const { SlintMcp, assertNoRowOverlap, byLabel, byLabelPrefix, label, sleep } = await import("./phase8-ui/mcp.mjs");
const { killTrees, processTree, sampleTree, stillAlive, summarizeSample } = await import("./phase7-perf/procs.mjs");
const { missingMsgids, usedMsgids } = await import("./phase9-settings/po-check.mjs");

const worse = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const reportDir = path.join(worse, ".work/phase9");
const binDir = path.join(worse, ".work/tmp/phase9-ui-bin");
const exeName = (name) => (process.platform === "win32" ? `${name}.exe` : name);
const IDLE_S = Number(process.env.PHASE9_UI_IDLE_S ?? 60);
const RAM_LIMIT_MB = 50;
const RAM_GROWTH_MB = 5;

const harnessHome = path.join(home, ".harness");
const workspace = path.join(home, "workspace");
const fakeState = path.join(home, "fake-agent-state");
const claudeDir = process.env.CLAUDE_CONFIG_DIR;
const codexDir = process.env.CODEX_HOME;
const settingsFile = path.join(harnessHome, "settings.json");
const claudeJson = path.join(claudeDir, ".claude.json");
const codexToml = path.join(codexDir, "config.toml");
const roots = new Set();
const results = { startedAt: new Date().toISOString(), steps: [], ram: {}, screenshots: [], pages: {} };
let currentMcp = null;

// Same ids and labels as SettingsShell (NAV in src/settings/mod.rs, Labels.page-label).
const { PAGES } = await import("./phase9-settings/pages.mjs");

class StepError extends Error {
  constructor(step, message) {
    super(message);
    this.step = step;
  }
}

const log = (line) => process.stdout.write(`[phase9-ui] ${line}\n`);

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
      await shot(currentMcp, `fail-${name.replace(/[^a-z0-9]+/gi, "-")}`).catch(() => {});
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
  if (process.env.PHASE9_UI_SKIP_BUILD !== "1") {
    cargo(["build", "--release", "-p", "harness-fake-agent", "-p", "harness-agent-hub", "--bins"]);
    copy("harness-fake-agent");
    copy("harness-memory-mcp");
    cargo(["build", "--release", "-p", "harness-slint", "--features", "automation"]);
    copy("harness-slint", "harness-slint-automation");
    cargo(["build", "--release", "-p", "harness-slint"]);
    copy("harness-slint");
  }
  for (const name of ["harness-fake-agent", "harness-memory-mcp", "harness-slint", "harness-slint-automation"]) {
    if (!existsSync(path.join(binDir, exeName(name)))) throw new Error(`${name} missing in ${binDir}`);
  }
}

function fixtures() {
  mkdirSync(path.join(workspace, "src"), { recursive: true });
  writeFileSync(path.join(workspace, "README.md"), "# demo workspace\n");
  mkdirSync(fakeState, { recursive: true });
  mkdirSync(harnessHome, { recursive: true });
  mkdirSync(claudeDir, { recursive: true });
  mkdirSync(codexDir, { recursive: true });
  // Unknown keys everywhere: Harness must keep them on every write.
  writeFileSync(settingsFile, `${JSON.stringify({ gateUnknown: { keep: true }, general: { gateUnknownField: 7 } }, null, 2)}\n`);
  writeFileSync(claudeJson, `${JSON.stringify({ gateUnknownKey: 42, mcpServers: {} }, null, 2)}\n`);
  writeFileSync(codexToml, 'model = "gate-model"\n\n[gate_unknown]\nkeep = true\n');
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

async function launch({ automation, agentsFile, size }) {
  const exe = path.join(binDir, exeName(automation ? "harness-slint-automation" : "harness-slint"));
  const env = {
    ...process.env,
    HARNESS_HOME: harnessHome,
    HARNESS_WORKSPACE: workspace,
    HARNESS_UI_AGENTS: agentsFile,
    HARNESS_MEMORY_MCP: path.join(binDir, exeName("harness-memory-mcp")),
    GIT_CEILING_DIRECTORIES: home,
  };
  for (const key of ["SLINT_MCP_PORT", "SLINT_BACKEND", "HARNESS_UI_CORE", "HARNESS_UI_WINDOW_SIZE"]) delete env[key];
  if (size) env.HARNESS_UI_WINDOW_SIZE = size;
  // Live update: the gate tracks this PID and its stderr, so run in place (no shadow copy, no parent exit).
  env.HARNESS_NO_SHADOW = "1";
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

// ---- UI helpers --------------------------------------------------------------------------------------------

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
/** Waits briefly for an element, then scrolls the settings content to find it (below-the-fold rows). */
async function locate(mcp, what, predicate) {
  try {
    return await mcp.waitFor(what, predicate, 3000);
  } catch {
    return reveal(mcp, what, predicate);
  }
}
const press = async (mcp, text) => mcp.activate(await locate(mcp, `button "${text}"`, isButton(text)));
/** A page title / heading: same label as its nav button, but not a button. */
const title = (text) => (e) => label(e) === text && e.accessibleRole !== "Button";

async function chord(mcp, modifiers, key) {
  const codes = { ctrl: "\u0011", alt: "\u0012", shift: "\u0010" };
  for (const m of modifiers) await mcp.key(codes[m], "Press");
  await mcp.key(key);
  for (const m of [...modifiers].reverse()) await mcp.key(codes[m], "Release");
}

async function openSettings(mcp) {
  // Opened through the default openSettings binding (Ctrl+,): exercises the global shortcut dispatch too.
  await chord(mcp, ["ctrl"], ",");
  await mcp.waitFor("settings shell", byLabel("Back to workspace"), 10_000);
}

async function closeSettings(mcp) {
  await mcp.key("\u001b");
  await mcp.waitGone("settings shell", byLabel("Back to workspace"));
}

/**
 * Finds an element that may sit below the fold of the settings ScrollView (Slint only exposes what is laid out in
 * the viewport): wheel-scroll the content column down step by step until it appears.
 */
async function reveal(mcp, what, predicate, { x = 700, y = 420, steps = 24 } = {}) {
  let hit = await mcp.find(predicate);
  // Down first (rows below the fold), then back up past the start (rows above after an earlier scroll).
  for (let i = 0; !hit && i < steps * 3; i++) {
    await mcp.scrollAt(x, y, i < steps ? -120 : 120);
    await sleep(120);
    hit = await mcp.find(predicate);
  }
  if (!hit) throw new Error(`${what} not found after scrolling`);
  return hit;
}

async function openPage(mcp, text) {
  await press(mcp, text);
  await mcp.waitFor(`page title ${text}`, title(text));
}

/** Types into a field: set the value, then confirm it reached the element. */
async function fill(mcp, fieldLabel, value) {
  const field = await locate(mcp, `field ${fieldLabel}`, (e) => label(e) === fieldLabel && isRole(e, "textinput"));
  await mcp.click(field);
  await mcp.setValue(field, value);
  await sleep(150);
  const after = await mcp.find((e) => e.handle === field.handle);
  if (after && after.accessibleValue !== undefined && after.accessibleValue !== value) {
    throw new Error(`field ${fieldLabel} shows ${JSON.stringify(after.accessibleValue)} after set`);
  }
}

async function waitFile(what, file, predicate, timeoutMs = 10_000) {
  const deadline = Date.now() + timeoutMs;
  let last = null;
  while (Date.now() < deadline) {
    try {
      last = readFileSync(file, "utf8");
      if (predicate(last)) return last;
    } catch {
      // not written yet
    }
    await sleep(150);
  }
  throw new Error(`${what}: ${path.relative(home, file)} did not reach the expected content`);
}

const settingsJson = () => JSON.parse(readFileSync(settingsFile, "utf8"));
const waitSettings = (what, predicate) => waitFile(what, settingsFile, (text) => predicate(JSON.parse(text)));

async function writeThroughDiff(mcp) {
  await mcp.waitFor("diff dialog", byLabel("Changes"), 10_000);
  await press(mcp, "Write changes");
  await mcp.waitGone("diff dialog", byLabel("Changes"), 10_000);
}

function findFile(dir, name) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      const hit = findFile(full, name);
      if (hit) return hit;
    } else if (entry.name === name) {
      return full;
    }
  }
  return null;
}

// ---- scenarios ---------------------------------------------------------------------------------------------

async function firstRun(agentsFile) {
  const app = await step("start", () => launch({ automation: true, agentsFile }));
  const { mcp } = app;
  try {
    await step("chat overlap (§6.5)", async () => {
      await press(mcp, "New session");
      for (const text of ["first message", "second message"]) {
        const input = await mcp.waitFor("message input", byLabel("Message"));
        await mcp.click(input);
        await mcp.setValue(input, text);
        await mcp.key("\n");
        await mcp.waitFor(`reply to ${text}`, byLabelPrefix("Claude Code: ", `reply to: ${text}`), 30_000);
      }
      await sleep(400);
      results.chatRowsChecked = await assertNoRowOverlap(mcp);
    });

    await step("every page opens (wide)", async () => {
      await openSettings(mcp);
      for (const [id, text] of PAGES) {
        await openPage(mcp, text);
        await sleep(200);
        results.pages[id] = { wide: true };
        await shot(mcp, `page-${id}`);
      }
    });

    await step("round trip: switch, segmented, stepper, text", async () => {
      await openPage(mcp, "General");
      await fill(mcp, "Terminal font", "Gate Mono");
      await press(mcp, "Save Terminal font");
      await waitSettings("terminal font", (s) => s.general?.terminal?.fontFamily === "Gate Mono");
      const sw = await reveal(mcp, "Show reasoning switch", (e) => label(e) === "Show reasoning" && isRole(e, "checkbox"));
      const before = Boolean(sw.accessibleChecked);
      await mcp.activate(sw);
      await waitSettings("showReasoning", (s) => s.general?.showReasoning === !before);
      results.showReasoning = !before;
      await mcp.activate(await reveal(mcp, "Steer", isButton("Steer")));
      await waitSettings("followUp", (s) => s.general?.followUp === "steer");
      await openPage(mcp, "Appearance");
      const zoom = await mcp.waitFor("Zoom stepper", (e) => label(e) === "Zoom" && isRole(e, "spinbox"));
      await mcp.tool("invoke_accessibility_action", { elementHandle: zoom.handle, action: "Increment" });
      await waitSettings("zoom", (s) => s.appearance?.zoomPercent === 110);
      const s = settingsJson();
      if (s.gateUnknown?.keep !== true || s.general?.gateUnknownField !== 7) throw new Error("unknown settings keys were dropped");
    });

    await step("agent config: MCP add (Claude Code, Codex), raw edit, delete", async () => {
      await openPage(mcp, "MCP Servers");
      await press(mcp, "Claude Code");
      await press(mcp, "User");
      await press(mcp, "Add server");
      await fill(mcp, "Name", "gate-mcp");
      await fill(mcp, "Command", "gate-server");
      await press(mcp, "Review changes");
      await shot(mcp, "diff-mcp-claude");
      await writeThroughDiff(mcp);
      await waitFile("claude mcp", claudeJson, (t) => {
        const j = JSON.parse(t);
        return j.mcpServers?.["gate-mcp"]?.command === "gate-server" && j.gateUnknownKey === 42;
      });
      await mcp.waitFor("claude list row", isButton("Edit gate-mcp"));

      await press(mcp, "Codex");
      await press(mcp, "Add server");
      await fill(mcp, "Name", "gate-mcp");
      await fill(mcp, "Command", "gate-server");
      await press(mcp, "Review changes");
      await writeThroughDiff(mcp);
      await waitFile("codex mcp", codexToml, (t) => t.includes("gate-mcp") && t.includes("gate-server") && t.includes("[gate_unknown]"));

      // Raw editor: whole source file, edited and written through the same diff confirmation.
      await press(mcp, "Edit gate-mcp");
      await press(mcp, "Raw");
      const raw = await mcp.waitFor("raw editor", (e) => label(e) === "Raw configuration" && isRole(e, "textinput"));
      const deadline = Date.now() + 5000;
      let text = raw.accessibleValue ?? "";
      while (!text.includes("gate-server") && Date.now() < deadline) {
        await sleep(150);
        text = (await mcp.find((e) => e.handle === raw.handle))?.accessibleValue ?? "";
      }
      if (!text.includes("gate-server")) throw new Error("raw editor did not load the source file");
      await mcp.setValue(raw, text.replace("gate-server", "gate-server-raw"));
      await press(mcp, "Review changes");
      await writeThroughDiff(mcp);
      await waitFile("codex raw edit", codexToml, (t) => t.includes("gate-server-raw"));

      await press(mcp, "Delete gate-mcp");
      await writeThroughDiff(mcp);
      await waitFile("codex delete", codexToml, (t) => !t.includes("gate-mcp") && t.includes("[gate_unknown]"));
      await mcp.waitGone("codex row", isButton("Edit gate-mcp"));
    });

    await step("agy: Unsupported where the backend says so", async () => {
      const unsupported = [];
      for (const [, text] of PAGES.filter(([id]) => ["subagent", "mcp", "skill", "command", "hook"].includes(id))) {
        await openPage(mcp, text);
        await press(mcp, "agy");
        await sleep(400);
        if (await mcp.find(byLabelPrefix("agy does not support"))) unsupported.push(text);
      }
      results.agyUnsupported = unsupported;
      if (unsupported.length === 0) throw new Error("no concept shows the agy Unsupported state");
      await shot(mcp, "agy-unsupported");
    });

    await step("instructions + memory: edit → diff → write", async () => {
      await openPage(mcp, "Instructions");
      await press(mcp, "Claude Code");
      await press(mcp, "User");
      await fill(mcp, "Instructions content", "# gate instructions\n\nbe brief\n");
      await press(mcp, "Save instructions");
      await writeThroughDiff(mcp);
      await waitFile("CLAUDE.md", path.join(claudeDir, "CLAUDE.md"), (t) => t.includes("gate instructions"));

      await openPage(mcp, "Memory");
      await press(mcp, "Global");
      await fill(mcp, "Memory content", "- gate memory note\n");
      await press(mcp, "Save memory");
      await writeThroughDiff(mcp);
      const deadline = Date.now() + 10_000;
      let file = null;
      while (!file && Date.now() < deadline) {
        file = findFile(harnessHome, "MEMORY.md");
        if (!file) await sleep(200);
      }
      if (!file) throw new Error("MEMORY.md was not written under HARNESS_HOME");
      await waitFile("MEMORY.md", file, (t) => t.includes("gate memory note"));
      results.memoryFile = path.relative(home, file).replaceAll("\\", "/");
    });

    await step("shortcuts: rebind, reserved, occupied + steal, reset", async () => {
      await openPage(mcp, "Keyboard Shortcuts");
      await press(mcp, "Rebind Toggle Sidebar");
      await chord(mcp, ["ctrl", "alt"], "k");
      await waitSettings("toggleSidebar", (s) => JSON.stringify(s.shortcuts?.toggleSidebar) === JSON.stringify(["CmdOrCtrl+Alt+k"]));

      await press(mcp, "Rebind Toggle Terminal");
      await chord(mcp, ["ctrl"], "c");
      await locate(mcp, "reserved error", byLabel("This combination is reserved by the system"));
      await chord(mcp, ["ctrl"], "n");
      await locate(mcp, "occupied error", byLabel("Already used by “New Task”"));
      await shot(mcp, "shortcut-conflict");
      await press(mcp, "Rebind anyway (current command loses it)");
      await waitSettings("steal", (s) => JSON.stringify(s.shortcuts?.toggleTerminal) === JSON.stringify(["CmdOrCtrl+n"]) && Array.isArray(s.shortcuts?.newTask) && s.shortcuts.newTask.length === 0);

      // A global binding dispatches through the new keymap: Ctrl+Alt+K is now Toggle Sidebar, Ctrl+, still opens
      // settings (checked by openSettings), Esc closes.
      await press(mcp, "Reset all to defaults");
      await mcp.waitFor("reset confirmation", byLabel("Reset all shortcuts to defaults?"));
      // 页面按钮与确认对话框按钮同名：对话框在元素树中靠后，取最后一个。
      const confirms = (await mcp.elements()).filter(isButton("Reset all to defaults"));
      await mcp.activate(confirms[confirms.length - 1]);
      await waitSettings("reset", (s) => Object.keys(s.shortcuts ?? {}).length === 0);
      // 重启后验证持久化：再绑定一次。
      await press(mcp, "Rebind Toggle Sidebar");
      await chord(mcp, ["ctrl", "alt"], "k");
      await waitSettings("toggleSidebar again", (s) => JSON.stringify(s.shortcuts?.toggleSidebar) === JSON.stringify(["CmdOrCtrl+Alt+k"]));
      await closeSettings(mcp);
    });

    await step("agents: overview + connection test (hub test_agent)", async () => {
      await openSettings(mcp);
      await openPage(mcp, "Agents");
      await mcp.activate(await reveal(mcp, "Test Claude Code", isButton("Test Claude Code")));
      await reveal(mcp, "test result", byLabelPrefix("Connected in"), { steps: 4 });
      await closeSettings(mcp);
    });

    await step("projects + archive (hub project / archive API)", async () => {
      // 侧栏归档：会话行的归档按钮 → hub archive_sessions → 归档页可见 → 取消归档后回到侧栏。
      const archiveBtn = await mcp.waitFor("sidebar archive button", (e) => isRole(e, "button") && String(label(e) ?? "").startsWith("Archive "));
      const archivedTitle = label(archiveBtn).slice("Archive ".length);
      await mcp.activate(archiveBtn);
      await mcp.waitGone("archived session row", (e) => isRole(e, "listitem") && label(e) === archivedTitle);
      await openSettings(mcp);
      await openPage(mcp, "Projects");
      await mcp.waitFor("project row", (e) => isRole(e, "checkbox") && String(label(e) ?? "").startsWith("Select "));
      if (await mcp.find(byLabel("Project management needs the hub project API"))) throw new Error("projects page fell back to unavailable");
      await shot(mcp, "projects-hub");
      await openPage(mcp, "Archive management");
      await press(mcp, "Unarchive");
      await mcp.waitFor("archive empty", byLabel("No archived sessions"));
      await closeSettings(mcp);
      await mcp.waitFor("unarchived session back in sidebar", (e) => isRole(e, "listitem") && label(e) === archivedTitle);
      results.archivedRoundTrip = archivedTitle;
    });
  } finally {
    results.firstRun = await close(app);
  }
}

async function restart(agentsFile) {
  const app = await step("restart", () => launch({ automation: true, agentsFile }));
  const { mcp } = app;
  try {
    await step("restart restores every field", async () => {
      await openSettings(mcp);
      await openPage(mcp, "General");
      const font = await mcp.waitFor("Terminal font", (e) => label(e) === "Terminal font" && isRole(e, "textinput"));
      if (font.accessibleValue !== undefined && font.accessibleValue !== "Gate Mono") throw new Error(`terminal font shows ${font.accessibleValue}`);
      const sw = await reveal(mcp, "Show reasoning switch", (e) => label(e) === "Show reasoning" && isRole(e, "checkbox"));
      if (Boolean(sw.accessibleChecked) !== results.showReasoning) throw new Error("Show reasoning not restored");
      const steer = await reveal(mcp, "Steer", isButton("Steer"));
      if (!steer.accessibleChecked) throw new Error("follow-up Steer not restored");
      await openPage(mcp, "Appearance");
      const zoom = await mcp.waitFor("Zoom stepper", (e) => label(e) === "Zoom" && isRole(e, "spinbox"));
      if (zoom.accessibleValue !== undefined && !String(zoom.accessibleValue).includes("110")) throw new Error(`zoom shows ${zoom.accessibleValue}`);
      await openPage(mcp, "MCP Servers");
      await press(mcp, "Claude Code");
      await mcp.waitFor("claude gate-mcp after restart", isButton("Edit gate-mcp"));
      await openPage(mcp, "Instructions");
      await press(mcp, "Claude Code");
      await mcp.waitFor("instructions after restart", (e) => label(e) === "Instructions content" && String(e.accessibleValue ?? "").includes("gate instructions"));
      await openPage(mcp, "Memory");
      await mcp.waitFor("memory after restart", (e) => label(e) === "Memory content" && String(e.accessibleValue ?? "").includes("gate memory note"));
      await openPage(mcp, "Keyboard Shortcuts");
      await locate(mcp, "rebound binding shown", (e) => label(e) === "Ctrl+Alt+K");
      await closeSettings(mcp);
    });
  } finally {
    results.secondRun = await close(app);
  }
}

async function narrow(agentsFile) {
  const app = await step("narrow: start 420x800", () => launch({ automation: true, agentsFile, size: "420x800" }));
  const { mcp } = app;
  try {
    await step("every page opens (narrow 420 px)", async () => {
      await openSettings(mcp);
      await shot(mcp, "narrow-nav");
      for (const [id, text] of PAGES) {
        await openPage(mcp, text);
        results.pages[id].narrow = true;
        await press(mcp, "All settings");
        await mcp.waitFor("nav list", isButton("General"));
      }
      await openPage(mcp, "Agents");
      await shot(mcp, "narrow-agents");
      await closeSettings(mcp);
    });
  } finally {
    results.narrowRun = await close(app);
  }
}

// ---- RAM ---------------------------------------------------------------------------------------------------

const mb = (bytes) => +(bytes / 1048576).toFixed(1);

async function sample(app, tag) {
  const summary = summarizeSample(await sampleTree([app.pid]));
  const value = { processes: summary.processCount, privateWorkingSetMb: mb(summary.privateWorkingSetBytes), workingSetMb: mb(summary.workingSetBytes) };
  log(`ram ${tag}: private WS ${value.privateWorkingSetMb} MB, ${value.processes} process(es)`);
  // 修复（门禁假阳性）：进程已退出或采样失败时得到 0 MB / 0 进程，原先会被当作“低于上限”而通过。
  // 没有采到存活进程即判失败，并带上退出码与 stderr 便于定位。
  if (value.processes === 0) {
    const code = await Promise.race([app.exited, sleep(200).then(() => "running")]);
    throw new Error(`ram ${tag}: no live process sampled (exit ${code}); stderr: ${app.stderr.join("").slice(-800)}`);
  }
  return value;
}

async function ram(agentsFile) {
  await step("ram: product build idle", async () => {
    const app = await launch({ automation: false, agentsFile });
    try {
      await sleep(IDLE_S * 1000);
      results.ram.product = await sample(app, "product idle");
    } finally {
      await close(app);
    }
    if (results.ram.product.privateWorkingSetMb > RAM_LIMIT_MB) throw new Error(`product idle ${results.ram.product.privateWorkingSetMb} MB > ${RAM_LIMIT_MB} MB`);
  });
  // 产品版无法被驱动打开设置页；“打开所有页面再关闭”的增量用自动化版测（同一进程前后对比）。
  await step("ram: settings opened through every page, closed, idle", async () => {
    const app = await launch({ automation: true, agentsFile });
    try {
      await sleep(IDLE_S * 1000);
      const before = await sample(app, "automation idle, settings never opened");
      await openSettings(app.mcp);
      for (const [, text] of PAGES) await openPage(app.mcp, text);
      await closeSettings(app.mcp);
      await sleep(IDLE_S * 1000);
      const after = await sample(app, "automation idle after settings");
      results.ram.settings = { before, after, growthMb: +(after.privateWorkingSetMb - before.privateWorkingSetMb).toFixed(1) };
      if (after.privateWorkingSetMb > RAM_LIMIT_MB) throw new Error(`idle after settings ${after.privateWorkingSetMb} MB > ${RAM_LIMIT_MB} MB`);
      if (results.ram.settings.growthMb > RAM_GROWTH_MB) throw new Error(`settings left +${results.ram.settings.growthMb} MB > ${RAM_GROWTH_MB} MB`);
    } finally {
      await close(app);
    }
  });
}

// ---- main --------------------------------------------------------------------------------------------------

async function main() {
  mkdirSync(reportDir, { recursive: true });
  // 用户决定 UI 只用英文、不再新增 zh-CN 翻译（2026-11-03）：缺少的 msgid 只记录数量，不再让门禁失败。
  await step("po: zh-CN catalog (report only)", async () => {
    const missing = missingMsgids();
    results.po = { msgids: usedMsgids().size, missing: missing.length, reportOnly: true, sample: missing.slice(0, 5).map((m) => m.id) };
  });
  await step("build", async () => build());
  const agentsFile = fixtures();
  await firstRun(agentsFile);
  await restart(agentsFile);
  await narrow(agentsFile);
  if (process.env.PHASE9_UI_SKIP_RAM === "1") return;
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
  results.ok = !failure;
  if (failure) results.failure = { step: failure.step ?? "main", message: String(failure.message ?? failure) };
  mkdirSync(reportDir, { recursive: true });
  writeFileSync(path.join(reportDir, "phase9-settings-ui.json"), `${JSON.stringify(results, null, 2)}\n`);
}
if (failure) {
  process.stdout.write(`PHASE9_SETTINGS_UI_FAIL ${failure.step ?? "main"} ${String(failure.message ?? failure).replace(/\s+/g, " ")}\n`);
  process.exitCode = 1;
} else {
  process.stdout.write("PHASE9_SETTINGS_UI_OK\n");
}
