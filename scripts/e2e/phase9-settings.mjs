#!/usr/bin/env node
// Phase 9.1 settings gate, core half (spec docs/specs/harness/phase-9-parity.md §9.1.7).
// Builds the release binaries and runs `harness-core-gate phase9-settings` in an isolated home seeded with dummy CLI
// configs and dummy secrets: settings store round trip, every agent-config concept cell, conflict detection, masked
// diffs, auth presence, the MCP overlay on session/new, mode/model/option switching and data-dir relocation.
// Afterwards every file outside the seeded fixture dirs, the fake agent log and the gate output are scanned for the
// fixture secrets. Kills only processes this gate started.
// Prints PHASE9_SETTINGS_CORE_OK plus a JSON summary, or PHASE9_SETTINGS_CORE_FAIL <reason>.
import { spawn, spawnSync } from "node:child_process";
import { readdirSync, readFileSync, statSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";
import { installIsolatedHome } from "./lib/isolatedHome.mjs";
import { classifySeen, identify, pidAlive } from "./lib/ownProcess.mjs";

// Same reason as phase8-core.mjs: keep the real toolchain dirs for cargo only (captured before isolation).
const toolchainEnv = {
  CARGO_HOME: process.env.CARGO_HOME ?? path.join(os.homedir(), ".cargo"),
  RUSTUP_HOME: process.env.RUSTUP_HOME ?? path.join(os.homedir(), ".rustup"),
};
const { home, env: isolated } = installIsolatedHome("phase9-settings");
const worse = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const targetDir = process.env.CARGO_TARGET_DIR ? path.resolve(process.env.CARGO_TARGET_DIR) : path.join(worse, "target");
const exe = (name) => path.join(targetDir, "release", process.platform === "win32" ? `${name}.exe` : name);
const GATE_TIMEOUT_MS = 300_000;
const EXPECTED_STEPS = [
  "settings_roundtrip",
  "concept_roundtrip",
  "conflict",
  "diff_masking",
  "auth_presence",
  "overlay_session_new",
  "modes_models",
  "relocation",
  "agents_overview",
  "memory_admin",
  "projects",
  "archive",
];
/** Must match `phase9::SECRETS` in crates/harness-core-gate/src/phase9.rs. */
const SECRETS = [
  "sk-ant-FIXTURE-cred-0001",
  "sk-codex-FIXTURE-auth-0002",
  "ya29.FIXTURE-oauth-0003",
  "sk-ant-FIXTURE-primary-0004",
  "FIXTURE-bearer-0005",
  "FIXTURE-envsecret-0006",
];

class GateFailure extends Error {}
const fail = (reason) => {
  throw new GateFailure(reason);
};

function build() {
  const args = ["build", "--release", "-p", "harness-core-gate", "-p", "harness-fake-agent", "-p", "harness-agent-hub", "--bins"];
  const result = spawnSync("cargo", args, {
    cwd: worse,
    env: { ...process.env, ...toolchainEnv },
    stdio: ["ignore", "inherit", "inherit"],
    windowsHide: true,
  });
  if (result.status !== 0) fail(`cargo ${args.join(" ")} exited ${String(result.status)}`);
}

function killOwnTree(pid) {
  if (!pidAlive(pid)) return;
  if (process.platform === "win32") {
    spawnSync("taskkill", ["/T", "/F", "/PID", String(pid)], { stdio: "ignore", windowsHide: true });
  } else {
    try {
      process.kill(pid, "SIGKILL");
    } catch {
      // already gone
    }
  }
}

function runGate() {
  return new Promise((resolve) => {
    const child = spawn(exe("harness-core-gate"), ["phase9-settings"], {
      cwd: home,
      // 隔离 home 位于用户仓库内：设 ceiling，避免 git 向上找到 ZCode 仓库（同 phase8-core.mjs）。
      env: { ...process.env, HARNESS_HOME: path.join(home, ".harness"), RUST_LOG: "warn", GIT_CEILING_DIRECTORIES: home },
      stdio: ["ignore", "pipe", "pipe"],
      windowsHide: true,
    });
    const state = { steps: [], notes: [], agentPids: new Map(), result: null, stdout: [], stderr: [] };
    const timer = setTimeout(() => {
      state.result = "GATE_FAIL timeout";
      killOwnTree(child.pid);
    }, GATE_TIMEOUT_MS);
    let buffer = "";
    child.stdout.on("data", (chunk) => {
      const text = chunk.toString("utf8");
      state.stdout.push(text);
      buffer += text;
      let index;
      while ((index = buffer.indexOf("\n")) >= 0) {
        const line = buffer.slice(0, index).trim();
        buffer = buffer.slice(index + 1);
        if (line.startsWith("STEP ")) {
          state.steps.push(line.slice(5));
          process.stderr.write(`${line}\n`);
        } else if (line.startsWith("AGENT_PID ")) {
          // 修复说明：Windows 复用 PID 很快，只凭 PID 仍存活判残留会误报，并可能 taskkill 无关进程；首次看到时记下身份。
          const pid = Number(line.slice(10));
          if (!state.agentPids.has(pid)) state.agentPids.set(pid, identify(pid));
        } else if (line.startsWith("NOTE ")) {
          state.notes.push(line.slice(5));
        } else if (line.startsWith("GATE_OK") || line.startsWith("GATE_FAIL")) {
          state.result ??= line;
        } else if (line) {
          process.stderr.write(`gate: ${line}\n`);
        }
      }
    });
    child.stderr.on("data", (chunk) => state.stderr.push(chunk.toString("utf8")));
    child.on("exit", (code) => {
      clearTimeout(timer);
      resolve({ ...state, code });
    });
  });
}

/** Every file under `dir`, skipping the seeded fixture dirs (which legitimately hold the dummy secrets). */
function* walk(dir, skip) {
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return;
  }
  for (const entry of entries) {
    const full = path.join(dir, entry.name);
    if (skip.some((s) => path.resolve(full).toLowerCase() === path.resolve(s).toLowerCase())) continue;
    if (entry.isDirectory()) yield* walk(full, skip);
    else if (entry.isFile()) yield full;
  }
}

/** Files (relative to home) or streams that contain a fixture secret. Contents are never printed. */
function scanSecrets(run) {
  const skip = [isolated.CLAUDE_CONFIG_DIR, isolated.CODEX_HOME, path.join(home, ".gemini")];
  const leaks = [];
  for (const file of walk(home, skip)) {
    if (statSync(file).size > 64 * 1024 * 1024) continue;
    const text = readFileSync(file).toString("latin1");
    if (SECRETS.some((s) => text.includes(s))) leaks.push(path.relative(home, file));
  }
  for (const [name, chunks] of [["gate stdout", run.stdout], ["gate stderr", run.stderr]]) {
    const text = chunks.join("");
    if (SECRETS.some((s) => text.includes(s))) leaks.push(name);
  }
  return leaks;
}

async function main() {
  const startedAt = Date.now();
  build();
  const run = await runGate();
  const { survivors, reused } = await classifySeen(run.agentPids);
  if (reused.length > 0) run.notes.push("agent pid(s) reused by unrelated processes, left alone: ${reused.join(", ")}");
  survivors.forEach(killOwnTree);
  if (run.result !== "GATE_OK" || run.code !== 0) {
    const tail = run.stderr.join("").split("\n").slice(-30).join("\n");
    // 失败时只打印尾部，先确认不含 fixture 秘密再输出。
    if (tail.trim() && !SECRETS.some((s) => tail.includes(s))) process.stderr.write(`gate stderr (tail):\n${tail}\n`);
    fail(`${run.result ?? "no result line"} (exit ${String(run.code)})`);
  }
  if (survivors.length > 0) fail(`agent processes survived the gate: ${survivors.join(", ")}`);
  const passed = new Set(run.steps.filter((s) => s.endsWith(" OK")).map((s) => s.slice(0, -3)));
  const missing = EXPECTED_STEPS.filter((s) => !passed.has(s));
  if (missing.length > 0) fail(`steps missing: ${missing.join(", ")}`);
  const leaks = scanSecrets(run);
  if (leaks.length > 0) fail(`fixture secrets found outside the fixtures: ${leaks.join(", ")}`);
  const summary = {
    status: "ok",
    steps: run.steps,
    notes: run.notes,
    agentProcessesSeen: run.agentPids.size,
    agentSurvivors: 0,
    secretScan: "clean",
    elapsedMs: Date.now() - startedAt,
  };
  process.stdout.write(`PHASE9_SETTINGS_CORE_OK\n${JSON.stringify(summary, null, 2)}\n`);
}

main().then(
  () => process.exit(process.exitCode ?? 0),
  (error) => {
    process.stdout.write(`PHASE9_SETTINGS_CORE_FAIL ${error instanceof Error ? error.message : String(error)}\n`);
    process.exit(1);
  },
);
