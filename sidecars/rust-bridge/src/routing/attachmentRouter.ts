import type { BridgeLogger } from "../logger.ts";
import { RustCallError, RustCancelledError, RustTransportError } from "../process/jsonRpcConnection.ts";
import type { RustHostSupervisor } from "../process/supervisor.ts";
import { encodeError, encodeSuccess, readBody, readInboundHeader, readRequestArgs } from "../rpc/frames.ts";
import type { SessionRouter } from "../sessions/types.ts";
import { decideRustError } from "./errorMapping.ts";
import { mapRustResult } from "./resultMapping.ts";
import { findRoute, type Route, type RustService } from "./routeTable.ts";
import type { BridgeStats, RouteOutcome } from "./stats.ts";

export interface RouterDeps {
  supervisor: RustHostSupervisor;
  services: ReadonlySet<RustService>;
  stats: BridgeStats;
  logger: BridgeLogger;
  /** Logs every routing outcome at `info` (HARNESS_RUST_BRIDGE_TRACE=1). */
  trace: boolean;
  /** Stateful channels of this attachment (Phase 3b `terminal`, `file-watcher`); only enabled ones. */
  sessionRouters?: readonly SessionRouter[];
  /** Consumes Host responses to the bridge's own injected Host calls. */
  handleOutbound?: (frame: Uint8Array) => boolean;
}

export interface RouterIo {
  /** Sends a response frame back to the renderer. */
  reply(frame: Uint8Array): void;
  /** Hands the original request frame to the TS Host. */
  forward(frame: Uint8Array): void;
  /** Delivers a bridge-built request frame to the TS Host (injected Host calls). */
  injectToHost(frame: Uint8Array): void;
}

export interface AttachmentRouter {
  /** True when the frame was consumed by the bridge (the Host must not see it). */
  handleInbound(frame: Uint8Array): boolean;
  /** True when a Host -> renderer frame belongs to the bridge (it must not reach the renderer). */
  handleOutbound(frame: Uint8Array): boolean;
  /** Cancels every routed request of this attachment (attachment closed). */
  dispose(): void;
}

function isParamsObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * Per-attachment router. Owns the table of request ids it took from the TS
 * Host. A request id is in the table from interception until it is answered,
 * forwarded to TS, or cancelled; after that every frame for that id belongs to
 * the TS Host again, so a request is never answered twice.
 */
export function createAttachmentRouter(deps: RouterDeps, io: RouterIo): AttachmentRouter {
  const inFlight = new Map<number, { controller: AbortController; method: string }>();
  const sessionRouters = deps.sessionRouters ?? [];
  let disposed = false;

  const sessionRouterFor = (channel: string): SessionRouter | undefined =>
    sessionRouters.find((router) => router.channel === channel);

  const note = (method: string, outcome: RouteOutcome, detail?: string): void => {
    deps.stats.record(method, outcome);
    if (deps.trace) deps.logger("info", `rust-bridge route ${method} ${outcome}${detail ? ` (${detail})` : ""}`);
  };

  /** Returns false when the id was cancelled meanwhile (its answer must be dropped). */
  const release = (id: number): boolean => inFlight.delete(id);

  const fallback = (id: number, frame: Uint8Array, method: string, reason: string): void => {
    if (!release(id)) return;
    note(method, "fallback", reason);
    io.forward(frame);
  };

  const execute = async (
    id: number,
    frame: Uint8Array,
    route: Route,
    params: Record<string, unknown>,
    signal: AbortSignal
  ): Promise<void> => {
    const method = route.rustMethod;
    const host = await deps.supervisor.acquire();
    if (host === null) return fallback(id, frame, method, "harness-host unavailable");
    if (!host.methods.has(method)) return fallback(id, frame, method, "method not advertised");
    try {
      const raw = await host.connection.call(method, params, { timeoutMs: route.timeoutMs, signal });
      const mapped = mapRustResult(route.result, raw);
      if (!mapped.ok) return fallback(id, frame, method, mapped.reason);
      if (!release(id)) return;
      io.reply(encodeSuccess(id, mapped.value));
      note(method, "rust");
    } catch (error) {
      if (error instanceof RustCancelledError) {
        // 取消只由 handleInbound/dispose 发起，它们已移除 id 并记账；这里不再重复处理。
        release(id);
        return;
      }
      if (error instanceof RustCallError) {
        const decision = decideRustError(error.rpcError);
        if (decision.kind === "fallback") return fallback(id, frame, method, decision.reason);
        if (!release(id)) return;
        io.reply(encodeError(id, decision.payload));
        note(method, "surfaced-error", decision.payload.message);
        return;
      }
      const reason = error instanceof RustTransportError ? `transport ${error.reason}` : String(error);
      fallback(id, frame, method, reason);
    }
  };

  const handleInbound = (frame: Uint8Array): boolean => {
    if (disposed || (deps.services.size === 0 && sessionRouters.length === 0) || deps.supervisor.state.kind === "disabled") return false;
    const header = readInboundHeader(frame);
    if (header === null) return false;
    if (header.kind === "eventDispose") return sessionRouters.some((router) => router.handleEventDispose(header.id));
    if (header.kind === "listen") {
      const router = sessionRouterFor(header.channel);
      return router !== undefined && router.handleListen(header.id, header.event, readBody(frame, header.bodyOffset));
    }
    if (header.kind === "cancel") {
      if (sessionRouters.some((router) => router.handleCancel(header.id))) return true;
      const entry = inFlight.get(header.id);
      if (entry === undefined) return false;
      // 先移出表再中止：之后无论 Rust 迟到的结果还是回退路径都会被丢弃，不会把已取消的请求再转给 TS。
      inFlight.delete(header.id);
      entry.controller.abort();
      note(entry.method, "cancelled");
      return true;
    }
    const sessionRouter = sessionRouterFor(header.channel);
    const route = sessionRouter === undefined ? findRoute(header.channel, header.method) : undefined;
    if (sessionRouter === undefined && (route === undefined || !deps.services.has(route.service))) return false;
    const args = readRequestArgs(frame, header.bodyOffset);
    if (args === null) return false;
    // project.readTree() / task-groups.read() 由 UI 无参调用；仅对声明 allowNoArgs 的路由按 {} 处理。
    const noArgs = args.length === 0 && route?.allowNoArgs === true;
    const params: unknown = noArgs ? {} : args[0];
    if ((!noArgs && args.length !== 1) || !isParamsObject(params)) return false;
    if (sessionRouter !== undefined) return sessionRouter.handlePromise(header.id, header.method, params, frame);
    if (route === undefined) return false;
    const controller = new AbortController();
    inFlight.set(header.id, { controller, method: route.rustMethod });
    void execute(header.id, frame, route, params, controller.signal).catch((error: unknown) => {
      deps.logger("error", "rust-bridge request handling failed", error);
      fallback(header.id, frame, route.rustMethod, "bridge failure");
    });
    return true;
  };

  return {
    handleInbound,
    handleOutbound: (frame) => deps.handleOutbound?.(frame) ?? false,
    dispose() {
      disposed = true;
      for (const router of sessionRouters) router.dispose();
      const entries = Array.from(inFlight.values());
      inFlight.clear();
      for (const entry of entries) entry.controller.abort();
    }
  };
}
