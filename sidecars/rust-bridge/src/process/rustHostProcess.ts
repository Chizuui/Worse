import { spawn, type ChildProcessByStdio } from "node:child_process";
import type { Readable, Writable } from "node:stream";
import { createJsonRpcConnection, type JsonRpcConnection } from "./jsonRpcConnection.ts";
import { PROTOCOL_VERSION, readInitializeResult, type InitializeResult } from "./messages.ts";

export const HANDSHAKE_TIMEOUT_MS = 10_000;
const SHUTDOWN_GRACE_MS = 2_000;
const CLIENT_NAME = "harness-rust-bridge";
const CLIENT_VERSION = "0.1.0";

export interface RustHostProcess {
  readonly connection: JsonRpcConnection;
  readonly info: InitializeResult;
  readonly methods: ReadonlySet<string>;
  readonly pid: number | undefined;
  /** Resolves once the child has exited (any reason). */
  readonly exited: Promise<void>;
  /** Ends stdin, waits up to 2 s, then kills. Idempotent. */
  dispose(): Promise<void>;
  /** Synchronous kill for process-exit paths. */
  killNow(): void;
}

export interface SpawnOptions {
  binaryPath: string;
  /** Extra arguments; harness-host itself takes none (tests run a Node fixture). */
  args?: readonly string[];
  cwd: string;
  /** Child environment; defaults to this process's env. */
  env?: NodeJS.ProcessEnv;
  onStderr: (text: string) => void;
  onDiagnostic: (message: string) => void;
}

export class HandshakeError extends Error {
  override readonly name = "HandshakeError";
}

/** Spawns `harness-host` and completes the `initialize` handshake (contract section 3). */
export async function startRustHostProcess(options: SpawnOptions): Promise<RustHostProcess> {
  const child: ChildProcessByStdio<Writable, Readable, Readable> = spawn(options.binaryPath, options.args ?? [], {
    cwd: options.cwd,
    env: options.env ?? process.env,
    stdio: ["pipe", "pipe", "pipe"],
    windowsHide: true
  });

  const exited = new Promise<void>((resolve) => {
    child.once("exit", () => resolve());
    child.once("error", () => resolve());
  });
  const spawnFailure = new Promise<never>((_resolve, reject) => {
    child.once("error", (error) => reject(new HandshakeError(`failed to spawn harness-host: ${error.message}`)));
  });
  // 修复说明：spawn 错误可能在握手完成之后才触发；未挂处理器的 rejected promise 会以
  // unhandledRejection 终止整个 host-node 进程（连同 TS Host）。这里只吞掉竞速用的副本。
  spawnFailure.catch(() => undefined);
  child.on("error", (error) => options.onDiagnostic(`harness-host process error: ${error.message}`));

  child.stderr.setEncoding("utf8").on("data", (chunk: string) => options.onStderr(chunk));
  const connection = createJsonRpcConnection(child.stdin, child.stdout, options.onDiagnostic);

  const killNow = (): void => {
    if (child.exitCode === null && child.signalCode === null) child.kill();
  };

  try {
    const raw = await Promise.race([
      connection.call(
        "initialize",
        { protocolVersion: PROTOCOL_VERSION, client: CLIENT_NAME, clientVersion: CLIENT_VERSION },
        { timeoutMs: HANDSHAKE_TIMEOUT_MS }
      ),
      spawnFailure
    ]);
    const info = readInitializeResult(raw);
    if (info === null) throw new HandshakeError("initialize returned a malformed result");
    if (info.protocolVersion !== PROTOCOL_VERSION) {
      throw new HandshakeError(`unsupported harness-host protocolVersion ${String(info.protocolVersion)}`);
    }

    let disposing: Promise<void> | null = null;
    const dispose = (): Promise<void> => {
      disposing ??= (async () => {
        connection.close("bridge disposed");
        child.stdin.end();
        const timer = setTimeout(killNow, SHUTDOWN_GRACE_MS);
        await exited;
        clearTimeout(timer);
      })();
      return disposing;
    };

    return { connection, info, methods: new Set(info.methods), pid: child.pid, exited, dispose, killNow };
  } catch (error) {
    connection.close("handshake failed");
    killNow();
    throw error instanceof HandshakeError ? error : new HandshakeError(error instanceof Error ? error.message : String(error));
  }
}
