import { RustCallError, RustTransportError } from "../process/jsonRpcConnection.ts";
import { ErrorCode } from "../process/messages.ts";
import type { RustHostProcess } from "../process/rustHostProcess.ts";
import { encodeError, encodeEventFire, encodeSuccess } from "../rpc/frames.ts";
import type { HostCaller } from "./hostCall.ts";
import { createOutputBuffer, type OutputBuffer } from "./outputBuffer.ts";
import { loadTerminalPolicy, readTerminalSettings, type TerminalPolicy, type TerminalSettings } from "./terminalPolicy.ts";
import { createNoter, describeError, errorPayload, isRecord, type SessionDeps, type SessionIo, type SessionRouter } from "./types.ts";

export const TERMINAL_CHANNEL = "terminal";
const SPAWN_TIMEOUT_MS = 15_000;
const SESSION_CALL_TIMEOUT_MS = 10_000;
const SETTINGS_TIMEOUT_MS = 5_000;
/** Exit code reported when harness-host dies under a live terminal (spec O2). */
export const HOST_LOST_EXIT_CODE = -1;

/** One event subscription: the attachment that asked and its request id. */
interface Listener {
  readonly io: SessionIo;
  readonly reqId: number;
}

export interface PtySession {
  readonly terminalId: string;
  readonly rustId: string;
  readonly host: RustHostProcess;
  readonly output: OutputBuffer;
  readonly dataListeners: Set<Listener>;
  readonly exitListeners: Set<Listener>;
  unregister: () => void;
  alive: boolean;
}

/**
 * Process-level owner of the Rust terminals (spec T3). Desktop-continuous
 * terminals are Host-global in the TS Host: a renderer whose RPC proxy is
 * regenerated re-subscribes to the same terminal id over a new attachment, so
 * sessions must outlive the attachment that created them. Attachments only own
 * their listeners. Sessions end on PTY exit, `terminal.dispose`, or host loss.
 */
export interface PtySessionStore {
  get(terminalId: string): PtySession | undefined;
  /** Registers a `pty.spawn` result; null when malformed. Must run synchronously in `onResult`. */
  register(host: RustHostProcess, value: unknown): PtySession | null;
  /** Ends the session without an exit event and kills its process tree. */
  kill(session: PtySession): Promise<void>;
  size(): number;
}

/** Process-wide counter: bridge terminal ids never collide with the TS Host's numeric ids. */
let nextTerminalNumber = 1;

export function createPtySessionStore(deps: SessionDeps): PtySessionStore {
  const note = createNoter(deps);
  const sessions = new Map<string, PtySession>();

  /** Session ended by the PTY itself or by host loss: notify exit listeners once, then forget the id. */
  const end = (session: PtySession, exitCode: number): void => {
    if (!session.alive) return;
    session.alive = false;
    session.unregister();
    for (const listener of session.exitListeners) listener.io.reply(encodeEventFire(listener.reqId, exitCode));
    session.dataListeners.clear();
    session.exitListeners.clear();
    sessions.delete(session.terminalId);
  };

  return {
    get: (terminalId) => sessions.get(terminalId),
    size: () => sessions.size,
    register(host, value) {
      if (!isRecord(value) || typeof value.sessionId !== "string") return null;
      const session: PtySession = {
        terminalId: `rpty-${nextTerminalNumber++}`,
        rustId: value.sessionId,
        host,
        output: createOutputBuffer(),
        dataListeners: new Set(),
        exitListeners: new Set(),
        unregister: () => undefined,
        alive: true
      };
      session.unregister = deps.directory.register(host, "pty", session.rustId, {
        notify(method, params) {
          if (!session.alive) return;
          if (method === "$/pty/output") {
            const data = params.data;
            if (typeof data !== "string" || data === "") return;
            if (session.dataListeners.size === 0) session.output.push(data);
            else for (const listener of session.dataListeners) listener.io.reply(encodeEventFire(listener.reqId, data));
          } else if (method === "$/pty/exit") {
            end(session, typeof params.exitCode === "number" ? params.exitCode : HOST_LOST_EXIT_CODE);
          }
        },
        hostLost() {
          end(session, HOST_LOST_EXIT_CODE);
        }
      });
      sessions.set(session.terminalId, session);
      return session;
    },
    async kill(session) {
      if (!session.alive) return;
      session.alive = false;
      session.unregister();
      session.dataListeners.clear();
      session.exitListeners.clear();
      sessions.delete(session.terminalId);
      try {
        await session.host.connection.call("pty.kill", { sessionId: session.rustId }, { timeoutMs: SESSION_CALL_TIMEOUT_MS });
        note("pty.kill", "rust");
      } catch (error) {
        // 进程已不在时 Job Object / SIGHUP 已回收 shell；超时则说明 harness-host 卡死，按故障处理以杀掉整棵进程。
        if (error instanceof RustTransportError && error.reason === "timeout") deps.supervisor.reportFailure(session.host, "pty.kill timed out");
        note("pty.kill", "surfaced-error", describeError(error));
      }
    }
  };
}

export interface PtyRouterDeps extends SessionDeps {
  store: PtySessionStore;
  hostCaller: HostCaller;
  /** Injectable for tests; defaults to the bundled ZCode policy. */
  loadPolicy?: () => Promise<TerminalPolicy>;
  env?: NodeJS.ProcessEnv;
}

interface SpawnPlan {
  params: Record<string, unknown>;
  result: (terminalId: string, shell: string) => Record<string, unknown>;
}

/**
 * Builds `pty.spawn` params and the TS `create` result in TS order: shell, cwd,
 * env, settings -> font profile, windowsPty. Throws whatever the ZCode policy
 * throws (the caller then lets the TS Host answer, which throws the same).
 */
function planSpawn(policy: TerminalPolicy, params: Record<string, unknown>, settings: TerminalSettings, env: NodeJS.ProcessEnv): SpawnPlan {
  const shell = policy.resolveShell();
  const cwd = policy.resolveCwd(typeof params.cwd === "string" ? params.cwd : undefined);
  const spawnEnv = policy.resolveEnv(env);
  const profile = policy.resolveFontProfile(settings, env);
  const windowsPty = policy.resolveWindowsPtyInfo();
  return {
    params: {
      shell,
      args: [],
      cwd,
      env: spawnEnv,
      cols: params.cols,
      rows: params.rows,
      name: "xterm-256color"
    },
    // 字段顺序与 TS terminalService.create 的返回对象一致。
    result: (terminalId, usedShell) => ({
      id: terminalId,
      shell: usedShell,
      fontFamily: profile.fontFamily,
      fontSize: profile.fontSize,
      theme: profile.theme,
      fontFamilySource: profile.source,
      windowsPty
    })
  };
}

/**
 * `terminal` channel for one desktop-continuous local attachment (spec T1-T3, O1, O2).
 * Owns this attachment's in-flight `create` calls and event subscriptions; the
 * sessions themselves live in the shared store. Ids the store does not know go
 * to the TS Host.
 */
export function createPtyRouter(deps: PtyRouterDeps, io: SessionIo): SessionRouter {
  const note = createNoter(deps);
  const { store } = deps;
  const loadPolicy = deps.loadPolicy ?? loadTerminalPolicy;
  const env = deps.env ?? process.env;
  const listens = new Map<number, { session: PtySession; listener: Listener; kind: "data" | "exit" }>();
  /** `create` requests in flight; a cancelled entry is removed so its late result is killed, not sent. */
  const creating = new Set<number>();
  let disposed = false;

  const readSettings = async (): Promise<TerminalSettings> => {
    try {
      return readTerminalSettings(await deps.hostCaller.call("setting", "get", [], SETTINGS_TIMEOUT_MS));
    } catch {
      // 与 TS create 的 catch 分支一致：读取设置失败时继承系统 profile。
      return { terminalInheritSystemProfile: true };
    }
  };

  const create = async (id: number, frame: Uint8Array, params: Record<string, unknown>): Promise<void> => {
    const fallback = (reason: string): void => {
      if (!creating.delete(id)) return;
      note("pty.spawn", "fallback", reason);
      io.forward(frame);
    };
    const host = await deps.supervisor.acquire();
    if (host === null) return fallback("harness-host unavailable");
    if (!host.methods.has("pty.spawn")) return fallback("method not advertised");
    let plan: SpawnPlan;
    try {
      const policy = await loadPolicy();
      plan = planSpawn(policy, params, await readSettings(), env);
    } catch (error) {
      return fallback(`policy: ${describeError(error)}`);
    }
    if (!creating.has(id)) return;
    // 结果在 onResult 中同步登记（早于该会话的第一条输出通知），用对象承载以免被控制流收窄。
    const spawned: { session: PtySession | null; shell: string } = { session: null, shell: "" };
    try {
      // 不传 AbortSignal：取消后 Rust 仍可能已拉起 shell，必须等结果回来再 kill，否则会留下孤儿进程。
      await host.connection.call("pty.spawn", plan.params, {
        timeoutMs: SPAWN_TIMEOUT_MS,
        onResult: (value) => {
          spawned.session = store.register(host, value);
          spawned.shell = isRecord(value) && typeof value.shell === "string" ? value.shell : String(plan.params.shell);
        }
      });
    } catch (error) {
      if (error instanceof RustTransportError && error.reason === "timeout") deps.supervisor.reportFailure(host, "pty.spawn timed out");
      return fallback(error instanceof RustCallError ? `spawn: ${error.message}` : describeError(error));
    }
    const created = spawned.session;
    if (created === null) return fallback("malformed pty.spawn result");
    if (!creating.delete(id) || disposed) {
      // 请求方已取消或 attachment 已关闭：没有人知道这个 id，立即回收，避免无主 PTY。
      await store.kill(created);
      return;
    }
    io.reply(encodeSuccess(id, plan.result(created.terminalId, spawned.shell)));
    note("pty.spawn", "rust");
  };

  const sessionCall = (id: number, session: PtySession, method: "pty.write" | "pty.resize", params: Record<string, unknown>): void => {
    void session.host.connection.call(method, { sessionId: session.rustId, ...params }, { timeoutMs: SESSION_CALL_TIMEOUT_MS }).then(
      () => {
        io.reply(encodeSuccess(id, undefined));
        note(method, "rust");
      },
      (error: unknown) => {
        const notFound = `Terminal not found: ${session.terminalId}`;
        if (error instanceof RustCallError && error.rpcError.code === ErrorCode.AppError) {
          const message = error.message.startsWith("Terminal not found") ? notFound : error.message;
          io.reply(encodeError(id, errorPayload(message)));
        } else {
          // 会话只存在于 Rust 中，无法回退给 TS；传输故障时会话随进程一起结束。
          io.reply(encodeError(id, errorPayload(error instanceof RustCallError ? error.message : notFound)));
        }
        note(method, "surfaced-error", describeError(error));
      }
    );
  };

  return {
    channel: TERMINAL_CHANNEL,
    handlePromise(id, method, params, frame) {
      if (disposed) return false;
      if (method === "create") {
        creating.add(id);
        void create(id, frame, params).catch((error: unknown) => {
          deps.logger("error", "rust-bridge terminal create failed", error);
          if (creating.delete(id)) io.forward(frame);
        });
        return true;
      }
      const session = typeof params.id === "string" ? store.get(params.id) : undefined;
      if (session === undefined) return false;
      switch (method) {
        case "write":
          sessionCall(id, session, "pty.write", { data: typeof params.data === "string" ? params.data : String(params.data) });
          return true;
        case "resize":
          sessionCall(id, session, "pty.resize", { cols: params.cols, rows: params.rows });
          return true;
        case "dispose":
          void store.kill(session).then(() => io.reply(encodeSuccess(id, undefined)));
          return true;
        default:
          return false;
      }
    },
    handleCancel(id) {
      if (!creating.delete(id)) return false;
      note("pty.spawn", "cancelled");
      return true;
    },
    handleListen(reqId, event, arg) {
      if (disposed || (event !== "onDynamicData" && event !== "onDynamicExit") || typeof arg !== "string") return false;
      const session = store.get(arg);
      if (session === undefined) return false;
      const listener: Listener = { io, reqId };
      if (event === "onDynamicExit") {
        session.exitListeners.add(listener);
        listens.set(reqId, { session, listener, kind: "exit" });
        return true;
      }
      const first = session.dataListeners.size === 0;
      session.dataListeners.add(listener);
      listens.set(reqId, { session, listener, kind: "data" });
      // 与 TS onWillAddFirstListener 一致：无监听期间缓存的输出按序回放给第一个新监听者。
      if (first) for (const chunk of session.output.drain()) io.reply(encodeEventFire(reqId, chunk));
      return true;
    },
    handleEventDispose(reqId) {
      const entry = listens.get(reqId);
      if (entry === undefined) return false;
      listens.delete(reqId);
      (entry.kind === "data" ? entry.session.dataListeners : entry.session.exitListeners).delete(entry.listener);
      return true;
    },
    dispose() {
      if (disposed) return;
      disposed = true;
      creating.clear();
      // 桌面终端是 Host 全局的（渲染进程代理换代后会在新 attachment 上按原 id 重新订阅），
      // 因此 attachment 关闭只撤销本 attachment 的订阅，不杀 PTY；此后的输出进入缓冲区等待重新订阅。
      for (const { session, listener, kind } of listens.values()) {
        (kind === "data" ? session.dataListeners : session.exitListeners).delete(listener);
      }
      listens.clear();
    }
  };
}
