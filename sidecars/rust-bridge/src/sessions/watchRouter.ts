import { RustCallError, RustTransportError } from "../process/jsonRpcConnection.ts";
import type { RustHostProcess } from "../process/rustHostProcess.ts";
import { encodeError, encodeEventFire, encodeSuccess } from "../rpc/frames.ts";
import { decideRustError } from "../routing/errorMapping.ts";
import { createNoter, describeError, isRecord, type SessionDeps, type SessionIo, type SessionRouter } from "./types.ts";

export const FILE_WATCHER_CHANNEL = "file-watcher";
const WATCH_TIMEOUT_MS = 10_000;

/** Process-wide counter: bridge watcher ids never collide with the TS Host's numeric ids. */
let nextWatchNumber = 1;

/** TS `FileWatchEvent`: `changedPath` omitted (not undefined-valued) when unknown. */
export interface FileWatchEvent {
  dirPath: string;
  changedPath?: string;
}

interface WatchSession {
  readonly watchId: string;
  readonly rustId: string;
  readonly host: RustHostProcess;
  readonly dirPath: string;
  readonly listeners: Set<number>;
  unregister: () => void;
  alive: boolean;
}

function readWatchEvent(value: unknown, fallbackDirPath: string): FileWatchEvent {
  if (!isRecord(value) || typeof value.dirPath !== "string") return { dirPath: fallbackDirPath };
  return typeof value.changedPath === "string" ? { dirPath: value.dirPath, changedPath: value.changedPath } : { dirPath: value.dirPath };
}

/**
 * `file-watcher` channel for one local attachment (spec W1, W2, O1, O2). Owns the
 * attachment's Rust watchers and their event subscriptions. Events are not
 * buffered without a listener (TS `Emitter` drops them too).
 */
export function createWatchRouter(deps: SessionDeps, io: SessionIo): SessionRouter {
  const note = createNoter(deps);
  const sessions = new Map<string, WatchSession>();
  const listens = new Map<number, WatchSession>();
  const subscribing = new Set<number>();
  let disposed = false;

  const fire = (session: WatchSession, event: FileWatchEvent): void => {
    for (const reqId of session.listeners) io.reply(encodeEventFire(reqId, event));
  };

  const forget = (session: WatchSession): boolean => {
    if (!session.alive) return false;
    session.alive = false;
    session.unregister();
    for (const reqId of session.listeners) listens.delete(reqId);
    session.listeners.clear();
    sessions.delete(session.watchId);
    return true;
  };

  const unsubscribe = async (session: WatchSession): Promise<void> => {
    if (!forget(session)) return;
    try {
      await session.host.connection.call("watch.unsubscribe", { watchId: session.rustId }, { timeoutMs: WATCH_TIMEOUT_MS });
      note("watch.unsubscribe", "rust");
    } catch (error) {
      note("watch.unsubscribe", "surfaced-error", describeError(error));
    }
  };

  const registerSession = (host: RustHostProcess, value: unknown, dirPath: string): WatchSession | null => {
    if (!isRecord(value) || typeof value.watchId !== "string") return null;
    const session: WatchSession = {
      watchId: `rw-${nextWatchNumber++}`,
      rustId: value.watchId,
      host,
      dirPath,
      listeners: new Set(),
      unregister: () => undefined,
      alive: true
    };
    session.unregister = deps.directory.register(host, "watch", session.rustId, {
      notify(method, params) {
        if (!session.alive) return;
        if (method === "$/watch/event") fire(session, readWatchEvent(params.event, session.dirPath));
        else if (method === "$/watch/closed") forget(session);
      },
      hostLost() {
        if (!session.alive) return;
        // 与 TS watcher 出错路径一致：发出最终的 {dirPath} 事件后清理，让订阅方整体刷新。
        fire(session, { dirPath: session.dirPath });
        forget(session);
      }
    });
    sessions.set(session.watchId, session);
    return session;
  };

  const subscribe = async (id: number, frame: Uint8Array, path: string, recursive: unknown): Promise<void> => {
    const fallback = (reason: string): void => {
      if (!subscribing.delete(id)) return;
      note("watch.subscribe", "fallback", reason);
      io.forward(frame);
    };
    const host = await deps.supervisor.acquire();
    if (host === null) return fallback("harness-host unavailable");
    if (!host.methods.has("watch.subscribe")) return fallback("method not advertised");
    if (!subscribing.has(id)) return;
    const params: Record<string, unknown> = recursive === undefined ? { path } : { path, recursive };
    const subscribed: { session: WatchSession | null } = { session: null };
    try {
      // 不传 AbortSignal：取消后 Rust 侧可能已建立监视，必须等结果回来再注销，避免泄漏句柄。
      await host.connection.call("watch.subscribe", params, {
        timeoutMs: WATCH_TIMEOUT_MS,
        onResult: (value) => {
          subscribed.session = registerSession(host, value, path);
        }
      });
    } catch (error) {
      if (error instanceof RustCallError) {
        const decision = decideRustError(error.rpcError);
        if (decision.kind === "fallback") return fallback(decision.reason);
        if (!subscribing.delete(id)) return;
        io.reply(encodeError(id, decision.payload));
        note("watch.subscribe", "surfaced-error", decision.payload.message);
        return;
      }
      if (error instanceof RustTransportError && error.reason === "timeout") deps.supervisor.reportFailure(host, "watch.subscribe timed out");
      return fallback(describeError(error));
    }
    const session = subscribed.session;
    if (session === null) return fallback("malformed watch.subscribe result");
    if (!subscribing.delete(id) || disposed) {
      await unsubscribe(session);
      return;
    }
    io.reply(encodeSuccess(id, { id: session.watchId }));
    note("watch.subscribe", "rust");
  };

  return {
    channel: FILE_WATCHER_CHANNEL,
    handlePromise(id, method, params, frame) {
      if (disposed) return false;
      if (method === "watch") {
        if (typeof params.path !== "string") return false;
        subscribing.add(id);
        void subscribe(id, frame, params.path, params.recursive).catch((error: unknown) => {
          deps.logger("error", "rust-bridge watch subscribe failed", error);
          if (subscribing.delete(id)) io.forward(frame);
        });
        return true;
      }
      if (method === "disposeAll") {
        // TS 也会清理它自己的 watcher，因此这里只清理本 attachment 的 Rust watcher 后照常转发。
        for (const session of Array.from(sessions.values())) void unsubscribe(session);
        return false;
      }
      if (method !== "unwatch") return false;
      const session = typeof params.id === "string" ? sessions.get(params.id) : undefined;
      if (session === undefined) return false;
      void unsubscribe(session).then(() => io.reply(encodeSuccess(id, undefined)));
      return true;
    },
    handleCancel(id) {
      if (!subscribing.delete(id)) return false;
      note("watch.subscribe", "cancelled");
      return true;
    },
    handleListen(reqId, event, arg) {
      if (disposed || event !== "onDynamicChange" || typeof arg !== "string") return false;
      const session = sessions.get(arg);
      if (session === undefined) return false;
      session.listeners.add(reqId);
      listens.set(reqId, session);
      return true;
    },
    handleEventDispose(reqId) {
      const session = listens.get(reqId);
      if (session === undefined) return false;
      listens.delete(reqId);
      session.listeners.delete(reqId);
      return true;
    },
    dispose() {
      if (disposed) return;
      disposed = true;
      subscribing.clear();
      for (const session of Array.from(sessions.values())) void unsubscribe(session);
      listens.clear();
    }
  };
}
