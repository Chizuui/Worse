// Phase 7 parity scenarios S1-S10 (spec phase-7-parity.md §5). Each scenario gets
// `ctx = { page, provider, ws, home, tag, state, note }` and throws on a failed criterion. `tag` keeps the
// markers of different app runs apart in the shared mock provider ("H" Harness, "Z" ZCode, ...).
// `state` carries session ids between scenarios; `note(key, value)` records evidence for the report.
import { existsSync, watch } from "node:fs";
import { readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { dataPaths, git } from "./fixtures.mjs";
import {
  answerPermission,
  COMPOSER_INPUT,
  htmlTheme,
  newSession,
  openSession,
  PANE,
  PERMISSION,
  QUEUE_ITEM,
  send,
  STOP,
  taskItemIds,
  terminalText,
  tid,
  timelineText,
  waitGone,
  waitSessionCreated,
  waitTimeline,
} from "./ui.mjs";

export class ScenarioError extends Error {}

function check(condition, message) {
  if (!condition) throw new ScenarioError(message);
}

/** Resolves when `predicate()` holds, re-evaluated on every change in `dir` (fs.watch), with a timeout. */
export function waitForFile(dir, predicate, timeoutMs, label) {
  return new Promise((resolve, reject) => {
    let done = false;
    let watcher;
    const finish = (error, value) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      watcher?.close();
      if (error) reject(error);
      else resolve(value);
    };
    const evaluate = async () => {
      try {
        const value = await predicate();
        if (value) finish(null, value);
      } catch {
        // 文件可能正在写入；等下一次变更事件再判断。
      }
    };
    const timer = setTimeout(() => finish(new ScenarioError(`timed out after ${timeoutMs} ms waiting for ${label}`)), timeoutMs);
    watcher = watch(dir, { recursive: true }, () => void evaluate());
    void evaluate();
  });
}

async function readSettings(home) {
  return JSON.parse(await readFile(dataPaths(home).settings, "utf8"));
}

export async function s1Stream(ctx) {
  const { page, provider, tag, state } = ctx;
  const marker = `P7_HELLO_${tag}a`;
  await send(page, `${marker} say hi`);
  const record = await provider.waitFor("completed", (r) => r.marker === marker && r.kind === "hello", 60_000, marker);
  const reply = `PHASE7_REPLY_${tag}a streamed in five parts.`;
  await waitTimeline(page, reply);
  state.sessionA = await waitSessionCreated(page);
  await page.waitForSelector(tid(`task-item-${state.sessionA}`), { timeout: 30_000 });
  ctx.note("S1", { session: state.sessionA, streamMs: record.completedAt - record.at });
}

export async function s2Stop(ctx) {
  const { page, provider, tag } = ctx;
  const marker = `P7_SLOW_${tag}b`;
  await send(page, `${marker} keep talking`);
  // 渲染中的部分输出出现即证明逐段流式渲染（完整回复需要 60 s，远在停止之后）。
  await waitTimeline(page, `slow${tag}b-5 `, 60_000);
  await page.locator(STOP).first().click();
  const record = await provider.waitFor("aborted", (r) => r.marker === marker, 30_000, `${marker} aborted`);
  await waitGone(page, STOP, 30_000);
  await page.waitForSelector(COMPOSER_INPUT, { timeout: 30_000 });
  check(record.abortedAfterEvents < record.totalEvents, "stream was not cut short");
  const after = await timelineText(page);
  check(!after.includes(`slow${tag}b-399`), "timeline shows the full stream after stop");
  ctx.note("S2", { abortedAfterEvents: record.abortedAfterEvents, totalEvents: record.totalEvents });
}

export async function s3Queue(ctx) {
  const { page, provider, tag, state } = ctx;
  await newSession(page);
  const busy = `P7_BUSY_${tag}c`;
  const first = `P7_QUEUED_${tag}d`;
  const second = `P7_QUEUED_${tag}e`;
  await send(page, `${busy} work for a while`);
  await provider.waitFor("streaming", (r) => r.marker === busy, 60_000, busy);
  await send(page, `${first} first queued`);
  await send(page, `${second} second queued`);
  await page.waitForFunction(({ selector }) => document.querySelectorAll(selector).length >= 2, { selector: QUEUE_ITEM }, { timeout: 10_000 }).catch(() => {});
  const queued = await page.locator(QUEUE_ITEM).count();
  const busyRecord = await provider.waitFor("completed", (r) => r.marker === busy, 60_000, `${busy} completed`);
  check(!busyRecord.aborted, "busy stream was aborted by queueing");
  const firstRecord = await provider.waitFor("completed", (r) => r.marker === first && r.kind === "queued", 60_000, first);
  const secondRecord = await provider.waitFor("completed", (r) => r.marker === second && r.kind === "queued", 60_000, second);
  await waitTimeline(page, `PHASE7_QUEUED_DONE_${tag}e`);
  state.sessionB = await waitSessionCreated(page);
  const text = await timelineText(page);
  const order = [`PHASE7_BUSY_DONE_${tag}c`, `PHASE7_QUEUED_DONE_${tag}d`, `PHASE7_QUEUED_DONE_${tag}e`].map((value) => text.indexOf(value));
  check(queued >= 1, `queued messages were not shown as queue items (count ${queued})`);
  check(firstRecord.at >= busyRecord.completedAt, "first queued message reached the model before the busy turn ended");
  check(secondRecord.at >= firstRecord.completedAt, "second queued message reached the model before the first finished");
  check(order.every((index) => index >= 0) && order[0] < order[1] && order[1] < order[2], `timeline order is wrong: ${order.join(",")}`);
  ctx.note("S3", { queueItemsShown: queued, order: "busy→d→e" });
}

async function permissionTurn(ctx, id, fileName, kindSelector) {
  const { page, provider, ws, tag } = ctx;
  const marker = `P7_WRITE_${tag}${id}`;
  const file = path.join(ws, fileName);
  provider.plan(marker, { id: `toolu_phase7_${tag}${id}`, name: "Write", input: { file_path: file, content: `PHASE7_${fileName}\n` } });
  await send(page, `${marker} write the file`);
  await page.waitForSelector(PERMISSION, { timeout: 60_000 });
  await answerPermission(page, kindSelector);
  const result = await provider.waitFor("completed", (r) => r.marker === marker && r.kind === "tool_result", 60_000, `${marker} tool_result`);
  return { marker, file, result };
}

export async function s4Approve(ctx) {
  const { page, tag } = ctx;
  const { file, result } = await permissionTurn(ctx, "f", "phase7-approved.txt", '[data-permission-option-kind="allowOnce"]');
  check(result.toolResultIsError === false, "approved Write returned an error tool_result");
  check(existsSync(file), "approved file was not written");
  check((await readFile(file, "utf8")).includes("PHASE7_phase7-approved.txt"), "approved file content differs");
  await waitTimeline(page, `PHASE7_WRITE_DONE_${tag}f error=false`);
  ctx.note("S4", { toolResultIsError: result.toolResultIsError });
}

export async function s5Deny(ctx) {
  const { page, tag } = ctx;
  const { file, result } = await permissionTurn(ctx, "g", "phase7-denied.txt", '[data-permission-option-kind^="reject"]');
  check(result.toolResultIsError === true, "denied Write did not return an error tool_result");
  check(!existsSync(file), "denied file was written");
  await waitTimeline(page, `PHASE7_WRITE_DONE_${tag}g error=true`);
  ctx.note("S5", { toolResultIsError: result.toolResultIsError });
}

export async function s6FileTree(ctx) {
  const { page, ws } = ctx;
  if (!(await page.locator(`${tid("workspace-file-tree-panel")}:visible`).count())) {
    await page.locator(tid("virtual-project-row")).first().hover();
    await page.locator(tid("virtual-project-files")).first().click();
  }
  await page.waitForSelector(`${tid("workspace-file-tree-panel")}:visible`, { timeout: 30_000 });
  // 行的 test id 后缀是原始绝对路径；Windows 反斜杠在 CSS 属性值里必须转义。
  const rowSelector = (target) => `[data-testid="workspace-file-tree-row-${target.replaceAll("\\", "\\\\")}"]:visible`;
  const src = path.join(ws, "src");
  await page.waitForSelector(`${rowSelector(path.join(ws, "README.md"))}, ${rowSelector(src)}`, { timeout: 30_000 });
  check(await page.locator(rowSelector(path.join(ws, "README.md"))).count(), "file tree has no README.md row");
  await page.locator(rowSelector(src)).first().click();
  const hello = path.join(src, "hello.txt");
  await page.waitForSelector(rowSelector(hello), { timeout: 30_000 });
  await page.locator(rowSelector(hello)).first().click();
  await page.locator(`${tid("preview-pane")}:visible`).getByText("PHASE7_HELLO_FILE_CONTENT").first().waitFor({ timeout: 30_000 });
  ctx.note("S6", { opened: "src/hello.txt" });
}

export async function s7Git(ctx) {
  const { page, ws } = ctx;
  await writeFile(path.join(ws, "README.md"), "# Phase7 parity fixture\n\nPHASE7_README_MARKER\nchanged by the gate\n");
  await writeFile(path.join(ws, "notes.txt"), "PHASE7 notes\n");
  const status = await git(ws, "status", "--porcelain");
  check(status.includes("README.md") && status.includes("notes.txt"), "fixture repo changes were not created");
  // git status：文件树行带 git 状态标记（未跟踪 U、已修改 M），由 Host 的 git 状态驱动。
  const treeRow = (name) => page.locator(`${tid("workspace-file-tree-panel")}:visible [role="treeitem"]`).filter({ hasText: name }).first();
  if (await page.locator(`${tid("workspace-file-tree-panel")}:visible`).count()) {
    await page.waitForFunction(
      () => {
        const rows = [...document.querySelectorAll('[data-testid="workspace-file-tree-panel"] [role="treeitem"]')].filter((element) => element.getClientRects().length > 0);
        const text = (name) => rows.find((row) => row.textContent.startsWith(name))?.textContent ?? "";
        return text("notes.txt").endsWith("U") && text("README.md").endsWith("M");
      },
      undefined,
      { timeout: 30_000 },
    );
  }
  const treeStatus = { notes: await treeRow("notes.txt").textContent().catch(() => null), readme: await treeRow("README.md").textContent().catch(() => null) };
  // Git 工具在会话摘要面板里（ZCode git-commit-dialog e2e：先展开 chat-summary-panel）。
  if (!(await page.locator(`${tid("git-action-trigger")}:visible`).count())) {
    // 折叠态是状态条上的 "Changes" 按钮（aria-label "Expand status"），展开后才渲染 git-action-trigger。
    const expand = page.locator(`${PANE} button[aria-label="Expand status"]:visible`).first();
    if (await expand.count()) await expand.click();
    else await page.locator(`${tid("chat-summary-panel")} button[aria-label]:visible`).first().click();
  }
  const trigger = page.locator(`${tid("git-action-trigger")}:visible`).first();
  await trigger.waitFor({ timeout: 30_000 });
  await trigger.click();
  await page.waitForSelector(tid("git-commit-dialog"), { timeout: 30_000 });
  const dialogText = await page.locator(tid("git-commit-dialog")).innerText();
  const message = "phase7 parity commit";
  await page.locator(tid("git-commit-message-input")).fill(message);
  const include = page.locator(tid("git-commit-include-unstaged"));
  if ((await include.count()) && (await include.getAttribute("aria-checked")) === "false") await include.click();
  await page.locator(tid("git-commit-action-item-commit")).click();
  await waitGone(page, tid("git-commit-dialog"), 30_000);
  const head = await waitForFile(
    path.join(ws, ".git"),
    async () => {
      const subject = await git(ws, "log", "-1", "--format=%s");
      return subject === message ? subject : null;
    },
    30_000,
    "the commit in git log",
  );
  const after = await git(ws, "status", "--porcelain");
  check(after === "", `working tree not clean after commit: ${after}`);
  ctx.note("S7", { head, treeStatus, dialogListedChanges: /README|notes/.test(dialogText) });
}

export async function s8Terminal(ctx) {
  const { page, ws, tag } = ctx;
  if (!(await page.locator('[data-testid="terminal"] .xterm-helper-textarea').count())) {
    await page.locator(tid("terminal-toggle")).first().click();
  }
  await page.waitForSelector('[data-testid="terminal"] .xterm-helper-textarea', { timeout: 60_000, state: "attached" });
  // 等 shell 提示符出现后再输入（与 ZCode r3 terminal e2e 一致：最后一行以 > $ # 结尾）。
  await page.waitForFunction(
    () => {
      const rows = [...document.querySelectorAll('[data-testid="terminal"] .xterm-rows')].filter((element) => element.getClientRects().length > 0);
      const text = rows.at(-1)?.innerText ?? "";
      return /[>$#]\s*$/.test(text.replaceAll(" ", " ").trimEnd());
    },
    undefined,
    { timeout: 60_000 },
  );
  const textareas = page.locator('[data-testid="terminal"] .xterm-helper-textarea');
  await textareas.last().focus();
  const dir = `phase7-term-${tag}`;
  const token = `PHASE7_TERM_${tag}_OK`;
  await page.keyboard.type(`mkdir ${dir}`);
  await page.keyboard.press("Enter");
  await waitForFile(ws, async () => existsSync(path.join(ws, dir)), 30_000, `${dir} created by the terminal`);
  await page.keyboard.type(`echo ${token}`);
  await page.keyboard.press("Enter");
  await page.waitForFunction(
    (expected) => {
      const rows = [...document.querySelectorAll('[data-testid="terminal"] .xterm-rows')].filter((element) => element.getClientRects().length > 0);
      const lines = (rows.at(-1)?.innerText ?? "").replaceAll(" ", " ").split("\n").map((line) => line.trim());
      return lines.filter((line) => line.includes(expected)).length >= 2;
    },
    token,
    { timeout: 30_000 },
  );
  ctx.note("S8", { lastLines: (await terminalText(page)).split("\n").filter(Boolean).slice(-3) });
}

export async function s9Sessions(ctx) {
  const { page, provider, tag, state } = ctx;
  await newSession(page);
  const marker = `P7_HELLO_${tag}h`;
  await send(page, `${marker} third session`);
  await provider.waitFor("completed", (r) => r.marker === marker && r.kind === "hello", 60_000, marker);
  const replyH = `PHASE7_REPLY_${tag}h streamed`;
  // 会话 A 的最后一轮是 S2 被停止的 P7_SLOW 消息；时间线是虚拟列表，只断言可见的最后一轮。
  const replyA = `P7_SLOW_${tag}b keep talking`;
  await waitTimeline(page, replyH);
  state.sessionC = await waitSessionCreated(page);
  await openSession(page, state.sessionA);
  await waitTimeline(page, replyA);
  check(!(await timelineText(page)).includes(replyH), "session A shows session C's reply");
  await openSession(page, state.sessionC);
  await waitTimeline(page, replyH);
  check(!(await timelineText(page)).includes(replyA), "session C shows session A's reply");
  const ids = await taskItemIds(page);
  for (const id of [state.sessionA, state.sessionB, state.sessionC]) check(ids.includes(id), `sidebar lacks session ${id}`);
  ctx.note("S9", { sessions: [state.sessionA, state.sessionB, state.sessionC] });
}

async function openSettingsSection(page, section) {
  if (!(await page.locator(tid("settings-page")).count())) {
    await page.locator(tid("task-settings-button")).first().click();
    await page.waitForSelector(tid("settings-page"), { timeout: 30_000 });
  }
  await page.locator(tid(`settings-section-nav-${section}`)).first().click();
}

export async function setLocale(page, home, locale) {
  await openSettingsSection(page, "general");
  await page.locator(tid("settings-locale-select-trigger")).first().click();
  await page.locator(tid(`settings-locale-select-item-${locale}`)).first().click();
  const marker = locale === "zh-CN" ? "界面语言" : "Language";
  await page.waitForFunction((expected) => document.body.innerText.includes(expected), marker, { timeout: 30_000 });
  await waitForFile(
    path.dirname(dataPaths(home).settings),
    async () => {
      const settings = await readSettings(home);
      return settings.locale === locale && settings.localePreference === locale;
    },
    30_000,
    `setting.json locale ${locale}`,
  );
}

/** Theme select in Settings → Appearance ("App theme" row, a Radix Select without a test id). */
export async function setTheme(page, mode) {
  await openSettingsSection(page, "appearance");
  // "App theme" 行的 Radix Select 没有 test id；按当前取值文本（Dark/Light/System）定位触发器。
  const trigger = page
    .locator(`${tid("settings-page")} button[role="combobox"]:visible`)
    .filter({ hasText: /^\s*(Dark|Light|System|深色|浅色|跟随系统|系统)\s*$/ })
    .first();
  await trigger.click();
  const label = { "zai-light": /^(Light|浅色)/, "zai-dark": /^(Dark|深色)/ }[mode];
  await page.getByRole("option", { name: label }).first().click();
  await page.waitForFunction(
    (expected) => document.documentElement.classList.contains(`theme-${expected}`),
    mode,
    { timeout: 15_000 },
  );
}

export async function closeSettings(page) {
  if (await page.locator(tid("settings-back-button")).count()) await page.locator(tid("settings-back-button")).first().click();
  await page.waitForSelector(COMPOSER_INPUT, { timeout: 30_000 });
}

export async function s10LocaleTheme(ctx) {
  const { page, home } = ctx;
  await setLocale(page, home, "zh-CN");
  const helpZh = await page.locator(tid("workspace-help-menu-trigger")).first().getAttribute("aria-label").catch(() => null);
  await setLocale(page, home, "en-US");
  const before = await htmlTheme(page);
  const target = before.classes.includes("theme-zai-light") ? "zai-dark" : "zai-light";
  await setTheme(page, target);
  const after = await htmlTheme(page);
  check(after.stored === target, `localStorage zcode-theme is ${after.stored}, expected ${target}`);
  check(after.classes.includes(target === "zai-dark" ? "dark" : `theme-${target}`), `html classes ${after.classes.join(" ")}`);
  await closeSettings(page);
  ctx.state.theme = target;
  ctx.note("S10", { helpLabelZh: helpZh, themeBefore: before, themeAfter: after });
}

/** S11 runs after a relaunch on the same data (driven by the gate). */
export async function s11Restore(ctx) {
  const { page, tag, state, home } = ctx;
  await page.waitForSelector(tid(`task-item-${state.sessionA}`), { timeout: 60_000 });
  const ids = await taskItemIds(page);
  for (const id of [state.sessionA, state.sessionB, state.sessionC]) check(ids.includes(id), `after restart the sidebar lacks session ${id}`);
  const active = await page.locator(PANE).first().getAttribute("data-session-id");
  await openSession(page, state.sessionC);
  await waitTimeline(page, `PHASE7_REPLY_${tag}h streamed`);
  await openSession(page, state.sessionB);
  await waitTimeline(page, `PHASE7_QUEUED_DONE_${tag}e`);
  const theme = await htmlTheme(page);
  check(theme.stored === state.theme, `theme after restart is ${theme.stored}, expected ${state.theme}`);
  const settings = await readSettings(home);
  check(settings.locale === "en-US", `locale after restart is ${settings.locale}`);
  ctx.note("S11", { restoredActiveSession: active, theme: theme.stored });
}

export const SCENARIOS = [
  ["S1", "send + streamed reply", s1Stream],
  ["S2", "stop mid-stream", s2Stop],
  ["S3", "queue while busy", s3Queue],
  ["S4", "permission approve", s4Approve],
  ["S5", "permission deny", s5Deny],
  ["S6", "file tree + open file", s6FileTree],
  ["S7", "git status + commit", s7Git],
  ["S8", "terminal command", s8Terminal],
  ["S9", "multiple sessions", s9Sessions],
  ["S10", "language + theme", s10LocaleTheme],
];
