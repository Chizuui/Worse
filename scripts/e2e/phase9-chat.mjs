#!/usr/bin/env node
// Phase 9.2 chat and workspace gate, core half (spec docs/specs/harness/phase-9-parity.md §9.2.4).
// Builds the release binaries and runs `harness-core-gate phase9-chat` in an isolated home: organize (rename, pin,
// unread, groups), fork, rewind / edit-and-resend / retry, file undo / reapply, find in session, context blocks and
// limits, slash commands (agent + config merge), plan / usage / tool content, all four permission options, command
// search, read-only import from fixture stores (bytes and mtimes unchanged) and scheduled prompts (off by default,
// no catch-up after a restart). All agents are the fake agent. Kills only processes this gate started.
// Prints PHASE9_CHAT_CORE_OK plus a JSON summary, or PHASE9_CHAT_CORE_FAIL <reason>.
import { spawn, spawnSync } from "node:child_process";
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
const { home } = installIsolatedHome("phase9-chat");
const worse = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const targetDir = process.env.CARGO_TARGET_DIR ? path.resolve(process.env.CARGO_TARGET_DIR) : path.join(worse, "target");
const exe = (name) => path.join(targetDir, "release", process.platform === "win32" ? `${name}.exe` : name);
const GATE_TIMEOUT_MS = 300_000;
const EXPECTED_STEPS = [
  "organize",
  "fork",
  "rewind_edit_retry",
  "file_undo",
  "find",
  "context",
  "commands",
  "plan_usage_tools",
  "permissions",
  "command_search",
  "import",
  "queue_ops",
  "pickers",
  "failures",
  "applied_flag",
  "git_ops",
  "projects_tree",
  "schedules",
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
    const child = spawn(exe("harness-core-gate"), ["phase9-chat"], {
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

async function main() {
  const startedAt = Date.now();
  build();
  const run = await runGate();
  const { survivors, reused } = await classifySeen(run.agentPids);
  for (const pid of survivors) process.stderr.write(`survivor ${pid}: ${await run.agentPids.get(pid)}\n`);
  if (reused.length > 0) run.notes.push("agent pid(s) reused by unrelated processes, left alone: ${reused.join(", ")}");
  survivors.forEach(killOwnTree);

  if (run.result !== "GATE_OK" || run.code !== 0) {
    const tail = run.stderr.join("").split("\n").slice(-30).join("\n");
    if (tail.trim()) process.stderr.write(`gate stderr (tail):\n${tail}\n`);
    fail(`${run.result ?? "no result line"} (exit ${String(run.code)})`);
  }
  if (survivors.length > 0) fail(`agent processes survived the gate: ${survivors.join(", ")}`);
  const passed = new Set(run.steps.filter((s) => s.endsWith(" OK")).map((s) => s.slice(0, -3)));
  const missing = EXPECTED_STEPS.filter((s) => !passed.has(s));
  if (missing.length > 0) fail(`steps missing: ${missing.join(", ")}`);
  const summary = {
    status: "ok",
    steps: run.steps,
    notes: run.notes,
    agentProcessesSeen: run.agentPids.size,
    agentSurvivors: 0,
    elapsedMs: Date.now() - startedAt,
  };
  process.stdout.write(`PHASE9_CHAT_CORE_OK\n${JSON.stringify(summary, null, 2)}\n`);
}

main().then(
  () => process.exit(process.exitCode ?? 0),
  (error) => {
    process.stdout.write(`PHASE9_CHAT_CORE_FAIL ${error instanceof Error ? error.message : String(error)}\n`);
    process.exit(1);
  },
);
