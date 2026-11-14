import { log } from "./log.ts";
import type { SidecarLine } from "./protocol.ts";

export interface ProcessGuards {
  /** Called once the Host module has loaded and installed its own handlers. */
  handOverToHost(): void;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * Process-level error handling for the launcher.
 *
 * Before the Host loads, any uncaught error is fatal for the launcher. After
 * the Host loads, the Host's own `uncaughtException` handler decides.
 */
export function installProcessGuards(
  emit: (line: SidecarLine) => void,
  shutdown: (exitCode: number) => void
): ProcessGuards {
  let hostOwnsErrors = false;

  const onUncaught = (error: Error): void => {
    log("error", "uncaught exception", error);
    if (hostOwnsErrors) {
      // 修复说明：Host 的 uncaughtException 处理器会对 native TLS 分配失败做“可恢复”判定，
      // 只有致命错误才自行 dispose 并 exit(1)。若 launcher 在此一律 dispose+exit，
      // 会把 Host 有意容忍的错误升级为整个 Host 退出（Electron 下不会发生），
      // 因此 Host 加载后 launcher 只上报日志，由 Host 决定是否退出。
      emit({ v: 1, t: "log", level: "error", message: `uncaught exception: ${errorMessage(error)}` });
      return;
    }
    emit({ v: 1, t: "fatal", code: "internal", message: errorMessage(error) });
    shutdown(1);
  };

  const onRejection = (reason: unknown): void => {
    log("error", "unhandled rejection", reason);
    emit({ v: 1, t: "log", level: "error", message: `unhandled rejection: ${errorMessage(reason)}` });
    shutdown(1);
  };

  process.on("uncaughtException", onUncaught);
  process.on("unhandledRejection", onRejection);

  return {
    handOverToHost(): void {
      hostOwnsErrors = true;
      // 修复说明：Electron utilityProcess 中未处理的 rejection 按 Node 默认行为转为
      // uncaughtException(origin=unhandledRejection)，交给 Host 处理器。保留 launcher 的
      // unhandledRejection 监听会吞掉这一转换，因此 Host 加载后移除它以保持同等语义。
      process.off("unhandledRejection", onRejection);
    }
  };
}
