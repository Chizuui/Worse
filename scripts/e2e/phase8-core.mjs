#!/usr/bin/env node
// Phase 8 core gate (spec docs/specs/harness/phase-8-core.md §13, round 8.2 scenarios §15.5): harness-agent-hub against scripted fake ACP agents.
// Builds the release binaries, runs crates/harness-core-gate in an isolated HARNESS_HOME, measures the core's idle
// RSS, verifies that every agent process it saw is gone and kills only processes this gate started.
// Prints PHASE8_CORE_OK plus a JSON summary, or PHASE8_CORE_FAIL <reason>.
import { spawn, spawnSync } from "node:child_process";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";
import { installIsolatedHome } from "./lib/isolatedHome.mjs";
import { classifySeen, identify, pidAlive } from "./lib/ownProcess.mjs";

// 修复说明：installIsolatedHome 把 USERPROFILE/HOME 指向一次性 home，rustup/cargo 随之把 RUSTUP_HOME/CARGO_HOME
// 解析到隔离目录，每次运行都重新下载工具链和 registry（运行变慢且产生网络流量）。在隔离前记录真实的工具链目录，
// 仅传给 cargo build；gate 进程本身仍只看到隔离环境。
const toolchainEnv = {
  CARGO_HOME: process.env.CARGO_HOME ?? path.join(os.homedir(), ".cargo"),
  RUSTUP_HOME: process.env.RUSTUP_HOME ?? path.join(os.homedir(), ".rustup"),
};
const { home } = installIsolatedHome("phase8-core");
const worse = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const targetDir = process.env.CARGO_TARGET_DIR ? path.resolve(process.env.CARGO_TARGET_DIR) : path.join(worse, "target");
const exe = (name) => path.join(targetDir, "release", process.platform === "win32" ? `${name}.exe` : name);
const GATE_TIMEOUT_MS = 240_000;
/** Total idle budget of the product (user decision: 20-50 MB with the window open); the core alone must stay far below. */
const CORE_IDLE_RSS_LIMIT_MB = 50;

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

function measureRss(pid) {
  if (process.platform === "win32") {
    const script = `$p = Get-Process -Id ${pid}; "$($p.WorkingSet64) $($p.PrivateMemorySize64)"`;
    const out = spawnSync("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", script], {
      encoding: "utf8",
      windowsHide: true,
    });
    const [ws, priv] = (out.stdout ?? "").trim().split(/\s+/).map(Number);
    return { workingSetMb: +(ws / 1048576).toFixed(1), privateMb: +(priv / 1048576).toFixed(1) };
  }
  const out = spawnSync("ps", ["-o", "rss=", "-p", String(pid)], { encoding: "utf8" });
  return { workingSetMb: +(Number(out.stdout.trim()) / 1024).toFixed(1), privateMb: null };
}

function runGate() {
  return new Promise((resolve) => {
    const harnessHome = path.join(home, ".harness");
    const child = spawn(exe("harness-core-gate"), [], {
      cwd: home,
      // 修复说明：隔离 home 位于 ZCode 仓库内（Worse/.work/tmp）。没有 ceiling 时 git 会向上找到用户仓库，
      // 回合快照（spec §15.2）会把对象写进用户的 .git，且“非仓库工作区”场景无法成立。ceiling = 隔离 home。
      env: { ...process.env, HARNESS_HOME: harnessHome, RUST_LOG: "warn", GIT_CEILING_DIRECTORIES: home },
      stdio: ["pipe", "pipe", "pipe"],
      windowsHide: true,
    });
    const state = { steps: [], agentPids: new Map(), rss: null, result: null, stderr: [], gatePid: child.pid };
    const timer = setTimeout(() => {
      state.result = "GATE_FAIL timeout";
      killOwnTree(child.pid);
    }, GATE_TIMEOUT_MS);
    let buffer = "";
    child.stdout.on("data", (chunk) => {
      buffer += chunk.toString("utf8");
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
        } else if (line.startsWith("IDLE_RSS_CHECK ")) {
          state.rss = measureRss(Number(line.slice(15)));
          child.stdin.write(`${JSON.stringify(state.rss)}\n`);
        } else if (line.startsWith("GATE_OK") || line.startsWith("GATE_FAIL")) {
          state.result ??= line;
        } else if (line) {
          process.stderr.write(`gate: ${line}\n`);
        }
      }
    });
    child.stderr.on("data", (chunk) => {
      state.stderr.push(chunk.toString("utf8"));
      if (state.stderr.length > 200) state.stderr.shift();
    });
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
  // Agents are children of the gate: any survivor is ours, so it is safe (and required) to kill it.
  const { survivors, reused } = await classifySeen(run.agentPids);
  if (reused.length > 0) run.notes.push("agent pid(s) reused by unrelated processes, left alone: ${reused.join(", ")}");
  survivors.forEach(killOwnTree);
  if (run.result !== "GATE_OK" || run.code !== 0) {
    const tail = run.stderr.join("").split("\n").slice(-30).join("\n");
    if (tail.trim()) process.stderr.write(`gate stderr (tail):\n${tail}\n`);
    fail(`${run.result ?? "no result line"} (exit ${String(run.code)})`);
  }
  if (survivors.length > 0) fail(`agent processes survived the gate: ${survivors.join(", ")}`);
  if (!run.rss) fail("idle RSS was not measured");
  if (run.rss.workingSetMb > CORE_IDLE_RSS_LIMIT_MB) {
    fail(`core idle working set ${run.rss.workingSetMb} MB exceeds ${CORE_IDLE_RSS_LIMIT_MB} MB`);
  }
  const summary = {
    status: "ok",
    steps: run.steps,
    agentProcessesSeen: run.agentPids.size,
    agentSurvivors: 0,
    coreIdle: run.rss,
    elapsedMs: Date.now() - startedAt,
  };
  process.stdout.write(`PHASE8_CORE_OK\n${JSON.stringify(summary, null, 2)}\n`);
}

main().then(
  () => process.exit(process.exitCode ?? 0),
  (error) => {
    process.stdout.write(`PHASE8_CORE_FAIL ${error instanceof Error ? error.message : String(error)}\n`);
    process.exit(1);
  },
);
