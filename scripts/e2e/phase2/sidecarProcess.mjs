import { spawn } from "node:child_process";
import { isolatedHomeEnv } from "../lib/isolatedHome.mjs";
import { GateError } from "./gate.mjs";

const STDERR_TAIL_BYTES = 65_536;

/**
 * Spawns the launcher and exposes its NDJSON stdout as awaitable lines.
 *
 * Any non-NDJSON stdout line or `fatal` line aborts every pending wait.
 * Requires the gate to have called `installIsolatedHome` (throws otherwise).
 */
export function startSidecar({ launcher, cwd, env }) {
  // 修复说明：Host 的 setting.json 按 ZCODE_DESKTOP_HOME_DIR/HOME/USERPROFILE 解析，不认
  // ZCODE_DATA_BASE_DIR；未隔离时 gate 会写进用户真实 ~/.zcode。这里强制要求已安装隔离 home，
  // 且家目录类变量最后合并、调用方无法覆盖；调用方仍可指定自己的 ZCODE_DATA_BASE_DIR。
  const { ZCODE_DATA_BASE_DIR: defaultDataBaseDir, ...homeEnv } = isolatedHomeEnv();
  const child = spawn(process.execPath, ["--no-warnings", launcher], {
    cwd,
    env: { ...process.env, ZCODE_DATA_BASE_DIR: defaultDataBaseDir, ...env, ...homeEnv },
    stdio: ["pipe", "pipe", "pipe"],
    windowsHide: true,
  });

  const lines = [];
  const lineWaiters = new Set();
  const exitWaiters = new Set();
  let exitInfo = null;
  let fatalError = null;
  let stderrText = "";
  let remainder = "";

  const rejectAll = (makeError) => {
    for (const waiter of lineWaiters) {
      clearTimeout(waiter.timer);
      waiter.reject(makeError(waiter));
    }
    lineWaiters.clear();
  };

  const abort = (error) => {
    if (fatalError) return;
    fatalError = error;
    rejectAll(() => error);
  };

  const pushLine = (line) => {
    lines.push(line);
    for (const waiter of Array.from(lineWaiters)) {
      if (!waiter.predicate(line)) continue;
      clearTimeout(waiter.timer);
      lineWaiters.delete(waiter);
      waiter.resolve(line);
    }
    if (line.t === "fatal") {
      abort(new GateError("SIDECAR_FATAL", `sidecar fatal ${String(line.code)}: ${String(line.message)}`));
    }
  };

  const consumeLine = (raw) => {
    const text = raw.trim();
    if (!text) return;
    let parsed;
    try {
      parsed = JSON.parse(text);
    } catch {
      abort(new GateError("S1_STDOUT", `sidecar stdout is not NDJSON: ${text.slice(0, 200)}`));
      return;
    }
    if (!parsed || typeof parsed !== "object" || parsed.v !== 1 || typeof parsed.t !== "string") {
      abort(new GateError("S1_STDOUT", `sidecar stdout carried an invalid line: ${text.slice(0, 200)}`));
      return;
    }
    pushLine(parsed);
  };

  child.stdout.setEncoding("utf8").on("data", (chunk) => {
    const parts = (remainder + chunk).split("\n");
    remainder = parts.pop() ?? "";
    for (const part of parts) consumeLine(part);
  });
  child.stderr.setEncoding("utf8").on("data", (chunk) => {
    stderrText = (stderrText + chunk).slice(-STDERR_TAIL_BYTES);
  });
  child.on("error", (error) => {
    abort(new GateError("S0_SPAWN", `failed to spawn the sidecar: ${error.message}`));
  });
  child.on("exit", (code, signal) => {
    consumeLine(remainder);
    remainder = "";
    exitInfo = { code, signal, at: Date.now() };
    for (const waiter of exitWaiters) {
      clearTimeout(waiter.timer);
      waiter.resolve(exitInfo);
    }
    exitWaiters.clear();
    rejectAll(
      (waiter) =>
        new GateError(waiter.step, `sidecar exited before ${waiter.label} (code ${String(code)}, signal ${String(signal)})`),
    );
  });

  const waitForLine = (predicate, timeoutMs, step, label) => {
    if (fatalError) return Promise.reject(fatalError);
    const existing = lines.find(predicate);
    if (existing) return Promise.resolve(existing);
    if (exitInfo) return Promise.reject(new GateError(step, `sidecar already exited before ${label}`));
    return new Promise((resolve, reject) => {
      const waiter = { predicate, resolve, reject, step, label };
      waiter.timer = setTimeout(() => {
        lineWaiters.delete(waiter);
        reject(new GateError(step, `timed out after ${timeoutMs}ms while waiting for ${label}`));
      }, timeoutMs);
      lineWaiters.add(waiter);
    });
  };

  const waitForExit = (timeoutMs, step) => {
    if (exitInfo) return Promise.resolve(exitInfo);
    return new Promise((resolve, reject) => {
      const waiter = { resolve };
      waiter.timer = setTimeout(() => {
        exitWaiters.delete(waiter);
        reject(new GateError(step, `sidecar did not exit within ${timeoutMs}ms after stdin closed`));
      }, timeoutMs);
      exitWaiters.add(waiter);
    });
  };

  const sendLine = (line) => {
    if (!child.stdin.writable) throw new GateError("STDIN", "sidecar stdin is not writable");
    child.stdin.write(`${JSON.stringify({ v: 1, ...line })}\n`);
  };

  return {
    child,
    lines,
    waitForLine,
    waitForExit,
    sendLine,
    get exitInfo() {
      return exitInfo;
    },
    stderrTail(maxLines = 20) {
      return stderrText.trim().split(/\r?\n/).slice(-maxLines).join("\n");
    },
    /** Kills the launcher if it is still running (gate failure cleanup). */
    kill() {
      if (!exitInfo && child.pid !== undefined) child.kill();
    },
  };
}
