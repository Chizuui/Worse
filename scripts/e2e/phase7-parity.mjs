#!/usr/bin/env node
// Phase 7 (B) gate: functional parity Harness vs ZCode, Harness↔ZCode rollback on shared data, kill switch.
// Spec: Worse/docs/specs/harness/phase-7-parity.md. Prints PHASE7_PARITY_OK <json> or PHASE7_PARITY_FAIL <step>: <msg>.
//
// Usage: node scripts/e2e/phase7-parity.mjs [--only harness|zcode] [--skip-rollback]
// Needs: Worse/target/debug/harness-app.exe (cargo build -p harness-app) and packages/desktop/out (ZCode main build).
import { installIsolatedHome, isolatedHomeDir } from "./lib/isolatedHome.mjs";

installIsolatedHome("phase7-parity");

const { execFile } = await import("node:child_process");
const { mkdir, writeFile } = await import("node:fs/promises");
const path = (await import("node:path")).default;
const process = (await import("node:process")).default;
const { GateError } = await import("./phase2/gate.mjs");
const { launchApp, startDeadCloud, stopDevServers, stopStarted, trackedAlive, worseRoot } = await import("./phase7-parity/apps.mjs");
const { startMockProvider } = await import("./phase7-parity/mockProvider.mjs");
const { createWorkspace, seedZcodeData } = await import("./phase7-parity/fixtures.mjs");
const { diffShapes, shapeOf, snapshot } = await import("./phase7-parity/dataShape.mjs");
const { SCENARIOS, s11Restore } = await import("./phase7-parity/scenarios.mjs");
const { rollback } = await import("./phase7-parity/rollback.mjs");
const { waitForWorkspace } = await import("./phase7-parity/ui.mjs");

const args = process.argv.slice(2);
const only = args.includes("--only") ? args[args.indexOf("--only") + 1] : null;
const skipRollback = args.includes("--skip-rollback");
const evidenceDir = path.join(worseRoot, ".work", "phase7", "parity");
const runDir = path.join(worseRoot, ".work", "tmp", `phase7-parity-${process.pid}`);
await mkdir(evidenceDir, { recursive: true });
await mkdir(runDir, { recursive: true });

const root = isolatedHomeDir();
const results = { scenarios: {}, notes: {}, dataShape: null, rollback: null, cleanup: null };
const log = (line) => process.stdout.write(`${line}\n`);

/** Processes whose command line mentions the isolated home: only this gate's apps, Hosts and agents can. */
const scanErrors = [];
function scanOnce() {
  return new Promise((resolve) => {
    // 修复：扫描脚本自身的 powershell.exe 命令行也包含隔离 home 路径，必须排除 $PID，否则 WMI 正常时会把自己报成残留。
    const script = `Get-CimInstance Win32_Process -ErrorAction Stop | Where-Object { $_.CommandLine -and $_.CommandLine.Contains('${root.replaceAll("'", "''")}') -and $_.ProcessId -ne ${process.pid} -and $_.ProcessId -ne $PID } | ForEach-Object { "$($_.ProcessId) $($_.Name)" }`;
    execFile("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", script], { windowsHide: true, encoding: "utf8" }, (error, stdout, stderr) => {
      const failed = Boolean(error) || Boolean(stderr?.trim());
      resolve({ failed, message: (stderr || error?.message || "").trim().split(/\r?\n/)[0], lines: (stdout ?? "").split(/\r?\n/).map((line) => line.trim()).filter(Boolean) });
    });
  });
}

async function leftoverProcesses() {
  // WMI 在本机间歇性返回 "Call cancelled"：失败时扫描为空不代表没有残留，重试；仍失败则记录，
  // 并由 trackedAlive()（按 PID，不依赖 WMI）兜住本 gate 的直接子进程。
  for (let attempt = 1; attempt <= 3; attempt += 1) {
    const result = await scanOnce();
    if (!result.failed) return result.lines;
    scanErrors.push(`attempt ${attempt}: ${result.message}`);
  }
  return [];
}

async function runApp(kind, provider, cloud) {
  const tag = kind === "harness" ? "H" : "Z";
  const home = path.join(root, `parity-${kind}`);
  await mkdir(home, { recursive: true });
  const ws = await createWorkspace(home);
  await seedZcodeData({ runDir, providerBaseUrl: provider.baseUrl, workspace: ws, home });
  const outcome = {};
  results.scenarios[kind] = outcome;
  const state = {};
  const note = (key, value) => {
    results.notes[`${kind}.${key}`] = value;
  };
  let app = await launchApp(kind, { cloud, evidenceDir, label: `${kind}-1`, home });
  try {
    await waitForWorkspace(app.page);
    for (const [id, title, run] of SCENARIOS) {
      const started = Date.now();
      try {
        await run({ page: app.page, provider, ws, home, tag, state, note });
        outcome[id] = { title, pass: true, ms: Date.now() - started };
        log(`  ${kind} ${id} ${title}: pass (${Date.now() - started} ms)`);
      } catch (error) {
        outcome[id] = { title, pass: false, error: String(error.message).split("\n")[0] };
        log(`  ${kind} ${id} ${title}: FAIL ${outcome[id].error}`);
        await app.page.screenshot({ path: path.join(evidenceDir, `fail-${kind}-${id}.png`) }).catch(() => {});
      }
    }
    const closed = await app.close();
    // S11：正常退出（exit 0）后在同一数据上重启并恢复。
    app = null;
    const started = Date.now();
    try {
      if (!closed.closed || closed.exitCode !== 0) throw new Error(`quit was not clean: ${JSON.stringify(closed)}`);
      app = await launchApp(kind, { cloud, evidenceDir, label: `${kind}-2`, home });
      await waitForWorkspace(app.page);
      await s11Restore({ page: app.page, provider, ws, home, tag, state, note });
      outcome.S11 = { title: "restart restores", pass: true, ms: Date.now() - started, quit: closed };
      log(`  ${kind} S11 restart restores: pass`);
    } catch (error) {
      outcome.S11 = { title: "restart restores", pass: false, error: String(error.message).split("\n")[0], quit: closed };
      log(`  ${kind} S11 restart restores: FAIL ${outcome.S11.error}`);
      await app?.page.screenshot({ path: path.join(evidenceDir, `fail-${kind}-S11.png`) }).catch(() => {});
    }
  } finally {
    if (app) {
      const closed = await app.close();
      outcome.finalQuit = closed;
    }
  }
  return snapshot(home);
}

let failure = null;
let provider;
let cloud;
try {
  provider = await startMockProvider();
  cloud = await startDeadCloud();
  const snaps = {};
  for (const kind of only ? [only] : ["harness", "zcode"]) {
    log(`[phase7-parity] scenarios on ${kind}`);
    snaps[kind] = await runApp(kind, provider, cloud);
  }
  if (snaps.harness && snaps.zcode) {
    const diff = diffShapes(shapeOf(snaps.harness), shapeOf(snaps.zcode), "harness", "zcode");
    results.dataShape = { same: diff.length === 0, diff, harness: shapeOf(snaps.harness) };
    log(`[phase7-parity] persisted data shape: ${diff.length === 0 ? "same" : diff.join(" | ")}`);
  }
  if (!skipRollback) {
    log("[phase7-parity] rollback Harness -> ZCode -> Harness + kill switch");
    try {
      await rollback({ root, runDir, provider, cloud, evidenceDir, note: (key, value) => (results.notes[key] = value) });
      results.rollback = { pass: true };
      log("  rollback: pass");
    } catch (error) {
      results.rollback = { pass: false, step: error.step ?? "ROLLBACK", error: String(error.message).split("\n")[0] };
      log(`  rollback: FAIL ${results.rollback.step} ${results.rollback.error}`);
    }
  }
  const failed = Object.entries(results.scenarios).flatMap(([kind, outcome]) =>
    Object.entries(outcome)
      .filter(([id, value]) => id.startsWith("S") && !value.pass)
      .map(([id, value]) => `${kind} ${id}: ${value.error}`),
  );
  if (failed.length) failure = new GateError("SCENARIO", failed.join("; "));
  else if (results.dataShape && !results.dataShape.same) failure = new GateError("DATA_SHAPE", results.dataShape.diff.join("; "));
  else if (results.rollback && !results.rollback.pass) failure = new GateError(results.rollback.step, results.rollback.error);
} catch (error) {
  failure = error;
} finally {
  results.cleanup = { stopped: [...stopStarted(), ...stopDevServers()] };
  await provider?.close();
  cloud?.server.closeAllConnections?.();
  cloud?.server.close();
}

const leftovers = [...new Set([...(await leftoverProcesses()), ...trackedAlive()])];
results.cleanup.leftovers = leftovers;
if (scanErrors.length) results.cleanup.scanErrors = scanErrors;
if (leftovers.length && !failure) failure = new GateError("CLEANUP", `processes still running from the isolated home: ${leftovers.join(", ")}`);
await writeFile(path.join(evidenceDir, "parity-results.json"), `${JSON.stringify(results, null, 2)}\n`);

if (failure) {
  log(`PHASE7_PARITY_FAIL ${failure.step ?? "UNEXPECTED"}: ${failure.message}`);
  if (!(failure instanceof GateError)) process.stderr.write(`${failure.stack}\n`);
  process.exitCode = 1;
} else {
  const summary = Object.fromEntries(
    Object.entries(results.scenarios).map(([kind, outcome]) => [kind, Object.keys(outcome).filter((id) => id.startsWith("S") && outcome[id].pass).length]),
  );
  log(`PHASE7_PARITY_OK ${JSON.stringify({ scenariosPassed: summary, dataShape: results.dataShape?.same ?? null, rollback: results.rollback?.pass ?? null, killSwitch: results.notes.rollback?.killSwitch?.map((entry) => entry.label) ?? null })}`);
}
