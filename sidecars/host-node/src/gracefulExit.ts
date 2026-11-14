import { log } from "./log.ts";

type ExitCodeInput = number | string | null | undefined;

export interface GracefulExitOptions {
  /** How long the event loop may take to drain before the bounded hard exit. */
  drainTimeoutMs: number;
  /** Launcher-owned teardown (WS server, harness-host) that must finish before draining. */
  cleanup: () => Promise<void>;
}

export interface GracefulExit {
  /** Requests an orderly exit; idempotent, the first requested code wins. */
  request(code: number, reason: string): void;
  /** Bypasses draining. Only for paths where no Host isolate work can be in flight. */
  hardExit(code: number): never;
}

interface UnrefableHandle {
  unref?: () => unknown;
}

type ProcessWithHandles = NodeJS.Process & { _getActiveHandles?: () => unknown[] };

function toExitCode(code: ExitCodeInput): number {
  if (code === null || code === undefined) {
    return typeof process.exitCode === "number" ? process.exitCode : 0;
  }
  const value = typeof code === "number" ? code : Number.parseInt(code, 10);
  return Number.isInteger(value) ? value : 1;
}

function unrefActiveHandles(): number {
  const handles = (process as ProcessWithHandles)._getActiveHandles?.() ?? [];
  let count = 0;
  for (const handle of handles) {
    if (typeof handle !== "object" || handle === null) continue;
    const candidate = handle as UnrefableHandle;
    if (typeof candidate.unref === "function") {
      candidate.unref();
      count += 1;
    }
  }
  return count;
}

/**
 * Replaces `process.exit` with an orderly exit that lets the event loop drain.
 *
 * 修复说明（Host 退出崩溃，Windows 约 25% 复现）：Host 在 dispose 完成后直接调用
 * `process.exit()`。Node 的 process.exit 路径（DefaultProcessExitHandlerInternal）在 V8
 * isolate 仍存活时就执行 DisposePlatform：WorkerThreadsTaskRunner::Shutdown 先让
 * DelayedTaskScheduler 线程 uv_close 自己的 flush_tasks_ 异步句柄，之后才 join V8 后台
 * 线程。若此时仍在运行的 V8 后台任务（并发 GC/编译）调用 PostDelayedTask，就会对正在
 * 关闭的句柄执行 uv_async_send，触发 libuv 断言 `!(handle->flags & UV_HANDLE_CLOSING)`
 * （src\win\async.c:76）并以 0xC0000409 中止。证据：40 次冒烟 10 次崩溃；加
 * `--single-threaded`（无 V8 后台线程）40 次 0 崩溃。自然退出路径会先 Dispose isolate
 * （取消并等待 V8 后台任务）再关闭平台，不存在该竞态。因此这里把 exit 请求改为：设置
 * exitCode、完成 launcher 自身清理、unref 剩余句柄，让事件循环自然排空退出；仅在超时后
 * 才回退到真正的 process.exit。
 */
export function installGracefulExit(options: GracefulExitOptions): GracefulExit {
  const realExit = process.exit.bind(process);
  let requested = false;

  const hardExit = (code: number): never => realExit(code);

  const drain = async (code: number, reason: string): Promise<void> => {
    try {
      await options.cleanup();
    } catch (error) {
      log("warn", "launcher cleanup failed during exit", error);
    }
    const released = unrefActiveHandles();
    log("debug", `exit(${code}) requested (${reason}); draining the event loop`, { released });
    const fallback = setTimeout(() => {
      log("warn", "event loop did not drain in time; forcing exit", {
        resources: process.getActiveResourcesInfo()
      });
      hardExit(code);
    }, options.drainTimeoutMs);
    // 兜底计时器本身不能阻止事件循环排空。
    fallback.unref();
  };

  const request = (code: number, reason: string): void => {
    if (requested) {
      return;
    }
    requested = true;
    process.exitCode = code;
    void drain(code, reason);
  };

  const deferredExit = (code?: ExitCodeInput): void => {
    request(toExitCode(code), "process.exit");
  };
  // Host 的 exit 调用点都位于 dispose 回调末尾；唯一的例外（重入的致命异常）后续只会再次
  // 调用幂等的 disposeHostResources，因此 exit 返回而不中止当前调用栈是安全的。
  process.exit = deferredExit as typeof process.exit;

  return { request, hardExit };
}
