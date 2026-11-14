import type { BridgeLogger } from "../logger.ts";
import type { RustHostSupervisor } from "../process/supervisor.ts";
import type { RpcErrorPayload } from "../rpc/frames.ts";
import type { BridgeStats, RouteOutcome } from "../routing/stats.ts";
import type { SessionDirectory } from "./sessionDirectory.ts";

/** Frame sinks of one attachment. */
export interface SessionIo {
  /** Sends a frame to the renderer. */
  reply(frame: Uint8Array): void;
  /** Hands the original request frame to the TS Host (fallback / not ours). */
  forward(frame: Uint8Array): void;
}

/**
 * A per-attachment router for one stateful RPC channel (`terminal`, `file-watcher`).
 * Every `handle*` returns true when the frame was consumed; false means "not
 * ours", and the frame goes to the TS Host unchanged.
 */
export interface SessionRouter {
  readonly channel: string;
  handlePromise(id: number, method: string, params: Record<string, unknown>, frame: Uint8Array): boolean;
  handleCancel(id: number): boolean;
  handleListen(id: number, event: string, arg: unknown): boolean;
  handleEventDispose(id: number): boolean;
  /** Attachment closed: drop this attachment's subscriptions and in-flight calls (and the sessions it owns, per channel policy). */
  dispose(): void;
}

export interface SessionDeps {
  supervisor: RustHostSupervisor;
  directory: SessionDirectory;
  stats: BridgeStats;
  logger: BridgeLogger;
  trace: boolean;
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** `new Error(message)` as the TS ChannelServer serializes it (stack is not compared). */
export function errorPayload(message: string): RpcErrorPayload {
  return { name: "Error", message, stack: undefined };
}

export function createNoter(deps: SessionDeps): (method: string, outcome: RouteOutcome, detail?: string) => void {
  return (method, outcome, detail) => {
    deps.stats.record(method, outcome);
    if (deps.trace) deps.logger("info", `rust-bridge route ${method} ${outcome}${detail ? ` (${detail})` : ""}`);
  };
}

export function describeError(error: unknown): string {
  return error instanceof Error ? `${error.name}: ${error.message}` : String(error);
}
