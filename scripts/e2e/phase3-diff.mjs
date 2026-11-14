#!/usr/bin/env node
// Phase 3a differential gate (spec A1-A6): the real ZCode Host behind host-node is run three
// times on one fixture corpus — TS only (oracle), Rust routing on, and Rust on with a missing
// binary (fallback) — and every routed RPC result is compared. Harness-native methods are checked
// directly against Node oracles. Prints PHASE3_DIFF_OK plus a JSON perf table, or exits non-zero
// with PHASE3_DIFF_FAIL <op>.
import { existsSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { loadModules, paths, removeRunDir, repoRoot, verifyArtifacts } from "./phase2/environment.mjs";
import { GateError } from "./phase2/gate.mjs";
import { firstDifference, median, normalizeOutcome } from "./phase3/compare.mjs";
import { buildFixtures } from "./phase3/fixtures.mjs";
import { openHostSession } from "./phase3/hostSession.mjs";
import { NativeCheckError, runNativeChecks } from "./phase3/nativeChecks.mjs";
import { buildOperations } from "./phase3/operations.mjs";
import { installIsolatedHome } from "./lib/isolatedHome.mjs";

// 修复说明：Host/Agent 按 HOME/USERPROFILE 解析 ~/.zcode，曾把测试数据写进用户真实设置；
// 在任何子进程启动前切到一次性 home，并在退出时检查真实 ~/.zcode/v2 文件未被改动（HOME_LEAK）。
installIsolatedHome("phase3-diff");

const REPS = Number(process.env.PHASE3_DIFF_REPS ?? 5);
/** Services under test (default all); lets the gate run while a service is still being built. */
const SERVICES = (process.env.PHASE3_DIFF_SERVICES ?? "fs,search,git").split(",").map((entry) => entry.trim()).filter(Boolean);
const FIXTURE_ROOT = path.join(repoRoot, "Worse/.work/tmp/phase3-fixtures");
const exe = process.platform === "win32" ? ".exe" : "";
const RUST_HOST = process.env.HARNESS_RUST_HOST
  ? path.resolve(process.env.HARNESS_RUST_HOST)
  : path.join(repoRoot, `Worse/target/debug/harness-host${exe}`);

class DiffFailure extends Error {
  constructor(op, reason) {
    super(reason);
    this.op = op;
  }
}

/** Runs every op REPS times; keeps the first outcome (cold caches) and the median latency. */
async function runAll(session, ops) {
  const results = new Map();
  for (const op of ops) {
    const samples = [];
    let first;
    for (let rep = 0; rep < REPS; rep += 1) {
      const startedAt = performance.now();
      const outcome = await session.call(op.channel, op.method, op.params);
      samples.push(performance.now() - startedAt);
      if (rep === 0) first = outcome;
    }
    results.set(op.id, { outcome: normalizeOutcome(first), ms: median(samples) });
  }
  return results;
}

async function runMode(modules, mode, env, ops) {
  const session = await openHostSession({ modules, mode, env, step: `P3_${mode.toUpperCase()}` });
  try {
    const results = await runAll(session, ops);
    // Trace lines are written after each reply; give stderr a moment to drain.
    await new Promise((resolve) => setTimeout(resolve, 300));
    return { results, trace: [...session.trace] };
  } finally {
    await session.close();
  }
}

function compareModes(ops, oracle, candidate, label) {
  for (const op of ops) {
    const diff = firstDifference(oracle.results.get(op.id).outcome, candidate.results.get(op.id).outcome);
    if (diff) throw new DiffFailure(op.op, `[${label}] ${op.id} ${diff}`);
  }
}

/** Every routed call must have been answered by Rust (no silent fallback hides a mismatch). */
function assertServedByRust(ops, trace) {
  const expected = new Map();
  for (const op of ops) {
    const method = `${op.channel}.${op.method}`;
    expected.set(method, (expected.get(method) ?? 0) + REPS);
  }
  const served = new Map();
  for (const entry of trace) {
    if (entry.outcome === "fallback") throw new DiffFailure(entry.method, `fell back to TS: ${entry.detail ?? ""}`);
    served.set(entry.method, (served.get(entry.method) ?? 0) + 1);
  }
  for (const [method, count] of expected) {
    const got = served.get(method) ?? 0;
    if (got !== count) throw new DiffFailure(method, `expected ${count} Rust-served calls, trace shows ${got}`);
  }
}

function perfTable(ops, ts, rust) {
  const byOp = new Map();
  for (const op of ops) {
    const row = byOp.get(op.op) ?? { op: op.op, service: op.service, cases: 0, tsMs: 0, rustMs: 0 };
    row.cases += 1;
    row.tsMs += ts.results.get(op.id).ms;
    row.rustMs += rust.results.get(op.id).ms;
    byOp.set(op.op, row);
  }
  return [...byOp.values()].map((row) => formatRow(row));
}

function formatRow(row) {
  const round = (value) => Math.round(value * 100) / 100;
  return { ...row, tsMs: round(row.tsMs), rustMs: round(row.rustMs), speedup: round(row.tsMs / Math.max(row.rustMs, 0.001)) };
}

/** The TS oracle's packed file index per search root, used by the native `search.text` check. */
function collectPackedIndexes(ops, ts, fx) {
  return new Map(
    [fx.ws, fx.repo, fx.bulk].map((root) => {
      const op = ops.find((candidate) => candidate.method === "listWorkspaceFilesRange" && candidate.params.rootPath === root && candidate.params.offset === 0);
      return [root, ts.results.get(op.id).outcome.value];
    }),
  );
}

async function main() {
  verifyArtifacts();
  if (!existsSync(RUST_HOST)) throw new DiffFailure("BUILD_RUST_HOST", `${RUST_HOST} is missing (cargo build -p harness-host)`);
  const modules = await loadModules();
  await removeRunDir();
  const gitEnv = { ...process.env, GIT_CEILING_DIRECTORIES: FIXTURE_ROOT };
  const fx = await buildFixtures(FIXTURE_ROOT, gitEnv);
  const ops = buildOperations(fx).filter((op) => SERVICES.includes(op.service));
  const common = { GIT_CEILING_DIRECTORIES: FIXTURE_ROOT, HARNESS_RUST_BRIDGE_TRACE: "1" };

  const ts = await runMode(modules, "ts", { ...common, HARNESS_RUST_SERVICES: "none" }, ops);
  if (ts.trace.length > 0) throw new DiffFailure("ROUTING_OFF", `flags off but ${ts.trace.length} calls were routed`);
  const rust = await runMode(modules, "rust", { ...common, HARNESS_RUST_SERVICES: SERVICES.join(","), HARNESS_RUST_HOST: RUST_HOST }, ops);
  compareModes(ops, ts, rust, "rust");
  assertServedByRust(ops, rust.trace);

  const fallbackOps = ops.filter((op) => op.service === "fs").slice(0, 12);
  const missing = path.join(paths.runDir, `missing-harness-host${exe}`);
  const fallback = await runMode(modules, "fallback", { ...common, HARNESS_RUST_SERVICES: "all", HARNESS_RUST_HOST: missing }, fallbackOps);
  compareModes(fallbackOps, ts, fallback, "fallback");
  if (fallback.trace.some((entry) => entry.outcome !== "fallback")) {
    throw new DiffFailure("FALLBACK", "a missing binary must route nothing to Rust");
  }

  // 修复说明：PHASE3_DIFF_SERVICES 只选 git（性能剖析）时没有 listWorkspaceFilesRange 用例，
  // 原代码读取 undefined.id 直接以 UNEXPECTED 失败。原生检查依赖 fs+search 用例，子集运行时跳过。
  const runsNative = SERVICES.includes("fs") && SERVICES.includes("search");
  const native = runsNative
    ? await runNativeChecks({ binaryPath: RUST_HOST, fx, packedIndexes: collectPackedIndexes(ops, ts, fx), reps: REPS })
    : { rows: [], server: null, cancel: "skipped (service subset)" };

  const perf = [...perfTable(ops, ts, rust), ...native.rows.map((row) => formatRow({ ...row, service: row.op.split(".")[0] }))];
  const summary = {
    status: "ok",
    services: SERVICES,
    reps: REPS,
    rustHost: path.relative(repoRoot, RUST_HOST),
    server: native.server,
    routedCalls: rust.trace.length,
    outcomes: rust.trace.reduce((acc, entry) => ({ ...acc, [entry.outcome]: (acc[entry.outcome] ?? 0) + 1 }), {}),
    cases: ops.length,
    fallbackCases: fallbackOps.length,
    links: fx.links,
    cancel: native.cancel,
    perf,
  };
  process.stdout.write(`PHASE3_DIFF_OK\n${JSON.stringify(summary, null, 2)}\n`);
}

main()
  .then(async () => {
    await removeRunDir();
    process.exit(0);
  })
  .catch(async (error) => {
    await removeRunDir();
    const op =
      error instanceof DiffFailure || error instanceof NativeCheckError ? error.op : error instanceof GateError ? error.step : "UNEXPECTED";
    process.stderr.write(`PHASE3_DIFF_FAIL ${op}\nreason: ${error instanceof Error ? error.message : String(error)}\n`);
    process.exit(1);
  });
