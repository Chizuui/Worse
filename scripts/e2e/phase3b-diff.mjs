#!/usr/bin/env node
// Phase 3b differential gate (spec phase-3b-watch-pty.md, A1-A6, B1-B9): the real ZCode Host behind
// host-node runs the same watcher and terminal scenarios with TS only (oracle) and with `watch,pty`
// routed to harness-host; observations are compared, Rust service is proven from the bridge trace,
// host loss and missing-binary fallback are exercised, and no shell / harness-host may survive.
// Prints PHASE3B_DIFF_OK plus a latency table, or PHASE3B_DIFF_FAIL <case>.
import { existsSync, statSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { loadModules, paths, removeRunDir, repoRoot, verifyArtifacts } from "./phase2/environment.mjs";
import { GateError } from "./phase2/gate.mjs";
import { firstDifference, median } from "./phase3/compare.mjs";
import { openHostSession } from "./phase3/hostSession.mjs";
import { runExplicitShellCheck } from "./phase3b/directPty.mjs";
import { runInterleavedEcho } from "./phase3b/echoInterleaved.mjs";
import { createTreeTracker, descendants, killPid, waitForExit } from "./phase3b/processes.mjs";
import { ScenarioError, runHostLossScenario, runPtyScenarios } from "./phase3b/ptyScenarios.mjs";
import { runWatchScenarios } from "./phase3b/watchScenarios.mjs";
import { installIsolatedHome } from "./lib/isolatedHome.mjs";

// 修复说明：Host/Agent 按 HOME/USERPROFILE 解析 ~/.zcode，曾把测试数据写进用户真实设置；
// 在任何子进程启动前切到一次性 home，并在退出时检查真实 ~/.zcode/v2 文件未被改动（HOME_LEAK）。
installIsolatedHome("phase3b-diff");

const FIXTURE_ROOT = path.join(repoRoot, "Worse/.work/tmp/phase3b-fixtures");
const exe = process.platform === "win32" ? ".exe" : "";
/**
 * 默认取最近一次构建出的 harness-host（debug、target/release、.work/tmp/cargo-release 三者中 mtime 最新的）：
 * 只认 debug 时，延迟表测的是未优化构建（见 Worse/.work/fixes/git-perf.md 的"测量陷阱"）。
 * 实际使用的路径写在输出的 rustHost 字段里；HARNESS_RUST_HOST 仍可显式指定。
 */
function defaultRustHost() {
  const candidates = [
    `Worse/target/debug/harness-host${exe}`,
    `Worse/target/release/harness-host${exe}`,
    `Worse/.work/tmp/cargo-release/release/harness-host${exe}`,
  ].map((relative) => path.join(repoRoot, relative));
  const built = candidates.filter((candidate) => existsSync(candidate));
  if (built.length === 0) return candidates[0];
  return built.reduce((newest, candidate) => (statSync(candidate).mtimeMs > statSync(newest).mtimeMs ? candidate : newest));
}
const RUST_HOST = process.env.HARNESS_RUST_HOST ? path.resolve(process.env.HARNESS_RUST_HOST) : defaultRustHost();
const RUST_HOST_IMAGE = `harness-host${exe}`;

class DiffFailure extends Error {
  constructor(op, reason) {
    super(reason);
    this.op = op;
  }
}

function round(value) {
  return value === null || value === undefined ? null : Math.round(value * 100) / 100;
}

/** Bounded wait for host-node teardown; generous because node-pty ends consoles asynchronously. */
const ORPHAN_WAIT_MS = 10_000;

/** O1: after host-node exited, nothing it (transitively) started may survive. */
async function assertNoOrphans(label, tracker) {
  const { left, waitedMs, aliveAtStart } = await waitForExit(tracker.rows(), ORPHAN_WAIT_MS);
  if (left.length > 0) {
    // 只结束本次测试启动的进程：身份（pid + 名称 + 创建时间）都匹配，且创建时间不早于 host-node。
    for (const row of left) killPid(row.pid);
    throw new DiffFailure(`ORPHANS_${label.toUpperCase()}`, `left running after ${Math.round(waitedMs)}ms: ${left.map((row) => `${row.name}#${row.pid}`).join(", ")}`);
  }
  // 记录 host-node 退出后仍需等待的进程与时长，作为异步收尾的证据。
  return { tracked: tracker.rows().length, aliveAfterClose: aliveAtStart, waitedMs: round(waitedMs), staleLinksSkipped: tracker.staleLinks() };
}

async function runMode(modules, mode, env, { hostLoss = false } = {}) {
  const session = await openHostSession({ modules, mode, env, step: `P3B_${mode.toUpperCase()}` });
  const sidecarPid = session.sidecar.child.pid;
  const tracker = createTreeTracker(sidecarPid);
  let result;
  try {
    const cwd = path.join(FIXTURE_ROOT, `cwd-${mode}`);
    const { mkdir } = await import("node:fs/promises");
    await mkdir(cwd, { recursive: true });
    const watch = await runWatchScenarios(session.client, path.join(FIXTURE_ROOT, `watch-${mode}`), mode);
    const pty = await runPtyScenarios(session, { cwd, mode, onLive: tracker.snapshot });
    let loss = null;
    if (hostLoss) {
      tracker.snapshot();
      const killRustHost = () => {
        const hosts = descendants(sidecarPid).filter((row) => row.name.toLowerCase() === RUST_HOST_IMAGE);
        if (hosts.length === 0) throw new ScenarioError("B9", "no harness-host child found to kill");
        for (const row of hosts) killPid(row.pid);
      };
      loss = await runHostLossScenario(session, { cwd, killRustHost });
    }
    await new Promise((resolve) => setTimeout(resolve, 300));
    // 关闭前再记一次进程树：之后拉起的进程（如重启的 harness-host）也要被检查。
    tracker.snapshot();
    result = { watch, pty, loss, trace: [...session.trace] };
  } finally {
    await session.close();
  }
  const orphanCheck = await assertNoOrphans(mode, tracker);
  return { ...result, liveProcesses: tracker.names(), orphanCheck };
}

function compare(label, oracle, candidate) {
  for (const key of Object.keys(oracle)) {
    const diff = firstDifference(oracle[key], candidate[key]);
    if (diff) throw new DiffFailure(key, `[${label}] ${key} ${diff}`);
  }
}

/** Absolute expectations on the TS oracle: the scenarios themselves must be meaningful. */
function assertOracle(ts) {
  const w = ts.watch.obs;
  const p = ts.pty.obs;
  const checks = [
    ["A1", w.A1.create.length > 0 && w.A1.delete.length > 0, "TS reported no create/delete event"],
    ["A3", w.A3.nodeModules && w.A3.git, "TS did not report node_modules/.git changes"],
    ["A4", w.A4.events === 0, "events after unwatch"],
    ["A5", w.A5.ok === false, "watching a missing path succeeded"],
    ["B1", p.B1.echo === true, "no echo"],
    ["B6", p.B6.exitCode === 7, `exit code ${p.B6.exitCode}`],
    ["B5", p.B5.exitEvent === false, "dispose fired an exit event"],
    ["B7", p.B7.aSeen && p.B7.bSeen && !p.B7.crossed, "concurrent sessions crossed or were silent"],
  ];
  for (const [op, ok, reason] of checks) if (!ok) throw new DiffFailure(op, `[ts oracle] ${reason}`);
}

/** Every watch/pty entry point must have been served by Rust (no silent fallback). */
function assertServedByRust(trace) {
  const counts = {};
  for (const entry of trace) {
    if (!/^(watch|pty)\./.test(entry.method)) continue;
    if (entry.outcome === "fallback") throw new DiffFailure(entry.method, `fell back to TS: ${entry.detail ?? ""}`);
    counts[`${entry.method} ${entry.outcome}`] = (counts[`${entry.method} ${entry.outcome}`] ?? 0) + 1;
  }
  for (const method of ["watch.subscribe", "watch.unsubscribe", "pty.spawn", "pty.write", "pty.resize", "pty.kill"]) {
    if (!counts[`${method} rust`]) throw new DiffFailure(method, "never served by Rust");
  }
  return counts;
}

function latencyRow(metric, tsSamples, rustSamples) {
  const ts = tsSamples.length > 0 ? median(tsSamples) : null;
  const rust = rustSamples.length > 0 ? median(rustSamples) : null;
  return { metric, tsMs: round(ts), rustMs: round(rust), speedup: ts && rust ? round(ts / rust) : null, samples: `${tsSamples.length}/${rustSamples.length}` };
}

async function runFallback(modules) {
  // 二进制缺失时 watch/pty 必须完整回退到 TS，且行为可用。
  const missing = path.join(paths.runDir, `missing-harness-host${exe}`);
  const session = await openHostSession({
    modules,
    mode: "fallback",
    env: { HARNESS_RUST_BRIDGE_TRACE: "1", HARNESS_RUST_SERVICES: "watch,pty", HARNESS_RUST_HOST: missing },
    step: "P3B_FALLBACK",
  });
  try {
    const { openTerminal, waitQuiet, echoRoundtrip } = await import("./phase3b/terminal.mjs");
    const term = await openTerminal(session.client, { cols: 80, rows: 24 });
    await waitQuiet(term);
    const echo = await echoRoundtrip(term, "FB", "OK");
    term.unsubscribe();
    await term.dispose();
    const watched = await session.client.getChannel("file-watcher").call("watch", [{ path: FIXTURE_ROOT }]);
    await session.client.getChannel("file-watcher").call("unwatch", [{ id: watched.id }]);
    await new Promise((resolve) => setTimeout(resolve, 300));
    if (!echo.seen) throw new DiffFailure("FALLBACK", "terminal did not work on the TS fallback");
    if (String(term.id).startsWith("rpty-") || String(watched.id).startsWith("rw-")) throw new DiffFailure("FALLBACK", "ids came from the bridge");
    if (session.trace.some((entry) => entry.outcome !== "fallback")) throw new DiffFailure("FALLBACK", "a missing binary must route nothing to Rust");
    return { terminalId: term.id, watchId: watched.id, fallbacks: session.trace.length };
  } finally {
    await session.close();
  }
}

async function main() {
  verifyArtifacts();
  if (!existsSync(RUST_HOST)) throw new DiffFailure("BUILD_RUST_HOST", `${RUST_HOST} is missing (cargo build -p harness-host)`);
  const modules = await loadModules();
  await removeRunDir();
  const common = { HARNESS_RUST_BRIDGE_TRACE: "1" };

  const ts = await runMode(modules, "ts", { ...common, HARNESS_RUST_SERVICES: "none" });
  if (ts.trace.length > 0) throw new DiffFailure("ROUTING_OFF", `flags off but ${ts.trace.length} calls were routed`);
  assertOracle(ts);
  const rust = await runMode(modules, "rust", { ...common, HARNESS_RUST_SERVICES: "search,git,watch,pty", HARNESS_RUST_HOST: RUST_HOST }, { hostLoss: true });
  compare("rust", ts.watch.obs, rust.watch.obs);
  compare("rust", ts.pty.obs, rust.pty.obs);
  const preLossTrace = rust.trace;
  const served = assertServedByRust(preLossTrace.filter((entry) => !(entry.outcome === "fallback" && /harness-host unavailable/.test(entry.detail ?? ""))));
  if (!rust.watch.ids.every((id) => id.startsWith("rw-"))) throw new DiffFailure("A1", "watch ids were not bridge ids");

  const explicit = await runExplicitShellCheck(RUST_HOST, FIXTURE_ROOT);
  const explicitDiff = firstDifference({ marker: explicit.oracle.marker, exitCode: explicit.oracle.exitCode }, { marker: explicit.rust.marker, exitCode: explicit.rust.exitCode });
  if (explicitDiff) throw new DiffFailure("B2", `explicit ${explicit.shell}: ${explicitDiff}`);
  if (explicit.rust.exitCode !== 3 || !explicit.rust.marker) throw new DiffFailure("B2", `explicit ${explicit.shell}: exit ${explicit.rust.exitCode}, marker ${explicit.rust.marker}`);

  const fallback = await runFallback(modules);

  const echoReps = Number(process.env.PHASE3B_ECHO_REPS ?? 20);
  const echo = await runInterleavedEcho(modules, {
    cwd: path.join(FIXTURE_ROOT, "cwd-echo"),
    reps: echoReps,
    tsEnv: { ...common, HARNESS_RUST_SERVICES: "none" },
    rustEnv: { ...common, HARNESS_RUST_SERVICES: "pty", HARNESS_RUST_HOST: RUST_HOST },
  });
  const echoOrphanCheck = { ts: await assertNoOrphans("echo_ts", echo.trackers.ts), rust: await assertNoOrphans("echo_rust", echo.trackers.rust) };

  const latency = [
    latencyRow("watch event latency (op -> first event)", ts.watch.latency, rust.watch.latency),
    latencyRow("terminal.create (spawn, via RPC)", ts.pty.perf.spawnMs, rust.pty.perf.spawnMs),
    latencyRow("echo roundtrip (write -> marker), interleaved", echo.ts, echo.rust),
    latencyRow("echo roundtrip, sequential B1", ts.pty.perf.echoMs, rust.pty.perf.echoMs),
    latencyRow("echo write -> first output chunk (B1)", ts.pty.perf.echoFirstDataMs, rust.pty.perf.echoFirstDataMs),
    latencyRow("echo write -> write ack (B1)", ts.pty.perf.echoAckMs, rust.pty.perf.echoAckMs),
    latencyRow(`explicit ${explicit.shell} spawn (node-pty vs pty.spawn)`, [explicit.oracle.spawnMs], [explicit.rust.spawnMs]),
  ];
  const summary = {
    status: "ok",
    rustHost: path.relative(repoRoot, RUST_HOST),
    observations: { watch: rust.watch.obs, pty: rust.pty.obs },
    hostLoss: rust.loss,
    explicitShell: { shell: explicit.shell, oracle: { marker: explicit.oracle.marker, exitCode: explicit.oracle.exitCode }, rust: { marker: explicit.rust.marker, exitCode: explicit.rust.exitCode } },
    fallback,
    routed: served,
    liveProcesses: { ts: ts.liveProcesses, rust: rust.liveProcesses },
    orphanCheck: { ts: ts.orphanCheck, rust: rust.orphanCheck, echoTs: echoOrphanCheck.ts, echoRust: echoOrphanCheck.rust },
    latency,
  };
  process.stdout.write(`PHASE3B_DIFF_OK\n${JSON.stringify(summary, null, 2)}\n`);
  process.stdout.write("\n| metric | TS ms | Rust ms | speedup | samples ts/rust |\n| --- | --- | --- | --- | --- |\n");
  for (const row of latency) process.stdout.write(`| ${row.metric} | ${row.tsMs} | ${row.rustMs} | ${row.speedup} | ${row.samples} |\n`);
}

main()
  .then(async () => {
    await removeRunDir();
    process.exit(0);
  })
  .catch(async (error) => {
    await removeRunDir();
    const op = error instanceof DiffFailure || error instanceof ScenarioError ? error.op : error instanceof GateError ? error.step : "UNEXPECTED";
    process.stderr.write(`PHASE3B_DIFF_FAIL ${op}\nreason: ${error instanceof Error ? error.stack ?? error.message : String(error)}\n`);
    process.exit(1);
  });
