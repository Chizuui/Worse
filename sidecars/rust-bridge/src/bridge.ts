import { rustHostEnv, type BridgeConfig } from "./config.ts";
import { createInterceptingPort, type PortLike } from "./interceptingPort.ts";
import type { BridgeLogger } from "./logger.ts";
import { createRustHostSupervisor, type RustHostSupervisor } from "./process/supervisor.ts";
import { createAttachmentRouter, type RouterIo } from "./routing/attachmentRouter.ts";
import { RUST_SERVICES } from "./routing/routeTable.ts";
import { isDesktopContinuousLocalHostMessage, isLocalScopeHostMessage } from "./routing/scope.ts";
import { createBridgeStats, type BridgeStats } from "./routing/stats.ts";
import { attachmentServices } from "./routing/storeScope.ts";
import { createHostCaller } from "./sessions/hostCall.ts";
import { createPtyRouter, createPtySessionStore } from "./sessions/ptyRouter.ts";
import { createSessionDirectory, type SessionDirectory } from "./sessions/sessionDirectory.ts";
import type { SessionDeps, SessionRouter } from "./sessions/types.ts";
import { createWatchRouter } from "./sessions/watchRouter.ts";

export interface RustBridge {
  /** Port the Host should see for this attachment (the original port when not routable). */
  decoratePort<P extends PortLike>(port: P, hostMessage: Record<string, unknown>): PortLike | P;
  readonly stats: BridgeStats;
  readonly supervisor: RustHostSupervisor;
  /** Live Rust watch/pty sessions (diagnostics, gates). */
  readonly sessions: SessionDirectory;
  /** Graceful stop of harness-host (stdin EOF, 2 s grace, kill). */
  dispose(): Promise<void>;
  /** Synchronous kill for `process.on("exit")`. */
  killNow(): void;
}

/**
 * Process-level bridge: one supervisor (one harness-host) shared by every local
 * attachment of this host-node process. With no service flag on it is inert:
 * ports are returned unchanged and nothing is spawned.
 */
export function createRustBridge(config: BridgeConfig, logger: BridgeLogger): RustBridge {
  const stats = createBridgeStats();
  const supervisor = createRustHostSupervisor({
    binaryPath: config.binaryPath,
    cwd: config.cwd,
    env: rustHostEnv(process.env, config.conptyDir),
    logger
  });
  const directory = createSessionDirectory();
  const sessionDeps: SessionDeps = { supervisor, directory, stats, logger, trace: config.trace };
  const ptyStore = createPtySessionStore(sessionDeps);
  if (config.unknownFlags.length > 0) {
    logger("warn", `HARNESS_RUST_SERVICES: ignoring unknown entries ${config.unknownFlags.join(",")}`);
  }
  const enabled = RUST_SERVICES.filter((service) => config.services.has(service));
  logger(
    "info",
    enabled.length === 0 ? "rust services off (TS Host serves everything)" : `rust services on: ${enabled.join(",")} via ${config.binaryPath}`
  );
  if (enabled.includes("pty") && process.platform === "win32") {
    logger(
      "info",
      config.conptyDir === null ? "pty: harness-host uses the conpty.dll next to its binary, else the inbox ConPTY" : `pty: ConPTY sideloaded from ${config.conptyDir}`
    );
  }

  const createRouter = (hostMessage: Record<string, unknown>, io: RouterIo) => {
    const hostCaller = createHostCaller(io.injectToHost);
    const sessionIo = { reply: io.reply, forward: io.forward };
    const sessionRouters: SessionRouter[] = [];
    if (config.services.has("watch")) sessionRouters.push(createWatchRouter(sessionDeps, sessionIo));
    if (config.services.has("pty") && isDesktopContinuousLocalHostMessage(hostMessage)) {
      sessionRouters.push(createPtyRouter({ ...sessionDeps, store: ptyStore, hostCaller }, sessionIo));
    }
    // store 只服务桌面本地渲染器：web-remote 附件的 project 服务带活动装饰，Rust 不复现（规格 S4）。
    const services = attachmentServices(config.services, hostMessage);
    const router = createAttachmentRouter(
      { supervisor, services, stats, logger, trace: config.trace, sessionRouters, handleOutbound: hostCaller.handleOutbound },
      io
    );
    return {
      handleInbound: router.handleInbound,
      handleOutbound: router.handleOutbound,
      dispose() {
        router.dispose();
        hostCaller.dispose();
      }
    };
  };

  const decoratePort = <P extends PortLike>(port: P, hostMessage: Record<string, unknown>): PortLike | P => {
    if (config.services.size === 0 || !isLocalScopeHostMessage(hostMessage)) return port;
    return createInterceptingPort(port, (io) => createRouter(hostMessage, io), logger);
  };

  return {
    decoratePort,
    stats,
    supervisor,
    sessions: directory,
    dispose: () => supervisor.dispose(),
    killNow: () => supervisor.killNow()
  };
}
