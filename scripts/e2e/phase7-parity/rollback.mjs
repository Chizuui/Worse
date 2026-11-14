// Rollback Harness → ZCode → Harness on one data dir, plus the kill switch (spec phase-7-parity.md §6, §7).
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdir, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import process from "node:process";
import { harnessExe, homeEnv, launchApp } from "./apps.mjs";
import { diffTrees, hashTree, lostRows, snapshot } from "./dataShape.mjs";
import { createWorkspace, PROVIDER_ID, seedZcodeData } from "./fixtures.mjs";
import { ScenarioError, setLocale, closeSettings } from "./scenarios.mjs";
import { newSession, openSession, send, taskItemIds, tid, waitForWorkspace, waitSessionCreated, waitTimeline } from "./ui.mjs";

function check(condition, step, message) {
  if (!condition) {
    const error = new ScenarioError(message);
    error.step = step;
    throw error;
  }
}

async function helloSession(page, provider, id, { fresh }) {
  if (fresh) await newSession(page);
  const marker = `P7_HELLO_${id}`;
  await send(page, `${marker} rollback`);
  await provider.waitFor("completed", (r) => r.marker === marker && r.kind === "hello", 60_000, marker);
  await waitTimeline(page, `PHASE7_REPLY_${id} streamed`);
  return waitSessionCreated(page);
}

async function verifySessions(page, sessions, step) {
  const ids = await taskItemIds(page);
  for (const [id, sessionId] of Object.entries(sessions)) {
    check(ids.includes(sessionId), step, `session ${id} (${sessionId}) is not listed`);
    await openSession(page, sessionId);
    await waitTimeline(page, `PHASE7_REPLY_${id} streamed`);
  }
}

/** Files only Harness writes (device id, WebView2 profile root files); they must survive a ZCode run. */
async function harnessOnlyFiles(home) {
  const roaming = await hashTree(path.join(home, "AppData", "Roaming", "dev.harness.app"));
  const local = await hashTree(path.join(home, "AppData", "Local", "dev.harness.app"));
  return { roaming, local, webview2: existsSync(path.join(home, "harness-webview2")) };
}

async function quit(app, step) {
  const result = await app.close();
  check(result.closed && result.exitCode === 0, step, `${app.kind} did not quit cleanly: ${JSON.stringify(result)}`);
  return result;
}

/** Runs the Harness exe with the kill switch engaged; resolves with its exit code and stderr. */
function runKillSwitched(home, extraEnv) {
  return new Promise((resolve, reject) => {
    const child = spawn(harnessExe, [], { env: { ...process.env, ...homeEnv(home), ...extraEnv }, stdio: ["ignore", "pipe", "pipe"], windowsHide: true });
    let output = "";
    child.stdout.on("data", (chunk) => (output += chunk));
    child.stderr.on("data", (chunk) => (output += chunk));
    const timer = setTimeout(() => {
      child.kill();
      reject(new ScenarioError(`kill-switched Harness did not exit within 30 s: ${output}`));
    }, 30_000);
    child.once("exit", (code) => {
      clearTimeout(timer);
      resolve({ code, output: output.trim() });
    });
  });
}

/** §7 gate check: engaged by marker and by variable, exits 0, launches the stand-in, writes nothing. */
export async function killSwitchCheck(home) {
  const standIn = process.execPath; // node.exe：无参数且 stdin 为空时立即退出，作为“ZCode”替身。
  const marker = path.join(home, ".zcode", "harness", "kill-switch");
  await mkdir(path.dirname(marker), { recursive: true });
  await writeFile(marker, `${standIn}\n`);
  const results = [];
  for (const [label, env] of [
    ["marker", { HARNESS_KILL_SWITCH: "", HARNESS_ZCODE_EXE: "" }],
    ["variable", { HARNESS_KILL_SWITCH: "1", HARNESS_ZCODE_EXE: standIn }],
  ]) {
    if (label === "variable") await rm(marker, { force: true });
    const before = await hashTree(home);
    const run = await runKillSwitched(home, env);
    const after = await hashTree(home);
    const changes = diffTrees(before, after);
    check(run.code === 0, "KILL_SWITCH", `${label}: exit code ${run.code}`);
    check(/HARNESS_KILL_SWITCH engaged/.test(run.output) && /launched ZCode/.test(run.output), "KILL_SWITCH", `${label}: unexpected output: ${run.output}`);
    check(changes.length === 0, "KILL_SWITCH", `${label}: the kill switch changed data: ${changes.slice(0, 5).join("; ")}`);
    results.push({ label, exitCode: run.code, output: run.output, filesChanged: changes.length });
  }
  return results;
}

export async function rollback({ root, runDir, provider, cloud, evidenceDir, note }) {
  const home = path.join(root, "rollback");
  await mkdir(home, { recursive: true });
  const ws = await createWorkspace(home);
  await seedZcodeData({ runDir, providerBaseUrl: provider.baseUrl, workspace: ws, home });
  // 不固定 device id，让 Harness 按正常路径把 device-mid 写进自己的数据目录，验证它在 ZCode 运行后仍在。
  const harnessEnv = { HARNESS_DEVICE_MID: "" };
  const sessions = {};

  // 1. Harness creates two sessions and switches the language.
  let app = await launchApp("harness", { cloud, evidenceDir, label: "rollback-harness-1", home, extraEnv: harnessEnv });
  await waitForWorkspace(app.page);
  sessions.R1 = await helloSession(app.page, provider, "R1", { fresh: false });
  sessions.R2 = await helloSession(app.page, provider, "R2", { fresh: true });
  await setLocale(app.page, home, "zh-CN");
  await closeSettings(app.page);
  await quit(app, "ROLLBACK_HARNESS_QUIT");
  const snapA = await snapshot(home);
  const harnessFilesA = await harnessOnlyFiles(home);
  check(Object.keys(harnessFilesA.roaming).length + Object.keys(harnessFilesA.local).length > 0, "ROLLBACK_HARNESS_DATA", "Harness wrote no data of its own");

  // 2. ZCode on the same data: everything visible, then its own changes.
  app = await launchApp("zcode", { cloud, evidenceDir, label: "rollback-zcode", home });
  await waitForWorkspace(app.page);
  const zhLabel = await app.page.locator(tid("workspace-help-menu-trigger")).first().getAttribute("aria-label").catch(() => null);
  check(zhLabel === "帮助", "ROLLBACK_ZCODE_LOCALE", `ZCode did not open in zh-CN (help label ${zhLabel})`);
  await verifySessions(app.page, { R1: sessions.R1, R2: sessions.R2 }, "ROLLBACK_ZCODE_SESSIONS");
  sessions.R3 = await helloSession(app.page, provider, "R3", { fresh: true });
  await setLocale(app.page, home, "en-US");
  await closeSettings(app.page);
  await quit(app, "ROLLBACK_ZCODE_QUIT");
  const snapB = await snapshot(home);
  const lostAB = lostRows(snapA, snapB);
  check(lostAB.length === 0, "ROLLBACK_ZCODE_DATA", `rows lost after ZCode: ${lostAB.join("; ")}`);
  const harnessFilesB = await harnessOnlyFiles(home);
  const harnessDiff = [...diffTrees(harnessFilesA.roaming, harnessFilesB.roaming), ...diffTrees(harnessFilesA.local, harnessFilesB.local)].filter(
    // WebView2/Tauri 窗口状态等可能随时间变化的文件不算数据丢失；只关心被删除的文件。
    (change) => change.startsWith("removed"),
  );
  check(harnessDiff.length === 0 && harnessFilesB.webview2, "ROLLBACK_HARNESS_DATA", `Harness data changed while ZCode ran: ${harnessDiff.join("; ")}`);

  // 3. Kill switch: Harness hands over to "ZCode" and touches nothing.
  const killSwitch = await killSwitchCheck(home);

  // 4. Harness again, switch off: all three sessions and ZCode's locale change are there.
  app = await launchApp("harness", { cloud, evidenceDir, label: "rollback-harness-2", home, extraEnv: harnessEnv });
  await waitForWorkspace(app.page);
  // 渲染层把语言偏好缓存在各自浏览器 profile 的 localStorage（zcode-locale-preference），桌面端它优先于
  // setting.json（ZCode IntlProvider 语义，preferSettingServiceLocale=false）。所以 Harness 回来时界面仍按
  // 自己上次缓存的语言显示；这里只记录观察值，并断言共享的 setting.json 没被 Harness 启动改写（见 spec §8）。
  const enLabel = await app.page.locator(tid("workspace-help-menu-trigger")).first().getAttribute("aria-label").catch(() => null);
  await verifySessions(app.page, sessions, "ROLLBACK_HARNESS_SESSIONS");
  await quit(app, "ROLLBACK_HARNESS_QUIT_2");
  const snapC = await snapshot(home);
  check(snapC.settings.value?.locale === "en-US", "ROLLBACK_HARNESS_LOCALE", `Harness changed the shared locale to ${snapC.settings.value?.locale}`);
  const lostBC = lostRows(snapB, snapC);
  check(lostBC.length === 0, "ROLLBACK_HARNESS_DATA", `rows lost after returning to Harness: ${lostBC.join("; ")}`);
  for (const [name, snap] of [["A", snapA], ["B", snapB], ["C", snapC]]) {
    check(snap.settings.valid && snap.providerConfig.valid, "ROLLBACK_JSON", `snapshot ${name}: setting.json or provider_config.json is not valid JSON`);
    check(JSON.stringify(snap.providerConfig.value).includes(PROVIDER_ID), "ROLLBACK_PROVIDER", `snapshot ${name}: the Personal Provider is gone`);
    check(JSON.stringify(snap.settings.value.recentProjects ?? []).includes("Phase7Project"), "ROLLBACK_SETTINGS", `snapshot ${name}: recent projects lost`);
    for (const db of ["tasksIndex", "cliDb"]) {
      if (snap[db].exists) check(snap[db].integrity === "ok", "ROLLBACK_DB", `snapshot ${name}: ${db} quick_check ${snap[db].integrity}`);
    }
  }
  const counts = (snap) =>
    Object.fromEntries(
      ["tasksIndex", "cliDb"].map((db) => [db, snap[db].exists ? Object.fromEntries(Object.entries(snap[db].tables).map(([table, value]) => [table, value.count])) : null]),
    );
  note("rollback", {
    sessions,
    killSwitch,
    rowCounts: { A: counts(snapA), B: counts(snapB), C: counts(snapC) },
    locale: { afterHarness: snapA.settings.value.locale, afterZcode: snapB.settings.value.locale, final: snapC.settings.value.locale, zcodeUiHelpLabel: zhLabel, harnessUiHelpLabelOnReturn: enLabel },
    harnessOnlyFiles: Object.keys(harnessFilesB.roaming).concat(Object.keys(harnessFilesB.local)).slice(0, 10),
  });
}
