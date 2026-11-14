#!/usr/bin/env node
// Phase 3c store differential gate (spec phase-3c-store.md, C1-C8): the real ZCode Host behind
// host-node creates and migrates tasks-index.sqlite, the gate seeds a large fixture while no Host
// runs, then Hosts run ONE AT A TIME on that data dir (TS stays the only writer): TS oracle,
// `store` routed to harness-host, TS again (drift check). Every routed read must be byte-identical
// (JSON text, key order included) except traceIds TS generates randomly per read; TS writes made
// while Rust serves reads must be visible to the next Rust read; an unknown migration must make
// Rust fall back. Prints PHASE3C_STORE_DIFF_OK plus a latency table, or PHASE3C_STORE_DIFF_FAIL <case>.
import { existsSync, statSync } from "node:fs";
import { copyFile, mkdir, rm } from "node:fs/promises";
import path from "node:path";
import process from "node:process";
import { performance } from "node:perf_hooks";
import { loadModules, repoRoot, verifyArtifacts } from "./phase2/environment.mjs";
import { GateError, delay } from "./phase2/gate.mjs";
import { firstDifference, median } from "./phase3/compare.mjs";
import { openHostSession } from "./phase3/hostSession.mjs";
import { addFutureMigration, breakTaskMeta, seedFixture, TASK_COUNT } from "./phase3c-store/fixture.mjs";
import { installIsolatedHome } from "./lib/isolatedHome.mjs";

// 修复说明：Host/Agent 按 HOME/USERPROFILE 解析 ~/.zcode，曾把测试数据写进用户真实设置；
// 在任何子进程启动前切到一次性 home，并在退出时检查真实 ~/.zcode/v2 文件未被改动（HOME_LEAK）。
installIsolatedHome("phase3c-store-diff");

const WORK = path.join(repoRoot, "Worse/.work/tmp/phase3c-store/gate");
const DATA = path.join(WORK, "data");
const MISMATCH = path.join(WORK, "data-mismatch");
const BAD_META = path.join(WORK, "data-bad-meta");
const ROOTS = path.join(WORK, "roots");
const dbFile = (dataDir) => path.join(dataDir, ".zcode/v2/tasks-index.sqlite");
const exe = process.platform === "win32" ? ".exe" : "";
const WARMUP = 2;
const SAMPLES = Number(process.env.PHASE3C_STORE_SAMPLES ?? 15);

/**
 * 默认取本 gate 的独立 release 构建（cargo build --release -p harness-host --target-dir Worse/.work/tmp/phase3c-store/cargo），否则取 debug、release 中 mtime 最新者，
 * 与 phase3b-diff 相同的理由：只认 debug 时延迟表测的是未优化构建。HARNESS_RUST_HOST 可显式指定。
 */
function defaultRustHost() {
  const candidates = [
    `Worse/.work/tmp/phase3c-store/cargo/release/harness-host${exe}`,
    `Worse/target/debug/harness-host${exe}`,
    `Worse/target/release/harness-host${exe}`,
  ].map((relative) => path.join(repoRoot, relative));
  // 本 gate 自己的 release 构建优先：并行的其他构建可能更新 target/ 下的二进制。
  if (existsSync(candidates[0])) return candidates[0];
  const built = candidates.filter((candidate) => existsSync(candidate));
  if (built.length === 0) return candidates[0];
  return built.reduce((newest, candidate) => (statSync(candidate).mtimeMs > statSync(newest).mtimeMs ? candidate : newest));
}
const RUST_HOST = process.env.HARNESS_RUST_HOST ? path.resolve(process.env.HARNESS_RUST_HOST) : defaultRustHost();

class DiffFailure extends Error {
  constructor(op, reason) {
    super(reason);
    this.op = op;
  }
}

/** Routed reads, each with the exact argument list the renderer sends. */
const OPS = [
  { name: "readTree()", channel: "project", method: "readTree", args: [], rust: "store.readTree" },
  { name: "readTree(all)", channel: "project", method: "readTree", args: [{ includeDeleted: true, includeDeletedTasks: true }], rust: "store.readTree" },
  { name: "readTree(local)", channel: "project", method: "readTree", args: [{ hostId: "local", includeDeletedTasks: 1 }], rust: "store.readTree" },
  { name: "readProjects()", channel: "project", method: "readProjects", args: [], rust: "store.readProjects" },
  { name: "readProjects(deleted)", channel: "project", method: "readProjects", args: [{ includeDeleted: true }], rust: "store.readProjects" },
  { name: "task-groups.read()", channel: "task-groups", method: "read", args: [], rust: "store.readGroups" },
];
/** Must reach TS: another Host's projects (Rust declines, the TS route answers). */
const REMOTE_OP = { name: "readProjects(remote)", channel: "project", method: "readProjects", args: [{ hostId: "remote:server:fixture" }], rust: "store.readProjects" };

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

/** Replaces the per-read random traceIds (meta without traceId) after checking their format. */
function normalizeTrace(value, randomTrace, where) {
  if (!value || typeof value !== "object" || !Array.isArray(value.tasks)) return value;
  for (const task of value.tasks) {
    if (!randomTrace.has(task.taskId)) continue;
    if (!UUID.test(task.traceId)) throw new DiffFailure("TRACE_ID", `${where}: ${task.taskId} traceId ${task.traceId} is not a v4 UUID`);
    task.traceId = "<random>";
  }
  return value;
}

async function invoke(session, op) {
  const started = performance.now();
  try {
    const value = await session.client.getChannel(op.channel).call(op.method, op.args);
    return { ms: performance.now() - started, outcome: { ok: true, value } };
  } catch (error) {
    const shape = error instanceof Error ? { name: error.name, message: error.message } : { message: String(error) };
    return { ms: performance.now() - started, outcome: { ok: false, error: shape } };
  }
}

/** One Host run: first answer of every op (canonical JSON text) plus latency samples. */
async function runMode(modules, mode, dataDir, services, randomTrace, extra) {
  const env = { HARNESS_RUST_SERVICES: services, HARNESS_RUST_BRIDGE_TRACE: "1", ZCODE_DATA_BASE_DIR: dataDir, HARNESS_RUST_HOST: RUST_HOST };
  const session = await openHostSession({ modules, mode, env, step: `P3C_${mode.toUpperCase()}` });
  try {
    const results = {};
    const latency = {};
    for (const op of [...OPS, REMOTE_OP]) {
      const first = await invoke(session, op);
      if (first.outcome.ok) normalizeTrace(first.outcome.value, randomTrace, `${mode} ${op.name}`);
      results[op.name] = JSON.stringify(first.outcome);
      if (!OPS.includes(op)) continue;
      for (let i = 0; i < WARMUP; i += 1) await invoke(session, op);
      const samples = [];
      for (let i = 0; i < SAMPLES; i += 1) samples.push((await invoke(session, op)).ms);
      latency[op.name] = median(samples);
    }
    const after = extra ? await extra(session) : null;
    // trace 行来自 sidecar stderr，可能晚于 RPC 应答到达：等 Host 退出（stderr 读尽）后再取。
    await session.close();
    return { results, latency, trace: session.trace, after };
  } catch (error) {
    await session.close().catch(() => null);
    throw error;
  }
}

function compareRuns(label, left, right) {
  for (const name of Object.keys(left)) {
    if (left[name] === right[name]) continue;
    const diff = firstDifference(JSON.parse(left[name]), JSON.parse(right[name])) ?? "key order differs (same values)";
    throw new DiffFailure(label, `${name}: ${diff}`);
  }
}

function traceCounts(trace) {
  const counts = {};
  for (const entry of trace) {
    const key = `${entry.method} ${entry.outcome}`;
    counts[key] = (counts[key] ?? 0) + 1;
  }
  return counts;
}

/** C2: writes go through the TS Host of the Rust run; the very next routed read must show them. */
async function writeThenRead(session) {
  const dir = path.join(ROOTS, "written-during-test");
  await mkdir(dir, { recursive: true });
  const saved = await session.client.getChannel("project").call("saveProject", [{ name: "Written during test", roots: [dir] }]);
  const tree = await session.client.getChannel("project").call("readTree", []);
  const project = tree.projects.find((candidate) => candidate.id === saved.id);
  if (!project) throw new DiffFailure("C2_PROJECT", `project ${saved.id} written by TS is missing from the next Rust readTree`);
  if (project.primaryRootStatus !== "available") throw new DiffFailure("C2_PROJECT", `primaryRootStatus ${project.primaryRootStatus}`);
  const group = await session.client.getChannel("task-groups").call("save", [{ title: "Group written during test" }]);
  const groups = await session.client.getChannel("task-groups").call("read", []);
  if (!groups.groups.some((candidate) => candidate.id === group.id)) {
    throw new DiffFailure("C2_GROUP", `group ${group.id} written by TS is missing from the next Rust read`);
  }
  await session.client.getChannel("project").call("deleteProject", [saved.id]);
  const afterDelete = await session.client.getChannel("project").call("readProjects", []);
  if (afterDelete.some((candidate) => candidate.id === saved.id)) {
    throw new DiffFailure("C2_DELETE", `project ${saved.id} deleted by TS is still listed by the next Rust read`);
  }
  // 写后读完全走 Rust：trace 中三次读必须都是 rust（见下方对 trace 的检查）。
  return { projectId: saved.id, groupId: group.id };
}

async function copyDatabase(fromDir, toDir) {
  await mkdir(path.dirname(dbFile(toDir)), { recursive: true });
  for (const suffix of ["", "-wal", "-shm"]) {
    if (existsSync(dbFile(fromDir) + suffix)) await copyFile(dbFile(fromDir) + suffix, dbFile(toDir) + suffix);
  }
}

async function removeWork() {
  for (let attempt = 0; attempt < 6; attempt += 1) {
    try {
      await rm(WORK, { recursive: true, force: true });
      return;
    } catch {
      await delay(250 * (attempt + 1));
    }
  }
}

const round = (value) => Math.round(value * 100) / 100;

async function main() {
  verifyArtifacts();
  if (!existsSync(RUST_HOST)) throw new DiffFailure("BUILD_RUST", `harness-host missing at ${RUST_HOST} (cargo build -p harness-host)`);
  const modules = await loadModules();
  await removeWork();

  // C1: the TS Host creates and migrates the database (no Rust involved), then exits.
  const create = await openHostSession({ modules, mode: "p3c-create", env: { HARNESS_RUST_SERVICES: "none", ZCODE_DATA_BASE_DIR: DATA }, step: "P3C_CREATE" });
  await create.client.getChannel("project").call("readTree", []);
  await create.client.getChannel("task-groups").call("read", []);
  await create.close();
  const randomTrace = await seedFixture(dbFile(DATA), ROOTS);

  const ts1 = await runMode(modules, "p3c-ts1", DATA, "none", randomTrace);
  const rust = await runMode(modules, "p3c-rust", DATA, "store", randomTrace);
  const ts2 = await runMode(modules, "p3c-ts2", DATA, "none", randomTrace);
  compareRuns("C1_TS_DRIFT", ts1.results, ts2.results);
  for (const op of OPS) {
    if (JSON.parse(ts1.results[op.name]).ok !== true) throw new DiffFailure("C1_TS_OK", op.name + " failed on TS: " + ts1.results[op.name].slice(0, 300));
  }
  compareRuns("C1_DIFF", ts1.results, rust.results);
  if (ts1.trace.length > 0 || ts2.trace.length > 0) throw new DiffFailure("C1_TS_ONLY", `TS runs were routed: ${JSON.stringify(traceCounts(ts1.trace))}`);
  const perOp = WARMUP + SAMPLES + 1;
  const rustCounts = traceCounts(rust.trace);
  const expected = {};
  for (const op of OPS) expected[`${op.rust} rust`] = (expected[`${op.rust} rust`] ?? 0) + perOp;
  for (const [key, count] of Object.entries(expected)) {
    if (rustCounts[key] !== count) throw new DiffFailure("C1_SERVED_BY_RUST", `${key}: ${rustCounts[key] ?? 0} != ${count}; trace ${JSON.stringify(rustCounts)}`);
  }
  if (rustCounts["store.readProjects fallback"] !== 1) throw new DiffFailure("C4_REMOTE_HOST", `remote hostId was not handed to TS: ${JSON.stringify(rustCounts)}`);

  // C2: TS writes while Rust serves reads (Host R's TS is the only writer); then TS re-reads the result.
  const written = await runMode(modules, "p3c-write", DATA, "store", randomTrace, writeThenRead);
  const writeCounts = traceCounts(written.trace);
  for (const key of ["store.readTree rust", "store.readGroups rust", "store.readProjects rust"]) {
    if ((writeCounts[key] ?? 0) <= (rustCounts[key] ?? 0)) throw new DiffFailure("C2_SERVED_BY_RUST", `${key} not served by Rust after the writes: ${JSON.stringify(writeCounts)}`);
  }
  if (Object.keys(writeCounts).some((key) => key.endsWith(" fallback") && key !== "store.readProjects fallback")) {
    throw new DiffFailure("C2_FALLBACK", JSON.stringify(writeCounts));
  }
  const ts3 = await runMode(modules, "p3c-ts3", DATA, "none", randomTrace);
  const written2 = await runMode(modules, "p3c-rust2", DATA, "store", randomTrace);
  compareRuns("C3_DIFF_AFTER_WRITES", ts3.results, written2.results);

  // C5: a migration id this build does not know -> every store read falls back, answers stay TS-identical.
  await copyDatabase(DATA, MISMATCH);
  addFutureMigration(dbFile(MISMATCH));
  const mismatchTs = await runMode(modules, "p3c-mismatch-ts", MISMATCH, "none", randomTrace);
  const mismatchRust = await runMode(modules, "p3c-mismatch-rust", MISMATCH, "store", randomTrace);
  compareRuns("C5_DIFF", mismatchTs.results, mismatchRust.results);
  const mismatchCounts = traceCounts(mismatchRust.trace);
  if (Object.keys(mismatchCounts).some((key) => key.endsWith(" rust"))) throw new DiffFailure("C5_NO_RUST", JSON.stringify(mismatchCounts));
  const ledgerFallbacks = mismatchRust.trace.filter((entry) => entry.outcome === "fallback" && /ledger differs/.test(entry.detail ?? ""));
  if (ledgerFallbacks.length < OPS.length) throw new DiffFailure("C5_FALLBACK_REASON", `ledger fallbacks ${ledgerFallbacks.length}: ${JSON.stringify(mismatchRust.trace.slice(0, 3))}`);

  // C1b: a row TS cannot read (meta_json 'null') -> TS readTree throws; Rust must fall back so the
  // caller gets the identical TS error instead of a Rust success.
  await copyDatabase(DATA, BAD_META);
  breakTaskMeta(dbFile(BAD_META));
  const badTs = await runMode(modules, "p3c-badmeta-ts", BAD_META, "none", randomTrace);
  const badRust = await runMode(modules, "p3c-badmeta-rust", BAD_META, "store", randomTrace);
  compareRuns("C1B_ERROR_PARITY", badTs.results, badRust.results);
  if (JSON.parse(badTs.results["readTree()"]).ok !== false) throw new DiffFailure("C1B_TS_THROWS", "TS readTree did not fail on meta_json null");
  const badCounts = traceCounts(badRust.trace);
  if ((badCounts["store.readTree rust"] ?? 0) !== 0 || !badRust.trace.some((entry) => /meta_json is null/.test(entry.detail ?? ""))) {
    throw new DiffFailure("C1B_FALLBACK", JSON.stringify(badCounts));
  }

  const sizes = Object.fromEntries(OPS.map((op) => [op.name, rust.results[op.name].length]));
  const latency = OPS.map((op) => ({
    op: op.name,
    resultBytes: sizes[op.name],
    tsMs: round(median([ts1.latency[op.name], ts2.latency[op.name]])),
    rustMs: round(rust.latency[op.name]),
    speedup: round(median([ts1.latency[op.name], ts2.latency[op.name]]) / rust.latency[op.name]),
  }));
  const summary = {
    rustHost: path.relative(repoRoot, RUST_HOST),
    fixture: { tasks: TASK_COUNT, randomTraceTasks: randomTrace.size, listedTasks: JSON.parse(rust.results["readTree()"]).value.tasks.length },
    samplesPerOp: SAMPLES,
    identical: OPS.map((op) => op.name).concat(REMOTE_OP.name),
    rustTrace: rustCounts,
    writeVisibility: { ...written.after, trace: writeCounts },
    schemaMismatch: { trace: mismatchCounts },
    badMetaErrorParity: { tsError: JSON.parse(badTs.results["readTree()"]).error, trace: badCounts },
    latency,
  };
  process.stdout.write(`PHASE3C_STORE_DIFF_OK\n${JSON.stringify(summary, null, 2)}\n`);
  for (const row of latency) process.stdout.write(`${row.op.padEnd(24)} ts ${String(row.tsMs).padStart(8)} ms  rust ${String(row.rustMs).padStart(8)} ms  x${row.speedup}  (${row.resultBytes} B)\n`);
}

main()
  .then(async () => {
    if (!process.env.PHASE3C_STORE_KEEP) await removeWork();
    process.exit(0);
  })
  .catch(async (error) => {
    const op = error instanceof DiffFailure ? error.op : error instanceof GateError ? error.step : "UNEXPECTED";
    process.stderr.write(`PHASE3C_STORE_DIFF_FAIL ${op}\nreason: ${error instanceof Error ? error.stack ?? error.message : String(error)}\n`);
    process.exit(1);
  });
