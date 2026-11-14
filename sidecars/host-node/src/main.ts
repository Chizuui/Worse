import { existsSync } from "node:fs";
import { createAttachBroker } from "./attachBroker.ts";
import { composeBrokerHooks, createBrowserOwner } from "./browserOwner.ts";
import { importHostEntry } from "./hostEntry.ts";
import { readLines } from "./lineReader.ts";
import { log } from "./log.ts";
import { installGracefulExit } from "./gracefulExit.ts";
import { createParentPortShim, installParentPort } from "./parentPort.ts";
import { SANDBOX_CALL, installPluginSandbox } from "./pluginSandbox.ts";
import { installRemoteWorkspaces } from "./remoteWorkspace.ts";
import { createRemoteTargetDiscovery } from "./remoteTargets.ts";
import { createRemoteWorkspaceWebRemoteBridge } from "./remoteWorkspaceWebRemote.ts";
import { installProcessGuards } from "./processGuards.ts";
import { createRustServices } from "./rustServices.ts";
import {
  MAX_LINE_BYTES,
  SidecarProtocolError,
  parseShellLine,
  serializeSidecarLine,
  type SidecarErrorCode,
  type SidecarLine
} from "./protocol.ts";
import { createAttachmentServer } from "./wsServer.ts";
import { WEB_REMOTE_MESSAGE_PREFIX, createWebRemoteHost } from "./webRemoteHost.ts";

const DEFAULT_SHUTDOWN_GRACE_MS = 3_000;
const EXIT_DRAIN_TIMEOUT_MS = 1_500;
const EXIT_STARTUP_FAILURE = 2;

function parseGraceMs(raw: string | undefined): number {
  if (raw === undefined) return DEFAULT_SHUTDOWN_GRACE_MS;
  const value = Number(raw);
  return Number.isFinite(value) && value >= 0 ? Math.floor(value) : DEFAULT_SHUTDOWN_GRACE_MS;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

// Step 1: validate the environment before touching stdout.
const hostEntry = process.env.HARNESS_HOST_ENTRY ?? "";
if (hostEntry === "" || !existsSync(hostEntry)) {
  log("error", hostEntry === "" ? "HARNESS_HOST_ENTRY is required" : `HARNESS_HOST_ENTRY does not exist: ${hostEntry}`);
  process.exit(EXIT_STARTUP_FAILURE);
}

// Step 2: stdout is the NDJSON control pipe; everything else written to it goes to stderr.
type StdoutWrite = typeof process.stdout.write;
const realWrite: StdoutWrite = process.stdout.write.bind(process.stdout);
process.stdout.write = process.stderr.write.bind(process.stderr) as StdoutWrite;

const writeLine = (text: string): void => {
  realWrite(`${text}\n`);
};
const emit = (line: SidecarLine): void => writeLine(serializeSidecarLine(line));

// Step 3: the Host checks `process.parentPort` at module load, so install it first.
const shim = createParentPortShim(writeLine);
installParentPort(shim);

// Phase 4: remote workspaces (Electron Main's remote session manager for this window).
const remoteWorkspaces = installRemoteWorkspaces({ shim, env: process.env, cwd: process.cwd() });

// Phase 5 (browser): Main's in-app browser owner; answers browser-execute-request with a CDP-driven Chromium.
const browser = createBrowserOwner();
browser.install(shim);

// Phase 5 (plugins): Main's plugin UI sandbox owner; answers plugin-sandbox-register-request and serves hsandbox://.
const pluginSandbox = installPluginSandbox({ shim, postToShell: (message) => emit({ v: 1, t: "host", message }), hostEntry });

// Phase 4: ZCode web remote control (Electron Main's manager) runs beside the Host it attaches to.
const webRemote = createWebRemoteHost({
  postToHost: (message, ports) => shim.emitHostMessage(message, ports),
  postToShell: (message) => emit({ v: 1, t: "host", message }),
  env: process.env,
  // 手机控制远程 workspace：复用本窗口已有远程会话；Docker/WSL/SSH 列表在桌面系统上探测。
  remoteWorkspaces: createRemoteWorkspaceWebRemoteBridge(remoteWorkspaces),
  remoteTargets: createRemoteTargetDiscovery(),
  remoteWorkspaceStats: () => remoteWorkspaces.stats()
});

let shuttingDown = false;
let closeServer: () => Promise<void> = async () => {};

// Phase 3a: Rust file/search/git services behind the RPC port (inert unless HARNESS_RUST_SERVICES is set).
const rustServices = createRustServices(process.env, process.cwd());
// harness-host must not outlive the launcher, whichever exit path is taken.
process.on("exit", () => rustServices.killNow());

let launcherCleanup: Promise<void> | null = null;
const cleanupLauncher = (): Promise<void> => {
  launcherCleanup ??= Promise.all([
    closeServer().catch((error: unknown) => log("warn", "failed to close the websocket server", error)),
    rustServices.dispose().catch((error: unknown) => log("warn", "failed to stop harness-host", error)),
    browser.dispose().catch((error: unknown) => log("warn", "failed to stop the browser owner", error))
  ]).then(() => undefined);
  return launcherCleanup;
};

// Installed before the Host loads: the Host calls process.exit itself once its dispose completes.
const exits = installGracefulExit({ drainTimeoutMs: EXIT_DRAIN_TIMEOUT_MS, cleanup: cleanupLauncher });

const shutdown = (exitCode: number): void => {
  if (shuttingDown) return;
  shuttingDown = true;
  process.exitCode = exitCode;
  void webRemote.dispose();
  pluginSandbox.dispose();
  void remoteWorkspaces.dispose();
  shim.emitHostMessage({ type: "dispose" }, []);
  void cleanupLauncher();
  // Bounded fallback for a Host whose dispose never requests the exit; it must not keep the loop alive.
  setTimeout(() => exits.hardExit(exitCode), parseGraceMs(process.env.HARNESS_SHUTDOWN_GRACE_MS)).unref();
};

const failFatal = (code: SidecarErrorCode, message: string): void => {
  emit({ v: 1, t: "fatal", code, message });
  log("error", message);
  shutdown(1);
};

const guards = installProcessGuards(emit, shutdown);

async function main(): Promise<void> {
  // Step 4: loopback WebSocket server.
  const server = await createAttachmentServer();
  closeServer = () => server.close();
  const broker = createAttachBroker(
    server,
    shim,
    emit,
    rustServices.decoratePort,
    composeBrokerHooks(remoteWorkspaces.brokerHooks, browser.brokerHooks)
  );
  remoteWorkspaces.attach(server);

  // Step 5: load the Host.
  try {
    await importHostEntry(hostEntry);
  } catch (error) {
    emit({ v: 1, t: "fatal", code: "host_import_failed", message: errorMessage(error) });
    log("error", "failed to import the Host entry", error);
    exits.hardExit(EXIT_STARTUP_FAILURE);
  }
  guards.handOverToHost();

  // Step 6: announce readiness (remote init-local needs the vendored manager loaded first).
  await remoteWorkspaces.ready;
  await browser.ready;
  emit({ v: 1, t: "ready", hostPid: process.pid, wsPort: server.port, protocol: 1 });

  // Step 7: serve stdin until EOF.
  readLines(process.stdin, {
    maxLineBytes: MAX_LINE_BYTES,
    onLine: (line) => {
      let parsed;
      try {
        parsed = parseShellLine(line);
      } catch (error) {
        failFatal(error instanceof SidecarProtocolError ? error.code : "bad_line", errorMessage(error));
        return;
      }
      switch (parsed.t) {
        case "ping":
          emit({ v: 1, t: "pong", id: parsed.id });
          return;
        case "host": {
          const type = parsed.message.type;
          // harness-web-remote-* 是 shell 与本进程远控 owner 的私有通道，绝不转交给 ZCode Host。
          // Phase 5：插件沙箱调用复用 harness-web-remote- 前缀（控制器/分类器无需改动），但由沙箱 owner 处理。
          if (type === SANDBOX_CALL) {
            pluginSandbox.handleShellMessage(parsed.message);
            return;
          }
          if (typeof type === "string" && type.startsWith(WEB_REMOTE_MESSAGE_PREFIX)) {
            webRemote.handleShellMessage(parsed.message);
            return;
          }
          webRemote.observeHostMessage(parsed.message);
          shim.emitHostMessage(parsed.message, []);
          return;
        }
        case "attach.open":
          // init-local 随首个 attachment 下发；远控 owner 只从中读取设备标识（deviceMid），不改动消息。
          webRemote.observeHostMessage(parsed.hostMessage);
          broker.open(parsed);
          return;
      }
    },
    onOverflow: () => failFatal("line_too_large", "stdin line exceeded 16 MiB"),
    onEnd: () => {
      log("info", "stdin closed; shutting down");
      shutdown(0);
    },
    onError: (error) => {
      log("error", "stdin error", error);
      shutdown(1);
    }
  });
}

void main().catch((error: unknown) => {
  emit({ v: 1, t: "fatal", code: "internal", message: errorMessage(error) });
  log("error", "launcher failed to start", error);
  exits.hardExit(EXIT_STARTUP_FAILURE);
});
