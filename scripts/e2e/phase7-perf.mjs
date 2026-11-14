#!/usr/bin/env node
// Phase 7 performance gate (agent A): ZCode baseline vs Harness on this machine, same probe, same
// fixtures, same isolation. Spec: Worse/docs/specs/harness/phase-7-performance.md.
//
//   node Worse/scripts/e2e/phase7-perf.mjs
//   PHASE7_PERF_QUICK=1          idle 60 s, 6 prompts (startup N stays 5)
//   PHASE7_PERF_SKIP=a,b         skip parts: size,startup,idle,latency,service
//   PHASE7_PERF_IDLE_S=<s>       idle period before sampling (default 300)
//   PHASE7_PERF_STRICT=1         fail when a 4.1 target is missed
//
// Writes Worse/.work/phase7/{baseline*.json,harness-perf.json,perfA.md}. Prints PHASE7_PERF_OK or
// PHASE7_PERF_FAIL <step> <reason> as the last line.
//
// Safety: installIsolatedHome() runs before anything else; both apps get their own sub-home inside
// it, loopback-only network (local mock LLM, local 404 cloud, proxy to 127.0.0.1:9), and only the
// process trees this gate spawned are measured or stopped.
import { installIsolatedHome, isolatedHomeEnv, checkHomeLeaks } from "./lib/isolatedHome.mjs";

const { home } = installIsolatedHome("phase7-perf");

const { existsSync, readdirSync } = await import("node:fs");
const { mkdir, readFile, writeFile } = await import("node:fs/promises");
const path = (await import("node:path")).default;
const process = (await import("node:process")).default;
const { closeApp, detach, killLeftoverRoots, launchApp, reattach, waitWorkspace } = await import("./phase7-perf/apps.mjs");
const { createWorkspace, ensureServiceFixtures, seedHome, worseRoot, cacheRoot } = await import("./phase7-perf/fixtures.mjs");
const { ensureHarnessInstall, SEVEN_ZIP } = await import("./phase7-perf/harnessInstall.mjs");
const { startDeadCloud, startMockProvider } = await import("./phase7-perf/mockProvider.mjs");
const { cpuBetween, machineInfo, sampleTree, summarizeSample } = await import("./phase7-perf/procs.mjs");
const { runServiceSide, stats } = await import("./phase7-perf/service.mjs");
const { registryClean, registryGuardReport, restoreRegistry } = await import("./phase7-perf/regGuard.mjs");
const { ensureZcodeLayout, sevenZip, stageZcodeInstallEstimate } = await import("./phase7-perf/zcodeLayout.mjs");

const QUICK = process.env.PHASE7_PERF_QUICK === "1";
const SKIP = new Set((process.env.PHASE7_PERF_SKIP ?? "").split(",").map((part) => part.trim()).filter(Boolean));
const IDLE_S = Number(process.env.PHASE7_PERF_IDLE_S ?? (QUICK ? 60 : 300));
const CPU_WINDOW_S = Math.min(60, Math.max(10, Math.floor(IDLE_S / 3)));
// 调试用覆盖（PHASE7_PERF_STARTUP_N）；正式结果要求 N >= 5。
const STARTUP_N = Math.max(1, Number(process.env.PHASE7_PERF_STARTUP_N ?? 5));
// 最近秩 p95 在 10 个样本时就是最大值；20 个样本让 p95 取第 19 名，减少单次抖动的影响。
const PROMPTS = QUICK ? 6 : 20;
const SERVICE_REPS = 5;
const WATCH_REPS = 10;
const FIXTURE_SCALE = { bigFileBytes: 100 * 1024 * 1024, treeFiles: 60_000, repoFiles: 20_000, repoModified: 500, repoUntracked: 200 };

const reportDir = path.join(worseRoot, ".work/phase7");
const logDir = path.join(reportDir, "perf-logs");
const runDir = path.join(home, "run");

class GateError extends Error {
  constructor(step, message) {
    super(message);
    this.step = step;
  }
}

function log(line) {
  process.stdout.write(`[phase7-perf] ${line}\n`);
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** Per-app sub-home inside the isolated home (both apps must not share ZCode data). */
async function subHome(name) {
  const base = isolatedHomeEnv();
  const root = base.HOME;
  const dir = path.join(root, name);
  const env = {};
  for (const [key, value] of Object.entries(base)) {
    if (typeof value === "string" && value.toLowerCase().startsWith(root.toLowerCase())) env[key] = dir + value.slice(root.length);
  }
  if (base.HOMEDRIVE) env.HOMEPATH = dir.slice(base.HOMEDRIVE.length);
  for (const key of ["APPDATA", "LOCALAPPDATA", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME"]) {
    if (env[key]) await mkdir(env[key], { recursive: true });
  }
  return { dir, env };
}

function pctDelta(zcode, harness) {
  if (typeof zcode !== "number" || typeof harness !== "number" || zcode === 0) return null;
  return Math.round(((harness - zcode) / zcode) * 1000) / 10;
}

// ---------------------------------------------------------------------------------------------
// Latency: one prompt through the real composer.

async function runPrompt(handle, mock, id) {
  const { page } = handle;
  const marker = `P7PERFQ${id}`;
  const firstToken = `P7PERFA${id}`;
  const endToken = `P7PERFZ${id}`;
  const before = await page.evaluate((token) => {
    window.__p7.expect(token);
    return { keydowns: window.__p7.keydowns.length, acks: window.__p7.acks.length };
  }, firstToken);
  const input = page.locator('[data-testid="v4-composer-input"]').first();
  await input.click({ timeout: 30_000 });
  await page.keyboard.insertText(`Reply with the measurement marker ${marker}`);
  await page.keyboard.press("Enter");
  await page.waitForFunction(() => window.__p7.watch?.paintAt != null, null, { timeout: 60_000, polling: 50 }).catch(() => {
    throw new GateError("LATENCY", `${handle.label}: first delta ${firstToken} never reached the DOM`);
  });
  const record = await mock.waitFor("first-delta", (entry) => entry.marker === marker, 10_000, marker);
  await mock.waitFor("completed", (entry) => entry.marker === marker, 30_000, marker);
  await page.waitForFunction((token) => document.body.innerText.includes(token), endToken, { timeout: 30_000, polling: 100 });
  // 回合结束：停止按钮消失后才发送下一条（不与上一回合重叠）。
  await page.locator('[data-testid="v4-stop"]').first().waitFor({ state: "hidden", timeout: 30_000 }).catch(() => {});
  const probe = await page.evaluate((offsets) => ({ keydowns: window.__p7.keydowns.slice(offsets.keydowns), acks: window.__p7.acks.slice(offsets.acks), watch: { ...window.__p7.watch } }), before);
  const sendAt = probe.keydowns[0];
  if (typeof sendAt !== "number") throw new GateError("LATENCY", `${handle.label}: no Enter keydown recorded for ${marker}`);
  const ack = probe.acks.find((entry) => typeof entry.p7At === "number" && entry.p7At >= sendAt);
  return {
    id,
    ackMs: ack ? Math.round((ack.p7At - sendAt) * 10) / 10 : null,
    ackType: ack ? `${ack.type}:${ack.status}` : null,
    requestMs: Math.round((record.arrivedAt - sendAt) * 10) / 10,
    firstDeltaDomMs: Math.round((probe.watch.domAt - sendAt) * 10) / 10,
    firstDeltaMs: Math.round((probe.watch.paintAt - sendAt) * 10) / 10,
    deltaToPaintMs: Math.round((probe.watch.paintAt - record.firstDeltaAt) * 10) / 10,
  };
}

async function measureLatency(handle, mock, tag) {
  const warmup = await runPrompt(handle, mock, `${tag}w0`);
  const samples = [];
  for (let index = 1; index <= PROMPTS; index += 1) samples.push(await runPrompt(handle, mock, `${tag}n${index}`));
  const pick = (key) => samples.map((sample) => sample[key]);
  return {
    warmup,
    samples,
    ack: stats(pick("ackMs")),
    request: stats(pick("requestMs")),
    firstDelta: stats(pick("firstDeltaMs")),
    deltaToPaint: stats(pick("deltaToPaintMs")),
    ackTypes: [...new Set(samples.map((sample) => sample.ackType))],
  };
}

// ---------------------------------------------------------------------------------------------

async function measureSizes(harnessInstall) {
  const out = {
    harness: { installerBytes: harnessInstall.installerBytes, installedBytes: harnessInstall.installedBytes, installedFiles: harnessInstall.installedFiles, installer: path.basename(harnessInstall.installer), note: harnessInstall.installedNote },
  };
  const layoutStamp = JSON.parse(await readFile(path.join(cacheRoot, "zcode-app/phase7-layout.json"), "utf8"));
  const cacheFile = path.join(cacheRoot, "zcode-size-estimate.json");
  if (existsSync(cacheFile)) {
    const cached = JSON.parse(await readFile(cacheFile, "utf8"));
    if (JSON.stringify(cached.layoutStamp) === JSON.stringify(layoutStamp)) {
      out.zcode = { ...cached.estimate, cached: true };
      return out;
    }
  }
  log("staging the ZCode install estimate and compressing it with 7-Zip (one-time, cached)");
  const staged = await stageZcodeInstallEstimate();
  const archive = path.join(cacheRoot, "zcode-staged.7z");
  const compressed = await sevenZip(staged.staged, archive, SEVEN_ZIP);
  const estimate = {
    installedBytesLowerBound: staged.bytes,
    installedFiles: staged.files,
    installerBytesLowerBound: compressed.bytes,
    method: `${compressed.note}; staged tree = Electron 44.7.0 runtime (no default_app.asar) + out/** without maps + renderer + glm + tools + config/icons; app.asar node_modules not included`,
  };
  await writeFile(cacheFile, JSON.stringify({ layoutStamp, estimate }, null, 2));
  out.zcode = { ...estimate, cached: false };
  return out;
}

async function launchMeasured(kind, ctx, label) {
  const side = ctx.sides[kind];
  const handle = await launchApp(kind, { exe: side.exe, home: side.home.dir, homeEnv: side.home.env, cloudPort: ctx.cloud.port, logDir, label });
  try {
    const timing = await waitWorkspace(handle);
    return { handle, timing };
  } catch (error) {
    await closeApp(handle).catch(() => {});
    throw new GateError("STARTUP", `${label}: ${error.message}`);
  }
}

// Host 侧里程碑（两边运行同一份 packages/desktop/out/host，日志行格式相同，时间为本地时间、毫秒精度）。
const HOST_MILESTONES = {
  hostDbPrepEndMs: /\[(\d{4}-\d\d-\d\d \d\d:\d\d:\d\d\.\d{3})\] \[pid:\d+\] \[zcode-host\] initializing local services/,
  hostServicesReadyMs: /\[(\d{4}-\d\d-\d\d \d\d:\d\d:\d\d\.\d{3})\] \[pid:\d+\] \[zcode-host\] window Host topology, reason=base-attachment-ready/,
  zcodeHostForkMs: /^\[(\d{4}-\d\d-\d\d \d\d:\d\d:\d\d\.\d{3})\] \[pid:\d+\] \[main\] \[spawnHostProcess\] forked host process/m,
};

async function hostMilestones(label, spawnAt) {
  const text = await readFile(path.join(logDir, `${label}.log`), "utf8").catch(() => "");
  const out = {};
  for (const [name, pattern] of Object.entries(HOST_MILESTONES)) {
    const match = pattern.exec(text);
    out[name] = match ? Math.round(new Date(match[1].replace(" ", "T")).getTime() - spawnAt) : null;
  }
  return out;
}

async function measureStartup(ctx) {
  const result = { zcode: { firstRun: null, runs: [] }, harness: { firstRun: null, runs: [] } };
  for (const kind of ["zcode", "harness"]) {
    const { handle, timing } = await launchMeasured(kind, ctx, `${kind}-startup-first`);
    result[kind].firstRun = { ...timing, close: await closeApp(handle) };
    log(`${kind} first run closed: ${JSON.stringify(result[kind].firstRun.close)}`);
    log(`${kind} first run: firstWindow ${timing.firstWindowMs} ms, workspace ${timing.workspaceMs} ms${timing.onboarding ? " (onboarding dismissed)" : ""}`);
  }
  for (let run = 1; run <= STARTUP_N; run += 1) {
    for (const kind of ["zcode", "harness"]) {
      const { handle, timing } = await launchMeasured(kind, ctx, `${kind}-startup-${run}`);
      const close = await closeApp(handle);
      if (close.forced) log(`${kind} run ${run} close was forced: ${JSON.stringify(close)}`);
      const milestones = await hostMilestones(`${kind}-startup-${run}`, handle.spawnAt);
      result[kind].runs.push({ ...timing, ...milestones, close });
      log(`${kind} run ${run}: firstWindow ${timing.firstWindowMs} ms, workspace ${timing.workspaceMs} ms, host DB prep done ${milestones.hostDbPrepEndMs} ms${timing.workspaceLate ? " (late probe)" : ""}`);
    }
  }
  for (const kind of ["zcode", "harness"]) {
    const runs = result[kind].runs;
    const usable = runs.filter((run) => !run.workspaceLate);
    result[kind].firstWindow = stats(runs.map((run) => run.firstWindowMs));
    result[kind].workspace = stats((usable.length >= 5 ? usable : runs).map((run) => run.workspaceMs));
    result[kind].lateWorkspaceSamples = runs.length - usable.length;
    for (const name of Object.keys(HOST_MILESTONES)) {
      const values = runs.map((run) => run[name]).filter((value) => typeof value === "number");
      if (values.length > 0) result[kind][name] = stats(values);
    }
  }
  return result;
}

async function measureIdleAndLatency(ctx, { idle, latency }) {
  const result = {};
  for (const kind of ["zcode", "harness"]) {
    const { handle, timing } = await launchMeasured(kind, ctx, `${kind}-idle`);
    const entry = { launch: timing };
    try {
      if (idle) {
        await detach(handle);
        log(`${kind}: idle ${IDLE_S} s (CPU window ${CPU_WINDOW_S} s)`);
        await sleep((IDLE_S - CPU_WINDOW_S) * 1000);
        const first = await sampleTree([handle.child.pid]);
        await sleep(CPU_WINDOW_S * 1000);
        const second = await sampleTree([handle.child.pid]);
        entry.idle = { idleSeconds: IDLE_S, summary: summarizeSample(second), cpu: cpuBetween(first, second), processes: second.processes };
        log(`${kind}: private WS ${(entry.idle.summary.privateWorkingSetBytes / 1048576).toFixed(1)} MiB, ${entry.idle.summary.processCount} processes, CPU ${entry.idle.cpu.percentOfMachine}% machine`);
        await reattach(handle);
      }
      if (latency) {
        log(`${kind}: ${PROMPTS} prompts (+1 warm-up)`);
        entry.latency = await measureLatency(handle, ctx.mock, kind === "zcode" ? "z" : "h");
        log(`${kind}: ACK p95 ${entry.latency.ack.p95} ms, first delta p95 ${entry.latency.firstDelta.p95} ms, delta-to-paint p95 ${entry.latency.deltaToPaint.p95} ms`);
        const afterLatency = await sampleTree([handle.child.pid]);
        entry.afterLatency = summarizeSample(afterLatency);
      }
    } finally {
      entry.close = await closeApp(handle);
      log(`${kind} idle/latency app closed: ${JSON.stringify(entry.close)}`);
    }
    result[kind] = entry;
  }
  return result;
}

function findFile(dir, name) {
  const stack = [dir];
  while (stack.length > 0) {
    const current = stack.pop();
    let entries = [];
    try {
      entries = readdirSync(current, { withFileTypes: true });
    } catch {
      continue;
    }
    for (const entry of entries) {
      const full = path.join(current, entry.name);
      if (entry.isFile() && entry.name.toLowerCase() === name) return full;
      if (entry.isDirectory()) stack.push(full);
    }
  }
  return null;
}

async function measureService(ctx) {
  const fx = await ensureServiceFixtures(FIXTURE_SCALE);
  log(`service fixtures ${fx.generated ? `generated in ${fx.seconds} s` : "reused"}: ${JSON.stringify(fx.scale)}`);
  const rustHost = findFile(path.join(ctx.sides.harness.dir, "host"), "harness-host.exe");
  if (!rustHost) throw new GateError("SERVICE", "harness-host.exe not found in the extracted Harness install");
  const zcode = await runServiceSide({ label: "zcode", env: { HARNESS_RUST_SERVICES: "none" }, fx, reps: SERVICE_REPS, watchReps: WATCH_REPS, log });
  const harnessEnv = { HARNESS_RUST_HOST: rustHost };
  const harness = await runServiceSide({ label: "harness", env: harnessEnv, fx, reps: SERVICE_REPS, watchReps: WATCH_REPS, log });
  const optIn = await runServiceSide({ label: "harness-optin", env: { ...harnessEnv, HARNESS_RUST_SERVICES: "search,git,pty,fs,watch" }, fx, reps: SERVICE_REPS, watchReps: WATCH_REPS, log });
  const summarize = (side) => ({
    fileOpenUi: stats(side.fileOpen.uiOpenMs),
    range64k: stats(side.fileOpen.rangeByWindow[64 * 1024]),
    range1m: stats(side.fileOpen.rangeByWindow[1024 * 1024]),
    fileOpenFull: stats(side.fileOpen.fullReadMs),
    fileOpenPreview: stats(side.fileOpen.previewMs),
    search: stats(side.search.queryMs),
    searchIndexRefreshMs: Math.round(side.search.indexRefreshMs),
    gitStatus: stats(side.git.statusMs),
    gitGetChanges: stats(side.git.getChangesMs),
    watcher: stats(side.watcher.eventMs),
    watcherSetupMs: Math.round(side.watcher.setupMs),
  });
  const gitFloor = await rawGitFloor(fx.repo);
  log(`raw git floor: status ${gitFloor.status.median} ms, diff --numstat ${gitFloor.numstat.median} ms`);
  return { scale: fx.scale, gitFloor, zcode: { ...zcode, summary: summarize(zcode) }, harness: { ...harness, summary: summarize(harness) }, harnessOptIn: { ...optIn, summary: summarize(optIn) } };
}

// 两边的 git 服务都派生同样的 git 命令；直接计时这些命令得到服务层无法低于的下限。
async function rawGitFloor(repo) {
  const { spawn } = await import("node:child_process");
  const timeGit = (args) =>
    new Promise((resolve) => {
      const started = performance.now();
      const child = spawn("git", ["-C", repo, "--no-optional-locks", ...args], { windowsHide: true, stdio: ["ignore", "ignore", "ignore"] });
      child.once("error", () => resolve(null));
      child.once("close", () => resolve(Math.round((performance.now() - started) * 10) / 10));
    });
  const run = async (args) => {
    await timeGit(args);
    const samples = [];
    for (let index = 0; index < SERVICE_REPS; index += 1) samples.push(await timeGit(args));
    return stats(samples.filter((value) => value !== null));
  };
  return {
    status: await run(["status", "--porcelain=v2", "--branch", "-uall", "-z"]),
    numstat: await run(["diff", "--numstat", "-z", "--find-renames", "--"]),
  };
}

// ---------------------------------------------------------------------------------------------
// Report

const TARGETS = {
  firstWindow: { reduction: 30, label: "-30 %" },
  workspace: { reduction: 25, label: "-25 %" },
  idleRam: { reduction: 15, label: "-15..20 % (Windows)" },
  processCount: { reduction: 25, label: "-25 %" },
  installer: { reduction: 15, label: "-15..20 % (Windows NSIS)" },
  ack: { reduction: 20, label: "p95 -20 %" },
  firstDelta: { reduction: 15, label: "p95 -15 %" },
  deltaToPaint: { reduction: 20, label: "p95 -20 %" },
  fileOpen: { reduction: 50, label: "-50 %" },
  search: { reduction: 40, label: "p95 -40 %" },
  git: { reduction: 35, label: "-35 %" },
  watcher: { reduction: 30, label: "p95 -30 %" },
};

function verdict(key, zcode, harness) {
  const delta = pctDelta(zcode, harness);
  if (delta === null) return { delta, pass: null };
  return { delta, pass: delta <= -TARGETS[key].reduction };
}

function buildRows(results) {
  const rows = [];
  const add = (metric, key, zcode, harness, unit, layer, extra = {}) => {
    const v = key ? verdict(key, zcode, harness) : { delta: pctDelta(zcode, harness), pass: null };
    rows.push({ metric, key, zcode, harness, unit, delta: v.delta, target: key ? TARGETS[key].label : extra.target ?? "-", pass: extra.pass !== undefined ? extra.pass : v.pass, layer, note: extra.note });
  };
  const s = results.startup;
  if (s) {
    add("Cold start to first window (median, first paint)", "firstWindow", s.zcode.firstWindow.median, s.harness.firstWindow.median, "ms", "shell: Electron browser+GPU+renderer start vs Tauri + WebView2 runtime start");
    add("Cold start to first interactive workspace (median)", "workspace", s.zcode.workspace.median, s.harness.workspace.median, "ms", "shell + Host start (utilityProcess vs bundled Node sidecar) + renderer boot");
  }
  const i = results.idle;
  if (i?.zcode?.idle && i?.harness?.idle) {
    add("Idle RAM, private working set, 1 window", "idleRam", i.zcode.idle.summary.privateWorkingSetBytes / 1048576, i.harness.idle.summary.privateWorkingSetBytes / 1048576, "MiB", "per-process table below");
    add("Process count, 1 window", "processCount", i.zcode.idle.summary.processCount, i.harness.idle.summary.processCount, "", "per-process table below");
    const zc = i.zcode.idle.cpu.percentOfMachine;
    const hc = i.harness.idle.cpu.percentOfMachine;
    add("Idle CPU (% of machine)", null, zc, hc, "%", "timers / polling in shell, Host, Agent", { target: "no regression, < 1 %", pass: hc < 1 && hc <= Math.max(zc, 0.05) * 1.1 + 0.05 });
  }
  add("3-window RAM", null, null, null, "", "Harness has a single window (no multi-window API)", { target: "-35 % (non-Windows row)", pass: null, note: "not measurable" });
  const z = results.sizes?.zcode;
  const h = results.sizes?.harness;
  if (z && h) {
    const installerPass = typeof z.installerBytesLowerBound === "number" ? (h.installerBytes <= z.installerBytesLowerBound * 0.85 ? true : null) : null;
    add("Installer size (ZCode = lower-bound estimate)", "installer", z.installerBytesLowerBound / 1048576, h.installerBytes / 1048576, "MiB", "Electron runtime vs bundled Node; ZCode figure excludes app.asar node_modules", { pass: installerPass, note: installerPass === null ? "inconclusive against a lower bound" : undefined });
    add("Installed size (ZCode = lower-bound estimate)", null, z.installedBytesLowerBound / 1048576, h.installedBytes / 1048576, "MiB", "Electron (~250 MB exe + ICU/pak) vs WebView2 (system) + bundled Node 24 + host resources", { target: "info" });
  }
  const l = results.idle;
  if (l?.zcode?.latency && l?.harness?.latency) {
    add("Command-to-ACK p95", "ack", l.zcode.latency.ack.p95, l.harness.latency.ack.p95, "ms", "renderer -> Host transport (Electron MessagePort IPC vs WebView2 WebSocket attachment) -> CommandInbox");
    add("Command-to-first-delta p95 (painted)", "firstDelta", l.zcode.latency.firstDelta.p95, l.harness.latency.firstDelta.p95, "ms", "Host -> Agent -> mock LLM -> Agent -> Host -> renderer");
    add("Delta-to-paint p95", "deltaToPaint", l.zcode.latency.deltaToPaint.p95, l.harness.latency.deltaToPaint.p95, "ms", "Agent parse -> Host -> transport -> React commit -> paint");
  }
  const v = results.service;
  if (v) {
    add("100 MiB file open (UI path: readTextFile 256 KiB -> truncated, median)", "fileOpen", v.zcode.summary.fileOpenUi.median, v.harness.summary.fileOpenUi.median, "ms", "fs service (TS in both by default) + RPC round trip", { note: `fs opt-in: ${v.harnessOptIn.summary.fileOpenUi.median} ms` });
    add("100 MiB full read (100 x 1 MiB readFileRange, median)", null, v.zcode.summary.fileOpenFull.median, v.harness.summary.fileOpenFull.median, "ms", "fs service + RPC bytes; opt-in adds base64-in-JSON over the Rust bridge", { target: "info (no UI path reads it)", note: `fs opt-in: ${v.harnessOptIn.summary.fileOpenFull.median} ms` });
    add(`Search p95 (${v.scale.treeFiles} files)`, "search", v.zcode.summary.search.p95, v.harness.summary.search.p95, "ms", "search service: TS index vs Rust (harness-host)");
    add(`Git status (${v.scale.repoFiles} files, refresh+getChanges, median)`, "git", v.zcode.summary.gitStatus.median, v.harness.summary.gitStatus.median, "ms", "git service: TS runner vs Rust runner (both spawn git)");
    add("Watcher event-to-Host-client p95", "watcher", v.zcode.summary.watcher.p95, v.harness.summary.watcher.p95, "ms", "watch service (TS by default; debounce-bound)", { note: `watch opt-in: ${v.harnessOptIn.summary.watcher.p95} ms` });
  }
  return rows;
}

const fmt = (value, unit) => (typeof value === "number" ? `${Math.round(value * 10) / 10}${unit ? ` ${unit}` : ""}` : "n/a");

function processTable(idle) {
  const lines = ["| app | process | count | private WS MiB | WS MiB |", "| --- | --- | --- | --- | --- |"];
  for (const kind of ["zcode", "harness"]) {
    const byName = idle?.[kind]?.idle?.summary?.byName ?? {};
    for (const [name, entry] of Object.entries(byName).sort((a, b) => b[1].privateWorkingSet - a[1].privateWorkingSet)) {
      lines.push(`| ${kind} | ${name} | ${entry.count} | ${(entry.privateWorkingSet / 1048576).toFixed(1)} | ${(entry.workingSet / 1048576).toFixed(1)} |`);
    }
  }
  return lines.join("\n");
}

async function writeReport(results, rows) {
  const lines = [
    "# Phase 7A performance: ZCode baseline vs Harness",
    "",
    `Generated by \`node Worse/scripts/e2e/phase7-perf.mjs\` at ${results.finishedAt}. Spec: \`Worse/docs/specs/harness/phase-7-performance.md\`.`,
    `Machine: ${results.machine.cpu}, ${results.machine.logicalCpus} logical CPUs, ${(results.machine.totalRamBytes / 1073741824).toFixed(1)} GiB RAM, ${results.machine.os}; ${results.machine.processCount} processes running at start (the user's other apps keep running).`,
    `Builds: ZCode = packaged-like layout of the current checkout (Electron 44.7.0, \`ZCODE_ENV=test\` build); Harness = ${results.sizes?.harness?.installer ?? "release NSIS installer"} payload.`,
    `Mode: ${QUICK ? "QUICK" : "full"}; idle ${IDLE_S} s; startup N=${STARTUP_N}; prompts ${PROMPTS}; skipped: ${[...SKIP].join(",") || "none"}.`,
    "",
    "| metric | ZCode | Harness | delta % | target | pass/fail | layer |",
    "| --- | --- | --- | --- | --- | --- | --- |",
    ...rows.map(
      (row) =>
        `| ${row.metric} | ${fmt(row.zcode, row.unit)} | ${fmt(row.harness, row.unit)} | ${row.delta === null ? "n/a" : `${row.delta > 0 ? "+" : ""}${row.delta}`} | ${row.target} | ${row.pass === true ? "PASS" : row.pass === false ? "FAIL" : row.note ?? "n/a"}${row.note && row.pass !== null ? ` (${row.note})` : ""} | ${row.layer} |`,
    ),
    "",
    "delta % = (Harness - ZCode) / ZCode; negative is better for Harness.",
    "",
  ];
  if (results.idle?.zcode?.idle) {
    lines.push("## Idle process breakdown (after the idle period, 1 window)", "", processTable(results.idle), "");
  }
  if (results.startup) {
    lines.push("## Startup samples (ms after spawn)", "");
    for (const kind of ["zcode", "harness"]) {
      const s = results.startup[kind];
      lines.push(
        `- ${kind}: first run (fresh profile) firstWindow ${s.firstRun.firstWindowMs}, workspace ${s.firstRun.workspaceMs}; runs firstWindow [${s.runs.map((run) => run.firstWindowMs).join(", ")}], workspace [${s.runs.map((run) => run.workspaceMs).join(", ")}], late workspace samples ${s.lateWorkspaceSamples}; Host DB prep done [${s.runs.map((run) => run.hostDbPrepEndMs).join(", ")}], Host services ready [${s.runs.map((run) => run.hostServicesReadyMs).join(", ")}]${kind === "zcode" ? `, Host forked [${s.runs.map((run) => run.zcodeHostForkMs).join(", ")}]` : ""}`,
      );
    }
    lines.push("");
  }
  if (results.idle?.zcode?.latency) {
    lines.push("## Latency samples (ms)", "");
    for (const kind of ["zcode", "harness"]) {
      const l = results.idle[kind].latency;
      lines.push(`- ${kind}: ACK p50/p95 ${l.ack.p50}/${l.ack.p95} (${l.ackTypes.join(", ")}); request-at-mock p50 ${l.request.p50}; first delta p50/p95 ${l.firstDelta.p50}/${l.firstDelta.p95}; delta-to-paint p50/p95 ${l.deltaToPaint.p50}/${l.deltaToPaint.p95}`);
    }
    lines.push("");
  }
  if (results.service) {
    lines.push("## Service layer (ms)", "", "| metric | ZCode (TS) | Harness default | Harness fs+watch opt-in |", "| --- | --- | --- | --- |");
    const keys = [
      ["file open UI path (readTextFile 256 KiB) p50/p95", "fileOpenUi"],
      ["file stat + readTextFile 256 KiB p50/p95", "fileOpenPreview"],
      ["full read 100 x 1 MiB p50/p95", "fileOpenFull"],
      ["single readFileRange 64 KiB p50/p95", "range64k"],
      ["single readFileRange 1 MiB p50/p95", "range1m"],
      ["search p50/p95", "search"],
      ["git status p50/p95", "gitStatus"],
      ["git getChanges p50/p95", "gitGetChanges"],
      ["watcher p50/p95", "watcher"],
    ];
    for (const [label, key] of keys) {
      const cell = (side) => `${results.service[side].summary[key].p50}/${results.service[side].summary[key].p95}`;
      lines.push(`| ${label} | ${cell("zcode")} | ${cell("harness")} | ${cell("harnessOptIn")} |`);
    }
    lines.push(
      `| search index refresh | ${results.service.zcode.summary.searchIndexRefreshMs} | ${results.service.harness.summary.searchIndexRefreshMs} | ${results.service.harnessOptIn.summary.searchIndexRefreshMs} |`,
      `| watcher setup (recursive, ${results.service.scale.treeFiles} files) | ${results.service.zcode.summary.watcherSetupMs} | ${results.service.harness.summary.watcherSetupMs} | ${results.service.harnessOptIn.summary.watcherSetupMs} |`,
      "",
      ...(results.service.gitFloor
        ? [`Raw git floor on the same repo (no service, median): \`git status --porcelain=v2 -uall\` ${results.service.gitFloor.status.median} ms, \`git diff --numstat --find-renames\` ${results.service.gitFloor.numstat.median} ms. Both stacks run status and both numstats per call.`, ""]
        : []),
      `Routes seen (Harness default): ${results.service.harness.routes.join(", ") || "none"}`,
      "",
    );
  }
  if (results.notes.length > 0) lines.push("## Notes", "", ...results.notes.map((note) => `- ${note}`), "");
  const analysisFile = path.join(reportDir, "perfA-analysis.md");
  if (existsSync(analysisFile)) lines.push(await readFile(analysisFile, "utf8"));
  await writeFile(path.join(reportDir, "perfA.md"), `${lines.join("\n")}\n`);
}

function stripForBaseline(results, kind) {
  const pick = (value) => value?.[kind];
  return {
    side: kind,
    finishedAt: results.finishedAt,
    machine: results.machine,
    config: results.config,
    build: results.builds[kind],
    startup: pick(results.startup),
    idle: pick(results.idle),
    sizes: pick(results.sizes),
    service: results.service ? (kind === "zcode" ? results.service.zcode : { default: results.service.harness, optIn: results.service.harnessOptIn, scale: results.service.scale }) : undefined,
    notes: results.notes,
  };
}

async function writeOutputs(results, rows) {
  await mkdir(reportDir, { recursive: true });
  const stamp = results.finishedAt.replace(/[:.]/g, "-");
  // 部分/调试运行（跳过阶段、QUICK 或 N<5）不得占用 baseline.json，只有完整运行才能成为基线。
  const partial = SKIP.size > 0 || QUICK || STARTUP_N < 5;
  let baselineFile = path.join(reportDir, partial ? `baseline-partial-${stamp}.json` : "baseline.json");
  if (!partial && existsSync(baselineFile)) baselineFile = path.join(reportDir, `baseline-${stamp}.json`);
  await writeFile(baselineFile, `${JSON.stringify(stripForBaseline(results, "zcode"), null, 2)}\n`);
  await writeFile(path.join(reportDir, "harness-perf.json"), `${JSON.stringify({ ...stripForBaseline(results, "harness"), rows }, null, 2)}\n`);
  await writeReport(results, rows);
  return baselineFile;
}

// ---------------------------------------------------------------------------------------------

async function main() {
  if (process.platform !== "win32") throw new GateError("PLATFORM", "the Phase 7 performance gate measures the Windows builds");
  await mkdir(runDir, { recursive: true });
  await mkdir(logDir, { recursive: true });
  const machine = await machineInfo();
  log(`machine: ${machine.cpu}, ${machine.logicalCpus} CPUs, ${machine.processCount} processes running`);

  const layout = await ensureZcodeLayout().catch((error) => {
    throw new GateError("ZCODE_LAYOUT", error.message);
  });
  log(`ZCode layout ${layout.rebuilt ? "built" : "reused"}: ${layout.exe}`);
  const harnessInstall = await ensureHarnessInstall({ log }).catch((error) => {
    throw new GateError("HARNESS_INSTALL", error.message);
  });
  log(`Harness install ${harnessInstall.reused ? "reused" : "extracted"}: ${harnessInstall.dir}`);

  const cloud = await startDeadCloud();
  const mock = await startMockProvider();
  const zHome = await subHome("zcode");
  const hHome = await subHome("harness");
  for (const sideHome of [zHome, hHome]) {
    const workspace = await createWorkspace(sideHome.dir);
    await seedHome({ home: sideHome.dir, runDir, providerBaseUrl: mock.baseUrl, workspace });
  }
  const ctx = {
    cloud,
    mock,
    sides: {
      zcode: { exe: layout.exe, home: zHome, dir: layout.dir },
      harness: { exe: harnessInstall.exe, home: hHome, dir: harnessInstall.dir },
    },
  };
  const results = {
    startedAt: new Date().toISOString(),
    machine,
    config: { quick: QUICK, skip: [...SKIP], idleSeconds: IDLE_S, cpuWindowSeconds: CPU_WINDOW_S, startupN: STARTUP_N, prompts: PROMPTS, serviceReps: SERVICE_REPS, watchReps: WATCH_REPS, fixtureScale: FIXTURE_SCALE },
    builds: {
      zcode: { layout: layout.dir, electron: "44.7.0", note: "packages/desktop/out (ZCODE_ENV=test) + vite-built renderer; app.isPackaged=true" },
      harness: { installer: harnessInstall.installer, installerMtime: harnessInstall.installerMtime, sha256: harnessInstall.sha256, dir: harnessInstall.dir },
    },
    notes: [],
  };
  try {
    if (!SKIP.has("size")) {
      results.sizes = await measureSizes(harnessInstall);
      log(`sizes: Harness installer ${(results.sizes.harness.installerBytes / 1048576).toFixed(1)} MiB, ZCode lower bound ${results.sizes.zcode.installerBytesLowerBound === null ? "n/a" : (results.sizes.zcode.installerBytesLowerBound / 1048576).toFixed(1)} MiB`);
    }
    if (!SKIP.has("startup")) results.startup = await measureStartup(ctx);
    if (!SKIP.has("idle") || !SKIP.has("latency")) results.idle = await measureIdleAndLatency(ctx, { idle: !SKIP.has("idle"), latency: !SKIP.has("latency") });
    if (!SKIP.has("service")) results.service = await measureService(ctx);
  } finally {
    await mock.close().catch(() => {});
    await cloud.close().catch(() => {});
    results.cloudRequests = [...new Set(cloud.paths)].slice(0, 40);
  }
  results.notes.push("Harness has no multi-window API; the 3-window RAM target is not measurable.");
  results.notes.push("ZCode installer/installed sizes are lower bounds (no ZCode release installed here; electron-builder NSIS cannot run offline).");
  results.notes.push("Service layer runs both sides' Host under the same Node (the ZCode Host normally runs in Electron's utilityProcess).");
  results.notes.push("Tauri resolves its own log/app-data dir through known folders (real %LOCALAPPDATA%\\dev.harness.app); accepted Phase 6 behaviour.");
  if (results.cloudRequests.length > 0) results.notes.push(`Requests that reached the local 404 cloud stand-in: ${results.cloudRequests.join(", ")}`);
  await restoreRegistry();
  results.registryGuard = registryGuardReport();
  results.notes.push(`ZCode rewrites HKCU zcode:// and the Explorer "Open in ZCode" menu on every start; the gate exported them first and restored them ${results.registryGuard.restores} time(s).`);
  results.finishedAt = new Date().toISOString();
  const rows = buildRows(results);
  results.rows = rows;
  const baselineFile = await writeOutputs(results, rows);
  return { results, rows, baselineFile };
}

let outcome;
try {
  outcome = await main();
} catch (error) {
  outcome = { error };
}
// 清理：只结束本 gate 启动且仍在运行的进程树。
const leftovers = await killLeftoverRoots();
await restoreRegistry().catch(() => {});
const regClean = await registryClean().catch(() => false);
const leaks = checkHomeLeaks();
if (outcome.error) {
  const step = outcome.error instanceof GateError ? outcome.error.step : "UNEXPECTED";
  process.stderr.write(`${outcome.error.stack ?? outcome.error}\n`);
  process.stdout.write(`PHASE7_PERF_FAIL ${step} ${String(outcome.error.message).split("\n")[0]}\n`);
  process.exitCode = 1;
} else if (leftovers.length > 0) {
  process.stdout.write(`PHASE7_PERF_FAIL CLEANUP app trees still running at the end (killed): ${leftovers.join(",")}\n`);
  process.exitCode = 1;
} else if (!regClean) {
  process.stdout.write("PHASE7_PERF_FAIL REGISTRY a guarded HKCU key still points into the gate layout\n");
  process.exitCode = 1;
} else if (leaks.length > 0) {
  process.stdout.write(`PHASE7_PERF_FAIL HOME_LEAK ${leaks.map((leak) => leak.print).join(", ")}\n`);
  process.exitCode = 1;
} else {
  const misses = outcome.rows.filter((row) => row.pass === false).map((row) => row.metric);
  const passes = outcome.rows.filter((row) => row.pass === true).length;
  const summary = { report: "Worse/.work/phase7/perfA.md", baseline: path.relative(worseRoot, outcome.baselineFile), targetsPassed: passes, targetsMissed: misses };
  if (process.env.PHASE7_PERF_STRICT === "1" && misses.length > 0) {
    process.stdout.write(`PHASE7_PERF_FAIL TARGETS ${JSON.stringify(summary)}\n`);
    process.exitCode = 1;
  } else {
    process.stdout.write(`PHASE7_PERF_OK ${JSON.stringify(summary)}\n`);
  }
}
