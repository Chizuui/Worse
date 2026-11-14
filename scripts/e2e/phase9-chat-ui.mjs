#!/usr/bin/env node
// Phase 9.2 chat and workspace gate, UI half (agent B). Spec: Worse/docs/specs/harness/phase-9-parity.md §7.
// Every step names the inventory rows (.work/phase9/inventory.md) it asserts. The core half is agent A's gate.
//
//   node Worse/scripts/e2e/phase9-chat-ui.mjs
//   PHASE9_CHAT_SKIP_BUILD=1   reuse the last build in Worse/.work/tmp/phase9-chat-bin
//   PHASE9_CHAT_SKIP_RAM=1     scenarios only (development; the result then does not cover RAM)
//   PHASE9_CHAT_ONLY=round2|round3   one round's scenarios only (development)
//   PHASE9_CHAT_IDLE_S=<s>     idle period before each RAM sample (default 60)
//
// Prints PHASE9_CHAT_UI_OK or PHASE9_CHAT_UI_FAIL <step> <reason> as the last line; report in
// Worse/.work/phase9/phase9-chat-ui.json, screenshots next to it, side-by-side compare shots (ZCode left, Harness
// right, both light) in .work/phase9/compare-chat-<scene>.png when phase9-chat/zcode-reference.mjs has run.
//
// Safety: installIsolatedHome() runs first. HARNESS_HOME and the workspace live in the isolated home and hold dummy
// fixtures only. Agents are harness-fake-agent processes; no real agent CLI is involved. Only process trees rooted
// at PIDs this gate spawned are inspected or stopped. The copy steps write to the system clipboard (that is the
// feature under test) and read back only what Harness put there.
import { installIsolatedHome } from "./lib/isolatedHome.mjs";

const { home } = installIsolatedHome("phase9-chat-ui");

const { spawn, spawnSync } = await import("node:child_process");
const { copyFileSync, existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync } = await import("node:fs");
const net = await import("node:net");
const os = (await import("node:os")).default;
const path = (await import("node:path")).default;
const process = (await import("node:process")).default;
const { fileURLToPath } = await import("node:url");
const { SlintMcp, assertNoRowOverlap, byLabel, byLabelPrefix, label, role, sleep } = await import("./phase8-ui/mcp.mjs");
const { meanLuminance, sideBySide } = await import("./phase8-ui/png.mjs");
const { killTrees, processTree, sampleTree, stillAlive, summarizeSample } = await import("./phase7-perf/procs.mjs");
const { missingMsgids, usedMsgids } = await import("./phase9-settings/po-check.mjs");

const worse = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const reportDir = path.join(worse, ".work/phase9");
const refDir = path.join(reportDir, "ref");
const binDir = path.join(worse, ".work/tmp/phase9-chat-bin");
const exeName = (name) => (process.platform === "win32" ? `${name}.exe` : name);
const IDLE_S = Number(process.env.PHASE9_CHAT_IDLE_S ?? 60);
const RAM_LIMIT_MB = 50;
const RAM_GROWTH_MB = 5;
const LONG_PASTE = 16 * 1024;
// Slint key codes (Key.UpArrow / DownArrow / Escape / Return).
const KEY = { up: "", down: "", esc: "\u001b", enter: "\n" };

const harnessHome = path.join(home, ".harness");
const workspace = path.join(home, "workspace");
const fakeState = path.join(home, "fake-agent-state");
const settingsFile = path.join(harnessHome, "settings.json");
const roots = new Set();
const results = { startedAt: new Date().toISOString(), steps: [], rows: {}, ram: {}, screenshots: [], compare: [] };
let currentMcp = null;

class StepError extends Error {
  constructor(step, message) {
    super(message);
    this.step = step;
  }
}

const log = (line) => process.stdout.write(`[phase9-chat] ${line}\n`);

/** One gate step; `rows` are the inventory ids it asserts (recorded only when the step passes). */
async function step(name, rows, fn) {
  const started = Date.now();
  try {
    const value = await fn();
    results.steps.push({ name, rows, ok: true, ms: Date.now() - started });
    for (const row of rows) results.rows[row] = name;
    log(`ok   ${name}${rows.length ? ` [${rows.join(", ")}]` : ""} (${Date.now() - started} ms)`);
    return value;
  } catch (error) {
    results.steps.push({ name, rows, ok: false, ms: Date.now() - started, error: String(error?.message ?? error) });
    // 失败现场：记录可见元素标签和截图。
    if (currentMcp) {
      results.failureLabels = await currentMcp
        .elements()
        .then((list) => list.map((e) => `${e.accessibleRole ?? ""}:${label(e)}`).filter((l) => l.length > 1).slice(0, 250))
        .catch((e) => [String(e?.message ?? e)]);
      await shot(currentMcp, `chat-fail-${name.replace(/[^a-z0-9]+/gi, "-")}`).catch(() => {});
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
  if (process.env.PHASE9_CHAT_SKIP_BUILD !== "1") {
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
  writeFileSync(path.join(workspace, "src", "main.rs"), "fn main() {}\n");
  // 轮次文件快照（撤销 / 重新应用）存放在 git 对象库里：工作区需要是一个 git 仓库（只在隔离 home 内）。
  const git = (...args) => {
    const r = spawnSync("git", ["-c", "user.name=gate", "-c", "user.email=gate@example.invalid", ...args], { cwd: workspace, stdio: "ignore", windowsHide: true });
    if (r.status !== 0) throw new Error(`git ${args[0]} exited ${String(r.status)}`);
  };
  if (!existsSync(path.join(workspace, ".git"))) {
    git("init", "-q");
    git("add", "-A");
    git("commit", "-q", "-m", "fixture");
    // Round 3 git（GIT-007..011）：README 不同的 feature 分支（未提交修改会阻止切换）、120 个提交的 long 分支
    // （提交图翻页）和一个本地裸仓库作为 origin（推送）。全部位于隔离 home 内。
    const base = spawnSync("git", ["rev-parse", "HEAD"], { cwd: workspace, encoding: "utf8", windowsHide: true }).stdout.trim();
    git("checkout", "-q", "-b", "feature");
    writeFileSync(path.join(workspace, "README.md"), "# feature branch\n");
    git("commit", "-q", "-am", "feature work");
    git("checkout", "-q", "-");
    let stream = "";
    for (let i = 1; i <= 120; i += 1) {
      const msg = `long ${i}\n`;
      stream += `commit refs/heads/long\nmark :${i}\ncommitter gate <gate@example.invalid> ${1_700_000_000 + i} +0000\ndata ${Buffer.byteLength(msg)}\n${msg}`;
      stream += i === 1 ? `from ${base}\n\n` : `from :${i - 1}\n\n`;
    }
    const imported = spawnSync("git", ["fast-import", "--quiet"], { cwd: workspace, input: stream, windowsHide: true });
    if (imported.status !== 0) throw new Error(`git fast-import exited ${String(imported.status)}`);
    const bare = path.join(home, "origin.git");
    const init = spawnSync("git", ["init", "-q", "--bare", bare], { windowsHide: true });
    if (init.status !== 0) throw new Error(`git init --bare exited ${String(init.status)}`);
    git("remote", "add", "origin", bare);
    // 隔离 home 没有全局 git 身份：提交对话框的提交需要仓库级身份（只写入隔离工作区的 .git/config）。
    git("config", "user.name", "gate");
    git("config", "user.email", "gate@example.invalid");
  }
  // Round 3：工作区作用域的 Claude Code 技能与子 agent（@ 弹窗的 skill / subagent 候选）。
  const skillDir = path.join(workspace, ".claude", "skills", "gate-skill");
  mkdirSync(skillDir, { recursive: true });
  writeFileSync(path.join(skillDir, "SKILL.md"), "---\nname: gate-skill\ndescription: Gate skill fixture\n---\n\nSay hello.\n");
  mkdirSync(path.join(workspace, ".claude", "agents"), { recursive: true });
  writeFileSync(path.join(workspace, ".claude", "agents", "gate-reviewer.md"), "---\nname: gate-reviewer\ndescription: Gate subagent fixture\n---\n\nReview things.\n");
  mkdirSync(fakeState, { recursive: true });
  mkdirSync(harnessHome, { recursive: true });
  // Same theme as the ZCode reference shots (light) so the compare pictures differ in layout only.
  writeFileSync(settingsFile, `${JSON.stringify({ appearance: { theme: "light" } }, null, 2)}\n`);
  const agent = (id, name) => ({
    id,
    name,
    program: path.join(binDir, exeName("harness-fake-agent")),
    args: ["--name", id],
    env: { FAKE_AGENT_STATE: fakeState, FAKE_AGENT_LOG: path.join(fakeState, "log.jsonl") },
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

async function launch({ automation, agentsFile, size, extraEnv = {} }) {
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
  Object.assign(env, extraEnv);
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

/** Harness half of a compare picture: must be light like the ZCode reference; composed when ZCode's shot exists. */
async function compareShot(mcp, scene) {
  await sleep(400);
  const png = await shot(mcp, `chat-${scene}`);
  const lum = meanLuminance(png);
  if (lum < 150) throw new Error(`compare ${scene}: Harness shot is not light (mean luminance ${lum.toFixed(0)})`);
  mkdirSync(refDir, { recursive: true });
  writeFileSync(path.join(refDir, `harness-chat-${scene}.png`), png);
  const zcode = path.join(refDir, `zcode-chat-${scene}.png`);
  if (!existsSync(zcode)) {
    results.compare.push({ scene, composed: false, reason: "no ZCode reference (run phase9-chat/zcode-reference.mjs)" });
    return;
  }
  const out = path.join(reportDir, `compare-chat-${scene}.png`);
  writeFileSync(out, sideBySide(readFileSync(zcode), png));
  results.compare.push({ scene, composed: true, file: path.relative(worse, out).replaceAll("\\", "/") });
}

const isRole = (e, role) => String(e.accessibleRole ?? "").toLowerCase() === role;
const isButton = (text) => (e) => label(e) === text && isRole(e, "button");
const isItem = (text) => (e) => label(e) === text && isRole(e, "listitem");
const button = (mcp, text, timeoutMs) => mcp.waitFor(`button "${text}"`, isButton(text), timeoutMs);
const press = async (mcp, text) => mcp.activate(await button(mcp, text));
/** Same label used twice (header toggle + overlay control): the overlay is later in the element tree. */
async function pressLast(mcp, text) {
  await button(mcp, text);
  const all = (await mcp.elements()).filter(isButton(text));
  await mcp.activate(all[all.length - 1]);
}

async function chord(mcp, modifiers, key) {
  const codes = { ctrl: "\u0011", alt: "\u0012", shift: "\u0010" };
  for (const m of modifiers) await mcp.key(codes[m], "Press");
  await mcp.key(key);
  for (const m of [...modifiers].reverse()) await mcp.key(codes[m], "Release");
}

const messageInput = (mcp) => mcp.waitFor("message input", (e) => label(e) === "Message" && isRole(e, "textinput"));
async function typeDraft(mcp, text) {
  const input = await messageInput(mcp);
  await mcp.click(input);
  await mcp.setValue(input, text);
  await sleep(200);
  return input;
}
async function draftValue(mcp) {
  return String((await messageInput(mcp)).accessibleValue ?? "");
}
async function waitDraft(mcp, what, predicate, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs;
  let value = "";
  while (Date.now() < deadline) {
    value = await draftValue(mcp);
    if (predicate(value)) return value;
    await sleep(150);
  }
  throw new Error(`${what}: draft is ${JSON.stringify(value.slice(0, 120))}`);
}

/** Sends a message and waits for the fake agent's reply (label "<agent>: [<id>] reply to: <text>"). */
async function send(mcp, text, replyIncludes = text.split("\n")[0].slice(0, 40)) {
  await typeDraft(mcp, text);
  await mcp.key(KEY.enter);
  return mcp.waitFor(`reply to ${replyIncludes}`, byLabelPrefix("Claude Code: ", `reply to: ${replyIncludes}`), 30_000);
}

function clipboard() {
  const result = spawnSync("powershell", ["-NoProfile", "-Command", "Get-Clipboard -Raw"], { encoding: "utf8", windowsHide: true });
  return String(result.stdout ?? "").replace(/\r\n/g, "\n");
}
async function waitClipboard(what, predicate, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs;
  let text = "";
  while (Date.now() < deadline) {
    text = clipboard();
    if (predicate(text)) return text;
    await sleep(200);
  }
  throw new Error(`${what}: clipboard holds ${JSON.stringify(text.slice(0, 120))}`);
}
const waitToast = (mcp, prefix) => mcp.waitFor(`toast ${prefix}`, byLabelPrefix(`toast ${prefix}`), 5000);

async function waitFile(what, dir, predicate, timeoutMs = 10_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const hit = existsSync(dir) ? readdirSync(dir).map((n) => path.join(dir, n)).find(predicate) : undefined;
    if (hit) return hit;
    await sleep(150);
  }
  throw new Error(`${what}: no matching file in ${path.relative(home, dir)}`);
}

async function waitSettings(what, predicate, timeoutMs = 10_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      if (predicate(JSON.parse(readFileSync(settingsFile, "utf8")))) return;
    } catch {
      // mid-write
    }
    await sleep(150);
  }
  throw new Error(`${what}: settings.json did not change as expected`);
}

const paletteInput = (mcp) => mcp.waitFor("command center input", (e) => label(e) === "Search commands, sessions and files" && isRole(e, "textinput"), 5000);
async function openPalette(mcp) {
  await chord(mcp, ["ctrl"], "k");
  await mcp.waitFor("command center", byLabel("Command center"), 5000);
  return paletteInput(mcp);
}
async function paletteQuery(mcp, text) {
  const input = await paletteInput(mcp);
  await mcp.setValue(input, text);
  await sleep(250);
}
/** A command-center result row (its description starts with the row kind). */
const paletteRow = (mcp, kind, predicate) =>
  mcp.waitFor(`palette ${kind} row`, (e) => isRole(e, "listitem") && String(e.accessibleDescription ?? "").startsWith(kind) && predicate(label(e)), 5000);

// ---- scenarios ---------------------------------------------------------------------------------------------

const CODE_MESSAGE = "alpha needle\n```rust\nfn main() {}\n```";

async function firstRun(agentsFile) {
  const app = await step("start (light theme seeded)", [], () => launch({ automation: true, agentsFile }));
  const { mcp } = app;
  try {
    await step("send + transcript, rows do not overlap", ["TL-001", "SB-002"], async () => {
      await press(mcp, "New session");
      await send(mcp, CODE_MESSAGE, "alpha needle");
      await mcp.waitFor("code block", byLabel("code rust"), 10_000);
      await sleep(400);
      results.chatRowsChecked = await assertNoRowOverlap(mcp);
      await compareShot(mcp, "transcript");
    });

    await step("code block: copy code, wrap toggle", ["TL-017"], async () => {
      await press(mcp, "Copy code");
      await waitToast(mcp, "Copied");
      await waitClipboard("copy code", (t) => t.includes("fn main() {}"));
      await press(mcp, "Disable wrap");
      await button(mcp, "Enable wrap", 3000);
      await press(mcp, "Enable wrap");
      await button(mcp, "Disable wrap", 3000);
    });

    await step("message actions: copy response, copy message", ["MSG-001"], async () => {
      await press(mcp, "Copy response");
      await waitClipboard("copy response", (t) => t.includes("reply to: alpha needle"));
      await press(mcp, "Copy message");
      await waitClipboard("copy message", (t) => t.trim() === CODE_MESSAGE);
    });

    await step("find in session: count, next, close", ["CMD-008", "KEY-009"], async () => {
      await send(mcp, "beta needle");
      await chord(mcp, ["ctrl"], "f");
      const input = await mcp.waitFor("find input", (e) => label(e) === "Find" && isRole(e, "textinput"), 5000);
      await mcp.setValue(input, "needle");
      const status = await mcp.waitFor("find status", (e) => /^find-status \d+ of \d+$/.test(label(e)), 5000);
      const [, current, total] = label(status).match(/(\d+) of (\d+)/).map(Number);
      // 两条用户消息 + 两条回复都包含 needle。
      if (total < 4) throw new Error(`find counted ${total} matches, expected ≥ 4`);
      await compareShot(mcp, "find");
      await press(mcp, "Next match");
      await mcp.waitFor("find advanced", byLabel(`find-status ${(current % total) + 1} of ${total}`), 3000);
      await mcp.setValue(input, "no-such-text");
      await mcp.waitFor("find no results", byLabel("find-status No results"), 3000);
      await mcp.key(KEY.esc);
      await mcp.waitGone("find bar", byLabel("Find in session"), 3000);
      results.find = { total };
    });

    await step("command center: open, run Open Settings", ["CMD-001", "CMD-004", "KEY-007"], async () => {
      await openPalette(mcp);
      await compareShot(mcp, "command-center");
      await paletteQuery(mcp, "open settings");
      await mcp.activate(await paletteRow(mcp, "action", (l) => l === "Open Settings"));
      await mcp.waitFor("settings open", byLabel("Back to workspace"), 5000);
      await mcp.key(KEY.esc);
      await mcp.waitGone("settings", byLabel("Back to workspace"), 5000);
    });

    await step("command center scopes: Sessions, Files → attachment chip", ["CMD-001", "MEN-002"], async () => {
      await openPalette(mcp);
      await pressLast(mcp, "Sessions");
      // 假 agent 不生成标题，会话行都叫 “New session”。
      await paletteQuery(mcp, "new sess");
      await paletteRow(mcp, "session", (l) => l === "New session");
      await pressLast(mcp, "Files");
      await paletteQuery(mcp, "READ");
      await mcp.activate(await paletteRow(mcp, "file", (l) => l === "README.md"));
      await mcp.waitGone("command center", byLabel("Command center"), 3000);
      await mcp.waitFor("file chip", byLabel("file README.md"), 3000);
      await press(mcp, "Remove attachment README.md");
      await mcp.waitGone("file chip", byLabel("file README.md"), 3000);
    });

    await step("theme command (Toggle Light/Dark Theme) twice", ["CMD-006"], async () => {
      for (const expectDark of [true, false]) {
        await openPalette(mcp);
        // 每次打开都回到 All 范围（上一步停在 Files）。
        const all = (await mcp.elements()).filter(isButton("All")).pop();
        if (!all?.accessibleChecked) throw new Error("command center did not reopen in the All scope");
        await paletteQuery(mcp, "theme");
        await mcp.activate(await paletteRow(mcp, "action", (l) => l === "Toggle Light/Dark Theme"));
        await waitSettings(`theme → ${expectDark ? "dark" : "light"}`, (s) => String(s.appearance?.theme) === (expectDark ? "dark" : "light"));
      }
    });

    await step("@ mention: popup, pick file", ["MEN-001", "MEN-002", "CMP-002"], async () => {
      await typeDraft(mcp, "see @");
      await mcp.waitFor("mention list", (e) => label(e) === "Mentions" && isRole(e, "list"), 5000);
      await compareShot(mcp, "mention");
      await typeDraft(mcp, "see @READ");
      await mcp.waitFor("README.md suggestion", isItem("README.md"), 5000);
      await mcp.key(KEY.enter);
      await waitDraft(mcp, "mention inserted", (v) => v === "see @README.md ");
      await typeDraft(mcp, "");
    });

    await step("/ slash command: /export-md writes Markdown", ["MEN-009", "5A"], async () => {
      const exports = path.join(harnessHome, "exports");
      await typeDraft(mcp, "/export-m");
      await mcp.waitFor("slash list", (e) => label(e) === "Commands" && isRole(e, "list"), 5000);
      await mcp.key(KEY.enter);
      const file = await waitFile("markdown export", exports, (f) => f.endsWith(".md"));
      const text = readFileSync(file, "utf8");
      if (!text.includes("alpha needle") || !text.includes("fn main() {}")) throw new Error("markdown export misses the transcript");
      await waitDraft(mcp, "draft cleared", (v) => v === "");
      results.exportMarkdown = path.relative(home, file).replaceAll("\\", "/");
    });

    await step("header menu: export HTML", ["5A"], async () => {
      const exports = path.join(harnessHome, "exports");
      await press(mcp, "Session actions");
      await mcp.waitFor("session actions menu", (e) => label(e) === "Session actions" && isRole(e, "groupbox"), 3000);
      await press(mcp, "Export conversation as HTML");
      const file = await waitFile("html export", exports, (f) => f.endsWith(".html"));
      const html = readFileSync(file, "utf8");
      if (!html.startsWith("<!doctype html>") || !html.includes("alpha needle") || !html.includes('class="language-rust"')) {
        throw new Error("html export misses the transcript or the code block");
      }
      if (html.includes("<script")) throw new Error("html export contains a script tag");
      await waitToast(mcp, "Exported to");
      results.exportHtml = path.relative(home, file).replaceAll("\\", "/");
    });

    await step("long paste → chip, sent as text", ["ATT-004"], async () => {
      const big = Array.from({ length: LONG_PASTE / 64 }, (_, i) => `pasteline${String(i).padStart(4, "0")} ${"x".repeat(49)}`).join("\n");
      await typeDraft(mcp, big);
      await mcp.waitFor("paste chip", byLabelPrefix("paste "), 3000);
      await waitDraft(mcp, "draft emptied by the chip", (v) => v === "");
      await mcp.key(KEY.enter);
      await mcp.waitFor("reply to the paste", byLabelPrefix("Claude Code: ", "reply to: pasteline0000"), 30_000);
      await mcp.waitGone("paste chip", byLabelPrefix("paste "), 3000);
    });

    await step("prompt history ↑ / ↓", ["KEY-028"], async () => {
      const input = await typeDraft(mcp, "");
      await mcp.click(input);
      await mcp.key(KEY.up);
      await waitDraft(mcp, "history ↑ → last typed prompt", (v) => v === "beta needle");
      await mcp.key(KEY.up);
      await waitDraft(mcp, "history ↑↑", (v) => v === CODE_MESSAGE);
      await mcp.key(KEY.down);
      await mcp.key(KEY.down);
      await waitDraft(mcp, "history back to the empty draft", (v) => v === "");
    });

    await step("scroll-to-bottom button", ["TL-009"], async () => {
      await send(mcp, Array.from({ length: 80 }, (_, i) => `tall line ${i}`).join("\n"), "tall line 0");
      await sleep(500);
      for (let i = 0; i < 12; i++) {
        await mcp.scrollAt(640, 300, 240);
        await sleep(80);
      }
      await press(mcp, "Scroll to bottom");
      await mcp.waitGone("scroll-to-bottom button", isButton("Scroll to bottom"), 5000);
    });

    await step("sidebar: timeline header, relative time", ["SB-019", "SB-003"], async () => {
      await mcp.waitFor("Today header", (e) => label(e) === "Today", 5000);
      // 新会话 → 第一条变为非活动行，显示相对时间。
      await press(mcp, "New session");
      await send(mcp, "gamma needle");
      await mcp.waitFor("relative time on an inactive row", (e) => /^updated (now|\d+m)$/.test(label(e)), 5000);
    });

    await step("header: back / forward, previous / next task", ["HDR-001", "WIN-024", "KEY-015", "KEY-016"], async () => {
      // 会话标题都相同，用转录内容判断当前打开的是哪个会话。
      const shows = (prefix) => (e) => isRole(e, "text") && label(e).startsWith(prefix);
      await mcp.waitFor("gamma transcript", shows("gamma needle"), 5000);
      await press(mcp, "Navigate Back");
      // 列表是虚拟化的：只有可见行在元素树里，第一个会话停在底部（tall line 消息）。
      await mcp.waitFor("back → first session", shows("tall line 0"), 5000);
      await press(mcp, "Navigate Forward");
      await mcp.waitFor("forward → gamma transcript", shows("gamma needle"), 5000);
      const enabled = async (text) => Boolean(await mcp.find((e) => isButton(text)(e) && e.accessibleEnabled !== false));
      const stepBtn = (await enabled("Next Task")) ? "Next Task" : "Previous Task";
      await press(mcp, stepBtn);
      await mcp.waitGone(`${stepBtn} leaves gamma`, shows("gamma needle"), 5000);
      results.taskStep = stepBtn;
    });

    await step("session menu: copy session ID, archive", ["SB-021", "SB-014", "SB-009"], async () => {
      const rows = async () => (await mcp.elements()).filter((e) => isRole(e, "listitem") && label(e) === "New session").length;
      const before = await rows();
      const more = async () => mcp.waitFor("row action button", (e) => isRole(e, "button") && label(e).startsWith("More actions for "), 5000);
      await mcp.activate(await more());
      await mcp.waitFor("session menu", (e) => label(e) === "Session menu" && isRole(e, "groupbox"), 3000);
      await compareShot(mcp, "session-menu");
      await press(mcp, "Copy session ID");
      await waitToast(mcp, "Copied");
      const id = (await waitClipboard("session id", (t) => /^[\w-]{6,}$/.test(t.trim()))).trim();
      results.copiedSessionIdLength = id.length;
      await mcp.activate(await more());
      await press(mcp, "Archive");
      const deadline = Date.now() + 5000;
      while ((await rows()) >= before && Date.now() < deadline) await sleep(150);
      if ((await rows()) >= before) throw new Error(`archive: still ${before} session rows`);
    });
    // 9.2 权限对话框：列出 agent 提供的全部选项（ChoosePermission），数字键按序选择。
    const PERM_HINT = "Press a number to choose, Esc to deny once";
    // 选项来自 agent（hub 的 fake ACP agent 提供 "Allow once" / "Deny"），按卡片内顺序读取按钮。
    const permOptions = async () => {
      for (let attempt = 0; attempt < 20; attempt += 1) {
        const all = await mcp.elements();
        const y = (e) => e.absolutePosition?.y ?? 0;
        const head = all.find((e) => label(e) === "Permission required");
        const hint = all.find((e) => label(e) === PERM_HINT);
        if (head && hint) {
          const options = all
            .filter((e) => role(e).toLowerCase() === "button" && y(e) > y(head) && y(e) < y(hint))
            .sort((a, b) => y(a) - y(b))
            .map(label);
          if (options.length > 0) return options;
        }
        await sleep(150);
      }
      return [];
    };
    await step("permission: every option shown, key 1 approves", ["PRM-001", "PRM-002", "KEY-029"], async () => {
      await press(mcp, "New session");
      await typeDraft(mcp, "[[permission]] please write");
      await mcp.key(KEY.enter);
      await mcp.waitFor("permission options", byLabel(PERM_HINT), 30_000);
      const options = await permOptions();
      results.permissionOptions = options;
      if (options.length < 2 || !/^allow/i.test(options[0]) || !options.some((o) => /^(deny|reject)/i.test(o))) {
        throw new Error(`permission options: ${JSON.stringify(options)}`);
      }
      await mcp.key("1");
      await mcp.waitGone("permission card", byLabel(PERM_HINT), 10_000);
      await mcp.waitFor("reply after approval", byLabelPrefix("Claude Code: ", "reply to:"), 30_000);
    });
    await step("permission: number key picks the deny option", ["PRM-002", "KEY-029"], async () => {
      await press(mcp, "New session");
      await typeDraft(mcp, "[[permission]] deny this");
      await mcp.key(KEY.enter);
      await mcp.waitFor("permission options", byLabel(PERM_HINT), 30_000);
      const deny = (await permOptions()).findIndex((o) => /^(deny|reject)/i.test(o));
      if (deny < 0 || deny > 8) throw new Error("no deny option");
      await mcp.key(String(deny + 1));
      await mcp.waitGone("permission card", byLabel(PERM_HINT), 10_000);
      await mcp.waitFor("denied tool result", (e) => /denied/i.test(e.accessibleLabel ?? e.label ?? ""), 30_000);
    });

    await step("zoom shortcuts", ["WIN-030", "KEY-022"], async () => {
      await chord(mcp, ["ctrl"], "=");
      await waitSettings("zoom in", (s) => s.appearance?.zoomPercent === 110);
      await chord(mcp, ["ctrl"], "0");
      await waitSettings("zoom reset", (s) => s.appearance?.zoomPercent === 100);
    });
  } finally {
    results.firstRun = await close(app);
  }
}

async function narrow(agentsFile) {
  const app = await step("narrow: start 420x800", [], () => launch({ automation: true, agentsFile, size: "420x800" }));
  const { mcp } = app;
  try {
    await step("narrow layout: header collapses, composer usable", ["SP-011"], async () => {
      await messageInput(mcp);
      if (await mcp.find(isButton("Navigate Back"))) throw new Error("back button shown at 420 px");
      await button(mcp, "Find in Task", 3000);
      await compareShot(mcp, "narrow");
      await openPalette(mcp);
      await shot(mcp, "chat-narrow-command-center");
      await mcp.key(KEY.esc);
      await mcp.waitGone("command center", byLabel("Command center"), 3000);
    });
  } finally {
    results.narrowRun = await close(app);
  }
}


// ---- round 9.2B-2: turn detail, message actions, sessions, groups, schedules, composer extras -------------

const action = (mcp, element, name) => mcp.tool("invoke_accessibility_action", { elementHandle: element.handle, action: name });
const dragTo = (mcp, element, x, y) => mcp.tool("drag_element", { elementHandle: element.handle, target: { x, y } });
/** The text field of the open dialog (every text input except the composer and the palette). */
const dialogField = (mcp, title) => mcp.waitFor(`dialog field ${title}`, (e) => isRole(e, "textinput") && label(e) === title, 5000);
const sessionRows = async (mcp) => (await mcp.elements()).filter((e) => isRole(e, "listitem") && !String(e.accessibleDescription ?? "").length).map(label);
async function sessionMenu(mcp, title) {
  await mcp.activate(await button(mcp, `More actions for ${title}`, 5000));
  await mcp.waitFor("session menu", (e) => label(e) === "Session menu" && isRole(e, "groupbox"), 3000);
}
async function waitReply(mcp, includes, timeoutMs = 30_000) {
  return mcp.waitFor(`reply to ${includes}`, byLabelPrefix("Claude Code: ", `reply to: ${includes}`), timeoutMs);
}
/** Presses a button until `opened` appears: a transcript row can be rebuilt (stale handle) right after a turn ends. */
async function pressUntil(mcp, text, what, opened, attempts = 4, last = false) {
  for (let i = 0; i < attempts; i += 1) {
    await (last ? pressLast(mcp, text) : press(mcp, text));
    const deadline = Date.now() + 1500;
    while (Date.now() < deadline) {
      if (await mcp.find(opened)) return;
      await sleep(100);
    }
  }
  throw new Error(`${text} did not open ${what}`);
}
/** Finds and activates an element, again if a model refresh destroyed it between the lookup and the action. */
async function activateFresh(mcp, what, predicate, attempts = 4) {
  for (let i = 0; ; i += 1) {
    try {
      return await mcp.activate(await mcp.waitFor(what, predicate, 5000));
    } catch (error) {
      if (i + 1 >= attempts || !/destroyed/.test(String(error?.message ?? error))) throw error;
      await sleep(200);
    }
  }
}
const OPTIONS_BUTTON = "Mode & effort";
const optionsButton = (e) => isButton(OPTIONS_BUTTON)(e) && e.accessibleEnabled === true;
const pickerButton = (which) => (e) => isRole(e, "button") && label(e).startsWith(`${which}: `) && e.accessibleEnabled === true;
/** Pickers shown as their own footer button (the rest are in the options popup or absent). */
const footerPickers = async (mcp) => {
  const all = await mcp.elements();
  return ["Mode", "Model", "Effort"].filter((w) => all.some((e) => isRole(e, "button") && label(e).startsWith(`${w}: `)));
};
/** Picks through the footer button when the picker has one, else through the options popup (collapsed footer). */
async function pickerPick(mcp, which, option) {
  const optionRow = (e) => label(e) === `${which.toLowerCase()}-option ${option}`;
  if (await mcp.find(pickerButton(which))) {
    await activateFresh(mcp, `${which} picker`, pickerButton(which));
    await activateFresh(mcp, `${which} option ${option}`, optionRow);
    await mcp.waitFor(`${which} shows ${option}`, (e) => isRole(e, "button") && label(e) === `${which}: ${option}`, 5000);
    return;
  }
  await activateFresh(mcp, "options button", optionsButton);
  await activateFresh(mcp, `${which} option ${option}`, optionRow);
  await mcp.waitGone("options popup", byLabel(`section ${which}`), 3000);
  await activateFresh(mcp, "options button", optionsButton);
  const row = await mcp.waitFor(`${which} ${option} in the popup`, optionRow, 3000);
  if (!row.accessibleChecked) throw new Error(`${which} ${option} is not checked in the options popup`);
  await activateFresh(mcp, "close options", isButton(OPTIONS_BUTTON));
  await mcp.waitGone("options popup", byLabel(`section ${which}`), 3000);
}
/** Footer row (same centre line as Send / Stop, inside the composer): no item overlaps another or passes Send. */
async function assertFooterFits(mcp) {
  const all = await mcp.elements();
  const end = all.find(isButton("Send")) ?? all.find(isButton("Stop"));
  const input = all.find((e) => label(e) === "Message" && isRole(e, "textinput"));
  if (!end || !input) throw new Error("composer footer not found");
  const x = (e) => Number(e.absolutePosition?.x ?? 0);
  const w = (e) => Number(e.size?.width ?? 0);
  const cy = (e) => Number(e.absolutePosition?.y ?? 0) + Number(e.size?.height ?? 0) / 2;
  const right = x(end) + w(end);
  // 16 px: the agent button may start a little left of the text input's inner edge (composer padding).
  const items = all
    .filter((e) => (isRole(e, "button") || label(e).startsWith("context ")) && Math.abs(cy(e) - cy(end)) <= 12)
    .filter((e) => x(e) >= x(input) - 16 && x(e) < right)
    .sort((a, b) => x(a) - x(b));
  for (let i = 0; i < items.length; i += 1) {
    const r = x(items[i]) + w(items[i]);
    if (r > right + 0.5) throw new Error(`footer item "${label(items[i])}" ends past Send/Stop`);
    if (i + 1 < items.length && r > x(items[i + 1]) + 0.5) {
      throw new Error(`footer items overlap: "${label(items[i])}" and "${label(items[i + 1])}"`);
    }
  }
  return items.map(label);
}
async function sendRaw(mcp, text) {
  await typeDraft(mcp, text);
  await mcp.key(KEY.enter);
}

async function round2(agentsFile) {
  const openerLog = path.join(home, "opener.log");
  const app = await step("round 2: start", [], () => launch({ automation: true, agentsFile, extraEnv: { HARNESS_OPENER_LOG: openerLog } }));
  const { mcp } = app;
  try {
    await step("thinking block, tool detail, plan, context meter", ["TL-003", "TL-007", "CTX-001"], async () => {
      await press(mcp, "New session");
      await sendRaw(mcp, "[[think pondering deeply]] [[tools]] [[plan A|B|C]] [[usage 5000 200000]] [[commands]] detail run");
      await waitReply(mcp, "");
      const thought = await mcp.waitFor("thought row", isButton("Thought"), 10_000);
      await mcp.activate(thought);
      await mcp.waitFor("thought expanded", byLabel("thought pondering deeply"), 3000);
      const tool = await mcp.waitFor("tool card", (e) => isRole(e, "button") && label(e).startsWith("edit src/lib.rs · "), 10_000);
      await mcp.activate(tool);
      await mcp.waitFor("tool detail", byLabel("tool detail edit src/lib.rs"), 3000);
      await mcp.waitFor("diff line", (e) => label(e) === "+ new line", 3000);
      await mcp.waitFor("plan", byLabel("Plan"), 5000);
      await mcp.waitFor("plan entry done", byLabel("done A"), 3000);
      await mcp.waitFor("plan entry doing", byLabel("doing B"), 3000);
      const meter = await mcp.waitFor("context meter", byLabelPrefix("context "), 5000);
      results.contextMeter = label(meter);
      await compareShot(mcp, "tool-detail");
    });

    await step("open in editor from a tool card", ["CHG-001"], async () => {
      const open = await mcp.waitFor("open link", (e) => isRole(e, "button") && label(e).startsWith("open src/lib.rs"), 3000);
      await mcp.activate(open);
      const deadline = Date.now() + 5000;
      while (Date.now() < deadline && !(existsSync(openerLog) && readFileSync(openerLog, "utf8").includes("lib.rs"))) await sleep(150);
      const logged = existsSync(openerLog) ? readFileSync(openerLog, "utf8") : "";
      if (!logged.includes(path.join("workspace", "src", "lib.rs")) && !logged.includes("workspace/src/lib.rs")) {
        throw new Error(`opener log: ${JSON.stringify(logged.slice(-200))}`);
      }
    });

    await step("slash: agent commands listed", ["MEN-009"], async () => {
      await typeDraft(mcp, "/rev");
      await mcp.waitFor("slash list", (e) => label(e) === "Commands" && isRole(e, "list"), 5000);
      await mcp.waitFor("agent command row", (e) => isRole(e, "listitem") && label(e).includes("review"), 5000);
      await typeDraft(mcp, "");
    });

    await step("table copy as CSV / Markdown, list block", ["TL-019"], async () => {
      await sendRaw(mcp, "tbl\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n- one\n- two");
      await waitReply(mcp, "tbl");
      await mcp.waitFor("table block", byLabel("table 2x2"), 10_000);
      await mcp.waitFor("list block", byLabel("list 2"), 3000);
      await press(mcp, "Copy as CSV");
      await waitClipboard("csv", (t) => t.trim() === "a,b\n1,2");
      await press(mcp, "Copy as Markdown");
      await waitClipboard("markdown table", (t) => t.includes("| a | b |") && t.includes("| 1 | 2 |"));
      await compareShot(mcp, "table");
    });

    await step("@ session mention → session chip", ["MEN-003"], async () => {
      await press(mcp, "New session");
      await typeDraft(mcp, "@New");
      await mcp.waitFor("session suggestion", isItem("New session"), 5000);
      await mcp.key(KEY.enter);
      await mcp.waitFor("session chip", byLabelPrefix("session "), 3000);
      await press(mcp, (await mcp.waitFor("chip remove", (e) => isRole(e, "button") && label(e).startsWith("Remove attachment "), 3000)).accessibleLabel);
      await mcp.waitGone("session chip", byLabelPrefix("session "), 3000);
      await typeDraft(mcp, "");
    });

    await step("edit and resend, retry, fork", ["MSG-002", "MSG-003", "MSG-005"], async () => {
      await sendRaw(mcp, "first draft");
      await waitReply(mcp, "first draft");
      const editField = (e) => isRole(e, "textinput") && label(e) === "Edit message";
      await pressUntil(mcp, "Edit message", "the edit dialog", editField);
      const field = await dialogField(mcp, "Edit message");
      await mcp.setValue(field, "second draft");
      await pressLast(mcp, "Send");
      await waitReply(mcp, "second draft");
      await mcp.waitGone("old reply", byLabelPrefix("Claude Code: ", "reply to: first draft"), 5000);
      const replies = async () => (await mcp.elements()).filter(byLabelPrefix("Claude Code: ", "reply to: second draft")).length;
      await press(mcp, "Retry");
      await sleep(1500);
      if ((await replies()) !== 1) throw new Error(`retry left ${await replies()} replies`);
      const before = (await sessionRows(mcp)).length;
      await pressLast(mcp, "Fork from here");
      const deadline = Date.now() + 5000;
      while (Date.now() < deadline && (await sessionRows(mcp)).length <= before) await sleep(150);
      if ((await sessionRows(mcp)).length <= before) throw new Error("fork did not add a session");
      await waitReply(mcp, "second draft", 5000);
    });

    await step("rewind restores the prompt to the composer", ["MSG-004", "MSG-007"], async () => {
      // 修复：会话里还有更早的用户消息（如表格消息）时，第一个“Rewind to here”属于它；回退目标是最后一条（second draft）。
      await pressUntil(mcp, "Rewind to here", "the rewind dialog", isButton("Rewind"), 4, true);
      await pressLast(mcp, "Rewind");
      await waitDraft(mcp, "rewound prompt", (v) => v === "second draft");
      await mcp.waitGone("rewound reply", byLabelPrefix("Claude Code: ", "reply to: second draft"), 5000);
      await typeDraft(mcp, "");
    });

    await step("changed files: undo / reapply", ["CHG-002", "CHG-003"], async () => {
      const target = path.join(workspace, "notes.txt");
      await press(mcp, "New session");
      await sendRaw(mcp, "[[write notes.txt hello notes]] write notes");
      await waitReply(mcp, "");
      await mcp.waitFor("changes card", byLabel("1 file changed"), 15_000);
      if (!existsSync(target)) throw new Error("fake agent did not write notes.txt");
      await press(mcp, "Undo changes");
      await waitToast(mcp, "");
      const deadline = Date.now() + 5000;
      while (Date.now() < deadline && existsSync(target)) await sleep(150);
      if (existsSync(target)) throw new Error("undo kept notes.txt");
      await press(mcp, "Reapply changes");
      const again = Date.now() + 5000;
      while (Date.now() < again && !existsSync(target)) await sleep(150);
      if (!existsSync(target) || !readFileSync(target, "utf8").includes("hello notes")) throw new Error("reapply did not restore notes.txt");
    });

    await step("deny with feedback", ["PRM-004"], async () => {
      await press(mcp, "New session");
      await sendRaw(mcp, "[[permission]] guarded write");
      await mcp.waitFor("permission card", byLabel("Press a number to choose, Esc to deny once"), 30_000);
      await press(mcp, "Deny with feedback…");
      await mcp.setValue(await dialogField(mcp, "Deny with feedback"), "try another file");
      await pressLast(mcp, "Deny and send");
      await mcp.waitGone("permission card", byLabel("Press a number to choose, Esc to deny once"), 10_000);
      await waitReply(mcp, "try another file");
    });

    await step("session menu: rename, pin, mark unread, delete", ["SB-006", "SB-007", "SB-008", "SB-010"], async () => {
      const first = (await sessionRows(mcp)).find((t) => t === "New session");
      if (!first) throw new Error("no session row");
      await sessionMenu(mcp, "New session");
      await press(mcp, "Rename");
      await mcp.setValue(await dialogField(mcp, "Rename session"), "Renamed alpha");
      await pressLast(mcp, "Rename");
      await mcp.waitFor("renamed row", isItem("Renamed alpha"), 5000);
      await sessionMenu(mcp, "Renamed alpha");
      await press(mcp, "Pin");
      await mcp.waitFor("Pinned section", byLabel("Pinned"), 5000);
      await sessionMenu(mcp, "Renamed alpha");
      await press(mcp, "Mark as unread");
      // 活动会话被标记未读后，切到别的会话才显示未读点。
      await press(mcp, "New session");
      await mcp.waitFor("unread dot", byLabel("unread"), 5000);
      await sessionMenu(mcp, "Renamed alpha");
      await press(mcp, "Unpin");
      await mcp.waitGone("Pinned section", byLabel("Pinned"), 5000);
      await sessionMenu(mcp, "Renamed alpha");
      await press(mcp, "Delete");
      await pressLast(mcp, "Delete");
      await mcp.waitGone("deleted row", isItem("Renamed alpha"), 5000);
    });

    await step("groups: create, color, move, collapse, delete", ["GRP-001", "GRP-002", "GRP-004", "GRP-005"], async () => {
      await press(mcp, "New group");
      await mcp.setValue(await dialogField(mcp, "New group"), "Team");
      await pressLast(mcp, "Create");
      await mcp.waitFor("group header", byLabel("group Team"), 5000);
      await activateFresh(mcp, "group actions", isButton("Group actions for Team"));
      await mcp.waitFor("group menu", byLabel("Group menu"), 3000);
      await activateFresh(mcp, "color blue", (e) => label(e) === "color blue");
      await sessionMenu(mcp, (await sessionRows(mcp)).find((t) => t !== "") ?? "New session");
      await press(mcp, "Move to Team");
      await sleep(500);
      await compareShot(mcp, "groups");
      await activateFresh(mcp, "group header", byLabel("group Team"));
      await mcp.waitFor("collapsed group", (e) => label(e) === "group Team" && e.accessibleDescription === "collapsed", 3000);
      await activateFresh(mcp, "group header", byLabel("group Team"));
      await mcp.waitFor("expanded group", (e) => label(e) === "group Team" && e.accessibleDescription === "expanded", 3000);
      await activateFresh(mcp, "group actions", isButton("Group actions for Team"));
      await activateFresh(mcp, "delete group", isButton("Delete group"));
      await pressLast(mcp, "Delete");
      await mcp.waitGone("group header", byLabel("group Team"), 5000);
    });

    await step("scheduled prompts: create, run now, delete", ["8A"], async () => {
      await press(mcp, "Scheduled prompts");
      await mcp.waitFor("schedules dialog", byLabel("Run scheduled prompts"), 5000);
      await press(mcp, "New schedule");
      await mcp.setValue(await dialogField(mcp, "New schedule"), "scheduled ping");
      await mcp.setValue(await dialogField(mcp, "Every N minutes"), "60");
      await pressLast(mcp, "Create");
      await mcp.waitFor("schedule row", byLabel("schedule scheduled ping"), 5000);
      await compareShot(mcp, "schedules");
      await press(mcp, "Run scheduled ping now");
      await waitReply(mcp, "scheduled ping");
      // 修复：回复流式到达时元素树会重建，按钮句柄可能在激活前失效（gate 偶发 "element that was destroyed"），改用重新查找后重试。
      await activateFresh(mcp, "scheduled prompts", isButton("Scheduled prompts"));
      await activateFresh(mcp, "delete schedule", isButton("Delete scheduled ping"));
      await mcp.waitGone("schedule row", byLabel("schedule scheduled ping"), 5000);
      await mcp.key(KEY.esc);
    });

    await step("command center: sections and recent actions", ["CMD-002"], async () => {
      await openPalette(mcp);
      await mcp.waitFor("section Suggested", byLabel("section Suggested"), 3000);
      // 结果列表是虚拟化的：用 ↓ 逐行移动（跳过分组标题），后面的分组滚动进来后才在元素树中。
      for (const s of ["Panels", "Configure"]) {
        for (let i = 0; i < 40 && !(await mcp.find(byLabel(`section ${s}`))); i += 1) await mcp.key(KEY.down);
        await mcp.waitFor(`section ${s}`, byLabel(`section ${s}`), 1000);
      }
      await paletteQuery(mcp, "copy session");
      await mcp.activate(await paletteRow(mcp, "action", (l) => l === "Copy session ID"));
      await mcp.waitGone("command center", byLabel("Command center"), 3000);
      await openPalette(mcp);
      await mcp.waitFor("section Recent", byLabel("section Recent"), 3000);
      await compareShot(mcp, "command-center-sections");
      await mcp.key(KEY.esc);
    });

    await step("whiteboard: draw and attach, annotate the image", ["13A"], async () => {
      await press(mcp, "Whiteboard");
      const board = await mcp.waitFor("board", byLabel("drawing 0"), 5000);
      const p = board.absolutePosition ?? { x: 300, y: 200 };
      await dragTo(mcp, board, p.x + 60, p.y + 60);
      await mcp.waitFor("one stroke", byLabel("drawing 1"), 3000);
      await compareShot(mcp, "whiteboard");
      await press(mcp, "Undo stroke");
      await mcp.waitFor("stroke undone", byLabel("drawing 0"), 3000);
      await dragTo(mcp, await mcp.waitFor("board", byLabel("drawing 0"), 3000), p.x + 80, p.y + 40);
      await mcp.waitFor("one stroke", byLabel("drawing 1"), 3000);
      await press(mcp, "Attach");
      const chip = await mcp.waitFor("whiteboard chip", byLabelPrefix("image whiteboard"), 5000);
      const name = label(chip).slice("image ".length);
      await press(mcp, `Annotate ${name}`);
      await mcp.waitFor("annotation dialog", (e) => label(e) === `Annotate ${name}`, 5000);
      const again = await mcp.waitFor("board", byLabel("drawing 0"), 5000);
      await dragTo(mcp, again, p.x + 20, p.y + 90);
      await mcp.waitFor("annotation stroke", byLabel("drawing 1"), 3000);
      await press(mcp, "Attach");
      await mcp.waitFor("annotated chip", byLabelPrefix("image annotated"), 5000);
      await mcp.waitGone("original chip", byLabel(label(chip)), 3000);
      const files = readdirSync(path.join(harnessHome, "attachments")).filter((n) => n.endsWith(".png"));
      if (files.length < 2) throw new Error(`attachments: ${files.join(", ")}`);
      await press(mcp, (await mcp.waitFor("chip remove", (e) => isRole(e, "button") && label(e).startsWith("Remove attachment "), 3000)).accessibleLabel);
    });

    await step("sidebar resize", [], async () => {
      const handle = await mcp.waitFor("resize handle", (e) => label(e) === "Resize sidebar", 3000);
      const before = Number(handle.accessibleValue);
      await action(mcp, handle, "Increment");
      const after = Number((await mcp.waitFor("resize handle", (e) => label(e) === "Resize sidebar", 3000)).accessibleValue);
      if (!(after > before)) throw new Error(`sidebar width ${before} → ${after}`);
      await action(mcp, handle, "Decrement");
    });
  } finally {
    results.round2Run = await close(app);
  }
}

// ---- round 9.2B-3: queue ops, pickers, skills / subagents, typed failures, projects tree --------------------

const queuedTexts = async (mcp) =>
  (await mcp.elements()).filter((e) => label(e).startsWith("Queued: ") && isRole(e, "text")).map((e) => label(e).slice("Queued: ".length));
async function waitQueue(mcp, what, expected, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs;
  let last = [];
  while (Date.now() < deadline) {
    last = await queuedTexts(mcp);
    if (JSON.stringify(last) === JSON.stringify(expected)) return;
    await sleep(120);
  }
  throw new Error(`${what}: queue ${JSON.stringify(last)} != ${JSON.stringify(expected)}`);
}
const promptCount = (needle) =>
  existsSync(path.join(fakeState, "log.jsonl"))
    ? readFileSync(path.join(fakeState, "log.jsonl"), "utf8").split("\n").filter((l) => l.includes('"prompt"') && l.includes(needle)).length
    : 0;
async function waitPrompts(what, needle, atLeast, timeoutMs = 15_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline && promptCount(needle) < atLeast) await sleep(150);
  if (promptCount(needle) < atLeast) throw new Error(`${what}: ${promptCount(needle)} prompts with ${needle} < ${atLeast}`);
}
const failureBanner = (mcp, kind, timeoutMs = 20_000) => mcp.waitFor(`failure ${kind}`, byLabelPrefix(`failure ${kind} `), timeoutMs);

async function round3(agentsFile) {
  const app = await step("round 3: start", [], () => launch({ automation: true, agentsFile }));
  const { mcp } = app;
  try {
    await step("mode / model / effort pickers", ["PCK-001", "PCK-004", "PCK-006", "PCK-007"], async () => {
      await press(mcp, "New session");
      await sendRaw(mcp, "picker warmup");
      await waitReply(mcp, "picker warmup");
      // 回复文字先于 TurnFinished 到达；回合结束前选择器处于 locked（禁用）状态，点击会被忽略。
      await mcp.waitGone("turn end", isButton("Stop"), 10_000);
      // Mode 可能已折叠进“选项”按钮（取决于页脚剩余宽度），两者之一可用即视为已解锁。
      await mcp.waitFor("pickers unlocked", (e) => pickerButton("Mode")(e) || optionsButton(e), 10_000);
      await assertFooterFits(mcp);
      await pickerPick(mcp, "Mode", "plan");
      await pickerPick(mcp, "Model", "fake-large");
      await pickerPick(mcp, "Effort", "high");
      await compareShot(mcp, "pickers");
      await sendRaw(mcp, "[[options]] picked");
      await mcp.waitFor("options applied", byLabelPrefix("Claude Code: ", "options=mode:plan,model:fake-large,effort:high"), 30_000);
    });

    await step("collapsed pickers at 1100 px with the context meter", ["PCK-006", "PCK-007"], async () => {
      await sendRaw(mcp, "[[usage 5000 200000]] meter on");
      await waitReply(mcp, "[[usage 5000 200000]] meter on");
      await mcp.waitGone("turn end", isButton("Stop"), 10_000);
      await mcp.waitFor("context meter", byLabelPrefix("context "), 5000);
      // 负责人决定：页脚放不下全部选择器时必须折叠进“选项”按钮，而不是裁掉文字。
      await mcp.waitFor("options button", (e) => isButton("Mode & effort")(e) && e.accessibleEnabled === true, 10_000);
      await assertFooterFits(mcp);
      const shown = await footerPickers(mcp);
      const inPopup = [];
      await activateFresh(mcp, "options button", isButton("Mode & effort"));
      for (const which of ["Mode", "Model", "Effort"]) {
        if (shown.includes(which)) continue;
        await mcp.waitFor(`${which} section in the popup`, byLabel(`section ${which}`), 3000);
        inPopup.push(which);
      }
      if (inPopup.length < 2) throw new Error(`collapsed popup holds only ${inPopup.join(", ")} (footer shows ${shown.join(", ")})`);
      await activateFresh(mcp, "effort low", (e) => label(e) === "effort-option low");
      await mcp.waitGone("options popup", byLabel("section Effort"), 3000);
      await activateFresh(mcp, "options button", isButton("Mode & effort"));
      const checked = await mcp.waitFor("effort low checked", (e) => label(e) === "effort-option low", 3000);
      if (!checked.accessibleChecked) throw new Error("effort low is not checked in the popup after picking it");
      await compareShot(mcp, "pickers-popup");
      await activateFresh(mcp, "close options", isButton("Mode & effort"));
      await mcp.waitGone("options popup", byLabel("section Effort"), 3000);
      results.pickerLayout = { footer: shown, popup: inPopup };
      await sendRaw(mcp, "[[options]] collapsed");
      await mcp.waitFor("effort applied", byLabelPrefix("Claude Code: ", "effort:low"), 30_000);
    });

    await step("@ skills and subagents", ["MEN-004", "MEN-005"], async () => {
      await typeDraft(mcp, "@gate-sk");
      await mcp.waitFor("skill row", isItem("gate-skill"), 10_000);
      await mcp.key(KEY.enter);
      await mcp.waitFor("skill chip", byLabel("skill gate-skill"), 3000);
      await typeDraft(mcp, "@gate-rev");
      await mcp.waitFor("subagent row", isItem("gate-reviewer"), 5000);
      await mcp.key(KEY.enter);
      await waitDraft(mcp, "subagent text", (v) => v === "@agent-gate-reviewer ");
      await mcp.key(KEY.enter);
      await waitReply(mcp, "@agent-gate-reviewer");
      // 技能 chip 作为额外的上下文块随消息发送（fake agent 只记录文本块，类型见 blockTypes）。
      const sent = readFileSync(path.join(fakeState, "log.jsonl"), "utf8").split("\n").filter((l) => l.includes('"prompt"') && l.includes("@agent-gate-reviewer"));
      const types = sent.length ? JSON.parse(sent[sent.length - 1]).blockTypes ?? [] : [];
      if (types.length < 2) throw new Error(`skill context not sent: blockTypes ${JSON.stringify(types)}`);
    });

    await step("queue: reorder, steer, edit, composer steer", ["Q-002", "Q-003"], async () => {
      await sendRaw(mcp, "[[slow 400]] long run");
      await mcp.waitFor("stop button", isButton("Stop"), 10_000);
      for (const text of ["q one", "q two", "q three"]) await sendRaw(mcp, text);
      await waitQueue(mcp, "queued", ["q one", "q two", "q three"]);
      await mcp.waitFor("queue title", byLabel("Queued messages (3)"), 3000);
      await compareShot(mcp, "queue");
      await action(mcp, await mcp.waitFor("grip", byLabel("Drag to reorder: q one"), 3000), "Increment");
      await waitQueue(mcp, "reordered", ["q two", "q one", "q three"]);
      const grip = await mcp.waitFor("grip", byLabel("Drag to reorder: q three"), 3000);
      const p = grip.absolutePosition ?? { x: 0, y: 0 };
      const h = Number(grip.size?.height ?? 30);
      const w = Number(grip.size?.width ?? 28);
      await dragTo(mcp, grip, p.x + w / 2, p.y + h / 2 - 2 * 34);
      await waitQueue(mcp, "dragged", ["q three", "q two", "q one"]);
      await press(mcp, "Steer queued message: q one");
      await waitQueue(mcp, "steered row", ["q one", "q three", "q two"]);
      await press(mcp, "Edit queued message: q two");
      await waitDraft(mcp, "edit draft", (v) => v === "q two");
      await mcp.waitFor("editing tag", byLabel("Editing queued message"), 3000);
      await typeDraft(mcp, "q two edited");
      await mcp.key(KEY.enter);
      await waitQueue(mcp, "edited", ["q one", "q three", "q two edited"]);
      await mcp.waitGone("editing tag", byLabel("Editing queued message"), 3000);
      await typeDraft(mcp, "steer me");
      await press(mcp, "Steer: run right after the current turn");
      await waitQueue(mcp, "composer steer", ["steer me", "q one", "q three", "q two edited"]);
      await press(mcp, "Stop");
      // 运行顺序即 fake agent 收到 prompt 的顺序。
      await waitReply(mcp, "q two edited", 60_000);
      const lines = readFileSync(path.join(fakeState, "log.jsonl"), "utf8").split("\n").filter((l) => l.includes('"prompt"'));
      const order = ["steer me", "q one", "q three", "q two edited"].map((t) => lines.findIndex((l) => l.includes(`"${t}"`) || l.includes(`${t}\\`)));
      if (order.some((i) => i < 0) || order.some((v, i) => i > 0 && v < order[i - 1])) throw new Error(`prompt order ${JSON.stringify(order)}`);
    });

    await step("typed failure: details, copy, retry", ["ERR-001", "ERR-002", "ERR-003"], async () => {
      await sendRaw(mcp, "[[fail auth]] auth boom");
      await failureBanner(mcp, "auth");
      await mcp.waitFor("login hint", byLabel("Sign in to the agent's CLI in a terminal, then retry."), 3000);
      await compareShot(mcp, "failure");
      await press(mcp, "Copy error");
      await waitClipboard("copied failure", (t) => t.startsWith("Sign-in required"));
      const before = promptCount("auth boom");
      await press(mcp, "Retry");
      await waitPrompts("retry re-ran the turn", "auth boom", before + 1);
      await failureBanner(mcp, "auth");
      await press(mcp, "Dismiss error");
      await mcp.waitGone("failure banner", byLabelPrefix("failure "), 3000);
      await sendRaw(mcp, "[[fail other]] other boom");
      await failureBanner(mcp, "other");
      await press(mcp, "Details");
      await mcp.waitFor("details", (e) => label(e) === "Error details" && isRole(e, "textinput"), 3000);
      await press(mcp, "Dismiss error");
    });

    await step("typed failure: process exit, transient wait", ["ERR-006"], async () => {
      await sendRaw(mcp, "[[fail exit]] exit boom");
      await failureBanner(mcp, "processExited");
      await sendRaw(mcp, "[[fail transient 2]] slow boom");
      await failureBanner(mcp, "transient");
      await mcp.waitFor("retry countdown", (e) => isRole(e, "button") && label(e) === "Retry", 3000);
      await press(mcp, "Dismiss error");
    });

    await step("typed failure: init failed, retry", ["ERR-004"], async () => {
      const flag = path.join(fakeState, "claude-code.initfail");
      writeFileSync(flag, "1");
      await press(mcp, "New session");
      await sendRaw(mcp, "init boom");
      await failureBanner(mcp, "initFailed");
      spawnSync(process.execPath, ["-e", `require("fs").rmSync(${JSON.stringify(flag)})`]);
      await press(mcp, "Retry");
      await waitReply(mcp, "init boom");
      await mcp.waitGone("failure banner", byLabelPrefix("failure "), 3000);
      // 重试后快照之前的事件不能被重放：没有一直“Streaming…”的旧回复行，用户消息也只有一条（A 的 0.5.2 修复）。
      await mcp.waitGone("stale streaming row", byLabel("Streaming…"), 5000);
      const bubbles = (await mcp.elements()).filter((e) => label(e) === "init boom").length;
      if (bubbles !== 1) throw new Error(`${bubbles} "init boom" user bubbles after retry`);
    });

    await step("git: branch switcher, blocked switch, commit & push, graph", ["GIT-007", "GIT-008", "GIT-009", "GIT-010", "GIT-011"], async () => {
      const gitOut = (...args) => spawnSync("git", args, { cwd: workspace, encoding: "utf8", windowsHide: true }).stdout.trim();
      const base = gitOut("rev-parse", "--abbrev-ref", "HEAD");
      await activateFresh(mcp, "Git tab", (e) => label(e) === "Git" && isRole(e, "tab"));
      // 未提交的 README 修改与 feature 分支冲突：切换必须被拒绝且什么都不改变。
      writeFileSync(path.join(workspace, "README.md"), "# local edit\n");
      await press(mcp, "Switch branch");
      await mcp.waitFor("feature row", byLabel("branch feature"), 10_000);
      await mcp.waitFor("long row", byLabel("branch long"), 3000);
      await compareShot(mcp, "git-branches");
      await press(mcp, "branch feature");
      await mcp.waitFor("blocked notice", byLabel("blocked feature"), 10_000);
      await mcp.waitFor("blocked files", byLabelPrefix("blocked files ", "README.md"), 3000);
      if (gitOut("rev-parse", "--abbrev-ref", "HEAD") !== base) throw new Error("blocked switch changed HEAD");
      await mcp.setValue(await mcp.waitFor("branch filter", byLabel("Find or create a branch"), 3000), "gate-new");
      await press(mcp, "Create branch gate-new");
      await mcp.waitFor("switched", byLabelPrefix("git switched ", "gate-new"), 10_000);
      if (gitOut("rev-parse", "--abbrev-ref", "HEAD") !== "gate-new") throw new Error(`HEAD is ${gitOut("rev-parse", "--abbrev-ref", "HEAD")}`);
      await mcp.waitGone("branch dialog", byLabel("Find or create a branch"), 3000);

      await mcp.setValue(await mcp.waitFor("panel message", byLabel("Commit message"), 3000), "gate commit all");
      await press(mcp, "More commit options");
      await mcp.waitFor("include unstaged", (e) => label(e) === "Include unstaged changes" && isRole(e, "checkbox"), 3000);
      await press(mcp, "Commit & push");
      await mcp.waitFor("pushed", byLabelPrefix("git pushed-upstream ", "gate-new"), 20_000);
      await compareShot(mcp, "git-push");
      if (gitOut("log", "-1", "--format=%s") !== "gate commit all") throw new Error("commit subject");
      if (gitOut("status", "--porcelain") !== "") throw new Error(`include unstaged left changes: ${gitOut("status", "--porcelain")}`);
      const remote = spawnSync("git", ["--git-dir", path.join(home, "origin.git"), "rev-parse", "gate-new"], { encoding: "utf8", windowsHide: true }).stdout.trim();
      if (remote !== gitOut("rev-parse", "HEAD")) throw new Error(`origin/gate-new is ${remote}`);
      await mcp.waitFor("push target", byLabelPrefix("push target ", "origin/gate-new"), 3000);
      await pressLast(mcp, "Close");
      await mcp.waitGone("push dialog", byLabel("Push now"), 3000);

      await press(mcp, "Commit graph");
      const head = gitOut("rev-parse", "HEAD");
      const top = await mcp.waitFor("head commit row", (e) => label(e) === `commit ${head.slice(0, 7)} gate commit all` && isRole(e, "listitem"), 10_000);
      await mcp.waitFor("load more", isButton("Load more"), 5000);
      await mcp.activate(top);
      await mcp.waitFor("commit hash", byLabel(`commit hash ${head}`), 3000);
      await compareShot(mcp, "git-graph");
      await press(mcp, "Copy commit hash");
      await waitClipboard("copied hash", (t) => t.trim() === head);
      await press(mcp, "Load more");
      await mcp.waitGone("load more", isButton("Load more"), 10_000);
      await pressLast(mcp, "Close");
      await mcp.waitGone("graph dialog", byLabel(`commit hash ${head}`), 3000);
    });

    await step("sidebar projects tree", ["PRJ-001"], async () => {
      await press(mcp, "Group sessions by project");
      const header = await mcp.waitFor("project header", (e) => isRole(e, "button") && label(e) === "project workspace", 5000);
      if (!String(header.accessibleDescription ?? "").startsWith("expanded")) throw new Error(`project header ${header.accessibleDescription}`);
      await compareShot(mcp, "projects");
      await mcp.activate(header);
      await mcp.waitFor("collapsed", (e) => label(e) === "project workspace" && String(e.accessibleDescription ?? "").startsWith("collapsed"), 3000);
      await activateFresh(mcp, "project header", (e) => label(e) === "project workspace");
      await press(mcp, "Group sessions by time");
    });
  } finally {
    results.round3Run = await close(app);
  }
}

// ---- RAM ---------------------------------------------------------------------------------------------------

const mb = (bytes) => +(bytes / 1048576).toFixed(1);

async function sample(app, tag) {
  const summary = summarizeSample(await sampleTree([app.pid]));
  const value = { processes: summary.processCount, privateWorkingSetMb: mb(summary.privateWorkingSetBytes), workingSetMb: mb(summary.workingSetBytes) };
  log(`ram ${tag}: private WS ${value.privateWorkingSetMb} MB, ${value.processes} process(es)`);
  // 没有采到存活进程即判失败（避免 0 MB 假阳性）。
  if (value.processes === 0) {
    const code = await Promise.race([app.exited, sleep(200).then(() => "running")]);
    throw new Error(`ram ${tag}: no live process sampled (exit ${code}); stderr: ${app.stderr.join("").slice(-800)}`);
  }
  return value;
}

async function ram(agentsFile) {
  await step("ram: product build idle", [], async () => {
    const app = await launch({ automation: false, agentsFile });
    try {
      await sleep(IDLE_S * 1000);
      results.ram.product = await sample(app, "product idle");
    } finally {
      await close(app);
    }
    if (results.ram.product.privateWorkingSetMb > RAM_LIMIT_MB) throw new Error(`product idle ${results.ram.product.privateWorkingSetMb} MB > ${RAM_LIMIT_MB} MB`);
  });
  // 产品版无法被驱动；“用过命令中心、查找、@ 弹窗后回到空闲”的增量用自动化版测（同一进程前后对比）。
  await step("ram: chat features used, closed, idle", [], async () => {
    const app = await launch({ automation: true, agentsFile });
    const { mcp } = app;
    try {
      await sleep(IDLE_S * 1000);
      const before = await sample(app, "automation idle, chat features unused");
      await openPalette(mcp);
      await pressLast(mcp, "Files");
      await paletteQuery(mcp, "main");
      await mcp.key(KEY.esc);
      await chord(mcp, ["ctrl"], "f");
      await mcp.key(KEY.esc);
      await typeDraft(mcp, "see @src");
      await typeDraft(mcp, "");
      await sleep(IDLE_S * 1000);
      const after = await sample(app, "automation idle after chat features");
      results.ram.chat = { before, after, growthMb: +(after.privateWorkingSetMb - before.privateWorkingSetMb).toFixed(1) };
      if (after.privateWorkingSetMb > RAM_LIMIT_MB) throw new Error(`idle after chat features ${after.privateWorkingSetMb} MB > ${RAM_LIMIT_MB} MB`);
      if (results.ram.chat.growthMb > RAM_GROWTH_MB) throw new Error(`chat features left +${results.ram.chat.growthMb} MB > ${RAM_GROWTH_MB} MB`);
    } finally {
      await close(app);
    }
  });
}

// ---- main --------------------------------------------------------------------------------------------------

async function main() {
  mkdirSync(reportDir, { recursive: true });
  // 用户决定 UI 只用英文、不再新增 zh-CN 翻译（2026-11-03）：缺少的 msgid 只记录数量，不再让门禁失败。
  await step("po: zh-CN catalog (report only)", [], async () => {
    const missing = missingMsgids();
    results.po = { msgids: usedMsgids().size, missing: missing.length, reportOnly: true, sample: missing.slice(0, 5).map((m) => m.id) };
  });
  await step("build", [], async () => build());
  const agentsFile = fixtures();
  // PHASE9_CHAT_ONLY=round2：开发时只跑第二轮场景（结果不算完整门禁）。
  const only = process.env.PHASE9_CHAT_ONLY;
  if (!only) {
    await firstRun(agentsFile);
    await narrow(agentsFile);
  }
  if (!only || only === "round2") await round2(agentsFile);
  if (!only || only === "round3") await round3(agentsFile);
  if (only) return;
  if (process.env.PHASE9_CHAT_SKIP_RAM === "1") return;
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
  writeFileSync(path.join(reportDir, "phase9-chat-ui.json"), `${JSON.stringify(results, null, 2)}\n`);
}
if (failure) {
  process.stdout.write(`PHASE9_CHAT_UI_FAIL ${failure.step ?? "main"} ${String(failure.message ?? failure).replace(/\s+/g, " ")}\n`);
  process.exitCode = 1;
} else {
  process.stdout.write(`PHASE9_CHAT_UI_OK rows=${Object.keys(results.rows).length}\n`);
}
