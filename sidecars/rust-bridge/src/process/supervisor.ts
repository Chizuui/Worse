import { existsSync } from "node:fs";
import type { BridgeLogger } from "../logger.ts";
import { startRustHostProcess, type RustHostProcess } from "./rustHostProcess.ts";

/** Restart backoff after a crash (contract section 7). */
export const RESTART_BACKOFF_MS: readonly number[] = [500, 2_000, 10_000];
export const CRASH_WINDOW_MS = 60_000;
export const MAX_CRASHES_IN_WINDOW = 3;

export type SupervisorState =
  | { kind: "idle" }
  | { kind: "starting" }
  | { kind: "ready"; pid: number | undefined }
  | { kind: "backoff"; until: number }
  | { kind: "disabled"; reason: string };

export interface RustHostSupervisor {
  /** Ready host, or null when Rust is unavailable right now (caller falls back to TS). */
  acquire(): Promise<RustHostProcess | null>;
  /** Reports a transport failure seen by a caller; counts as a crash. */
  reportFailure(host: RustHostProcess, reason: string): void;
  readonly state: SupervisorState;
  dispose(): Promise<void>;
  killNow(): void;
}

export interface SupervisorOptions {
  binaryPath: string;
  args?: readonly string[];
  cwd: string;
  /** harness-host environment (default: this process's env). */
  env?: NodeJS.ProcessEnv;
  logger: BridgeLogger;
  now?: () => number;
  start?: typeof startRustHostProcess;
}

/**
 * Owns the single `harness-host` child of this host-node process.
 *
 * Spawns lazily on first use, restarts after crashes with backoff, and disables
 * Rust routing for the process lifetime after 3 crashes in 60 s or when the
 * binary is missing. Every "unavailable" answer makes the caller use TS.
 */
export function createRustHostSupervisor(options: SupervisorOptions): RustHostSupervisor {
  const now = options.now ?? Date.now;
  const start = options.start ?? startRustHostProcess;
  const { logger } = options;
  let state: SupervisorState = { kind: "idle" };
  let current: RustHostProcess | null = null;
  let starting: Promise<RustHostProcess | null> | null = null;
  let crashes: number[] = [];
  let disposed = false;

  const disable = (reason: string): void => {
    state = { kind: "disabled", reason };
    logger("warn", `rust services disabled for this process: ${reason}`);
  };

  const recordCrash = (reason: string): void => {
    const at = now();
    crashes = crashes.filter((time) => at - time < CRASH_WINDOW_MS);
    crashes.push(at);
    if (crashes.length >= MAX_CRASHES_IN_WINDOW) {
      disable(`harness-host failed ${crashes.length} times within ${CRASH_WINDOW_MS / 1000}s (last: ${reason})`);
      return;
    }
    const delay = RESTART_BACKOFF_MS[Math.min(crashes.length - 1, RESTART_BACKOFF_MS.length - 1)] ?? 10_000;
    state = { kind: "backoff", until: at + delay };
    logger("warn", `harness-host unavailable (${reason}); retry in ${delay}ms`);
  };

  const onProcessGone = (host: RustHostProcess, reason: string): void => {
    if (current !== host) return;
    current = null;
    if (disposed || state.kind === "disabled") return;
    recordCrash(reason);
  };

  const spawnProcess = async (): Promise<RustHostProcess | null> => {
    state = { kind: "starting" };
    try {
      const host = await start({
        binaryPath: options.binaryPath,
        ...(options.args === undefined ? {} : { args: options.args }),
        cwd: options.cwd,
        ...(options.env === undefined ? {} : { env: options.env }),
        onStderr: (text) => logger("debug", `harness-host: ${text.trimEnd()}`),
        onDiagnostic: (message) => logger("warn", `harness-host protocol: ${message}`)
      });
      if (disposed) {
        void host.dispose();
        return null;
      }
      current = host;
      state = { kind: "ready", pid: host.pid };
      logger(
        "info",
        `harness-host ready pid=${String(host.pid)} version=${host.info.serverVersion} methods=${host.methods.size}`
      );
      host.connection.onClose((error) => onProcessGone(host, error.message));
      void host.exited.then(() => onProcessGone(host, "process exited"));
      return host;
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      if (/unsupported harness-host protocolVersion/.test(message)) disable(message);
      else recordCrash(message);
      return null;
    }
  };

  const acquire = async (): Promise<RustHostProcess | null> => {
    if (disposed) return null;
    if (current !== null && !current.connection.isClosed) return current;
    if (state.kind === "disabled") return null;
    if (state.kind === "backoff" && now() < state.until) return null;
    if (starting !== null) return starting;
    if (!existsSync(options.binaryPath)) {
      disable(`binary not found at ${options.binaryPath}`);
      return null;
    }
    starting = spawnProcess().finally(() => {
      starting = null;
    });
    return starting;
  };

  const reportFailure = (host: RustHostProcess, reason: string): void => {
    if (current !== host) return;
    // 超时或协议错乱后进程状态不可信：直接终止并按崩溃计数，避免后续请求继续排队到坏进程。
    host.killNow();
    host.connection.close(reason);
  };

  return {
    acquire,
    reportFailure,
    get state() {
      return state;
    },
    dispose: async () => {
      disposed = true;
      const host = current;
      current = null;
      if (host !== null) await host.dispose();
    },
    killNow: () => {
      disposed = true;
      current?.killNow();
    }
  };
}
