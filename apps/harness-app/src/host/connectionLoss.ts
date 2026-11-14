import { logger } from "@/logger.js";
import type { HostStatus } from "../generated/HostStatus.js";
import { hostStatus, onHostState, type HostCommandFailure } from "./bridge.js";
import type { HostMessagePort } from "./messagePort.js";

const HOST_FAILED_MESSAGE = "The local Host stopped unexpectedly.";

function lossFromStatus(status: HostStatus): HostCommandFailure {
  return {
    code: status.errorCode ?? "hostFailed",
    message: status.errorMessage ?? HOST_FAILED_MESSAGE,
  };
}

/**
 * Watches an admitted Host attachment and reports when it is lost after boot.
 *
 * 修复说明：Root 进入后，Rust 发出的 `harness://host/state` failed 与附着端口关闭都被忽略，
 * Host 崩溃时界面静默失效。这里在启动完成后订阅两类信号：
 * - Host 状态 `failed`（仅在非主动关闭时由 Rust 发出）直接上报；
 * - 端口关闭后读取一次 `host_status`：`stopped` 表示壳层正在主动关闭（关闭窗口/退出），
 *   不上报；其余状态（`failed`，或 Host 仍存活但附着断开）都上报，交由界面提供重连入口。
 * 依据事件顺序判断而不是用延时掩盖：Rust 在下发 dispose 之前就把槽位置为 Stopped。
 *
 * `onLost` may run more than once (for example a generic close followed by the
 * failed state with its error code); the latest report wins.
 */
export function watchHostConnection(
  port: HostMessagePort,
  onLost: (loss: HostCommandFailure) => void,
): () => void {
  let disposed = false;
  let unlistenState: (() => void) | null = null;

  const report = (loss: HostCommandFailure): void => {
    if (!disposed) {
      logger.warn("lost the local Host attachment", { code: loss.code });
      onLost(loss);
    }
  };

  onHostState((status) => {
    if (status.state === "failed") {
      report(lossFromStatus(status));
    }
  })
    .then((unlisten) => {
      if (disposed) {
        unlisten();
      } else {
        unlistenState = unlisten;
      }
    })
    .catch((error: unknown) => logger.warn("failed to subscribe to the Host state", { error }));

  void port.closed.then(async (reason) => {
    if (disposed) {
      return;
    }
    const status = await hostStatus().catch(() => null);
    if (status?.state === "stopped") {
      logger.info("Host attachment closed during shell shutdown");
      return;
    }
    report(
      status?.state === "failed"
        ? lossFromStatus(status)
        : { code: "connectionClosed", message: reason.message },
    );
  });

  return () => {
    disposed = true;
    unlistenState?.();
    unlistenState = null;
  };
}
