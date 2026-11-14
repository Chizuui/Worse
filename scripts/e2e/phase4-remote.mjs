#!/usr/bin/env node
// Phase 4 remote-workspace gate (spec docs/specs/harness/phase-4-remote-workspaces.md, section 9).
//
// Runs the real ZCode Host inside the real host-node launcher with every Rust service enabled, and
// drives remote workspaces exactly as the Harness renderer does: `rw.req` control frames on the
// window (init-local) attachment, `scoped-service-port` events answered by attaching the minted
// WebSocket grant, and the ZCode RPC services over that scoped attachment.
//
// Backends:
//   WSL    — end to end against an installed distro (PHASE4_WSL_DISTRO, default "Ubuntu"):
//            connect -> bind a temp git workspace -> file.readdir, git summary/changes, terminal I/O
//            -> dispose. The only state left in the distro is ZCode's remote runtime under the
//            distro user's home (reported; PHASE4_REMOTE_CLEAN_RUNTIME=1 removes new entries).
//   Docker — only when `isDockerAvailable` is true; otherwise SKIP with the reason.
//   SSH    — only a localhost sshd with gate-provided credentials; never the user's ~/.ssh config.
//            Without both, SKIP with the reason.
// Also asserts that remote calls never reach the Rust service bridge (zero `rust-bridge route`
// trace lines during the remote phase, while a local control call does produce one).
//
// Prints PHASE4_REMOTE_OK plus a JSON summary, or PHASE4_REMOTE_FAIL <step>.
import { spawn } from "node:child_process";
import { randomBytes, randomUUID } from "node:crypto";
import { existsSync } from "node:fs";
import { mkdir, rm } from "node:fs/promises";
import net from "node:net";
import path from "node:path";
import process from "node:process";
import { loadModules, paths, repoRoot, verifyArtifacts } from "./phase2/environment.mjs";
import { GateError, delay, fail, withTimeout } from "./phase2/gate.mjs";
import { connectRpc } from "./phase2/rpcClient.mjs";
import { ALLOWED_ORIGIN, SHUTDOWN_TIMEOUT_MS, awaitReady, buildInitLocalMessage, openAttachment } from "./phase2/scenarios.mjs";
import { startSidecar } from "./phase2/sidecarProcess.mjs";
import { attachSocket } from "./phase2/wsClient.mjs";
import { openTerminal, stripAnsi, waitQuiet } from "./phase3b/terminal.mjs";
import { createControlChannel } from "./phase4-remote/controlChannel.mjs";
import { installIsolatedHome } from "./lib/isolatedHome.mjs";

// 修复说明：Host/Agent 按 HOME/USERPROFILE 解析 ~/.zcode，曾把测试数据写进用户真实设置；
// 在任何子进程启动前切到一次性 home，并在退出时检查真实 ~/.zcode/v2 文件未被改动（HOME_LEAK）。
installIsolatedHome("phase4-remote");

const runDir = path.join(repoRoot, "Worse/.work/tmp/phase4-remote");
const dataDir = path.join(runDir, "data");
// ZCode 的 setting.json 固定写在 HOME/.zcode/v2 下（不认 ZCODE_DATA_BASE_DIR）；家目录隔离由
// lib/isolatedHome.mjs 统一完成（此前只设 HOME，Windows 上 os.homedir() 仍读真实 USERPROFILE，
// 例如 ~/.ssh/config 发现会读用户真实目录）。
const remoteVendor = path.join(repoRoot, "Worse/sidecars/host-node/dist-vendor/remoteWorkspace.js");
const exe = process.platform === "win32" ? ".exe" : "";
const RUST_HOST = process.env.HARNESS_RUST_HOST
  ? path.resolve(process.env.HARNESS_RUST_HOST)
  : path.join(repoRoot, `Worse/target/debug/harness-host${exe}`);
const WSL_DISTRO = process.env.PHASE4_WSL_DISTRO ?? "Ubuntu";
const TRACE_PATTERN = /rust-bridge route (\S+) (rust|surfaced-error|fallback|cancelled)/;

const DISCOVERY_TIMEOUT_MS = 60_000;
// 首次连接需要从 CDN 下载并部署远端运行时，可能耗时数分钟。
const CONNECT_TIMEOUT_MS = Number(process.env.PHASE4_CONNECT_TIMEOUT_MS ?? 15 * 60_000);
const BIND_TIMEOUT_MS = 120_000;
const CALL_TIMEOUT_MS = 60_000;
const PORT_EVENT_TIMEOUT_MS = 60_000;
const CLOSE_TIMEOUT_MS = 30_000;

const startTime = Date.now();

function run(command, args, { input, timeoutMs = 120_000 } = {}) {
  return new Promise((resolve) => {
    const child = spawn(command, args, { windowsHide: true, stdio: ["pipe", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    const timer = setTimeout(() => child.kill(), timeoutMs);
    child.stdout.on("data", (chunk) => (stdout += chunk.toString("utf8")));
    child.stderr.on("data", (chunk) => (stderr += chunk.toString("utf8")));
    child.on("error", (error) => {
      clearTimeout(timer);
      resolve({ code: -1, stdout, stderr: error.message });
    });
    child.on("exit", (code) => {
      clearTimeout(timer);
      resolve({ code, stdout, stderr });
    });
    if (input !== undefined) child.stdin.end(input);
    else child.stdin.end();
  });
}

/** Runs a POSIX script inside the WSL distro (fed on stdin so no Windows quoting is involved). */
function wslSh(script, timeoutMs) {
  return run("wsl.exe", ["-d", WSL_DISTRO, "--", "sh", "-s"], { input: script, timeoutMs });
}

function canConnect(port, host = "127.0.0.1") {
  return new Promise((resolve) => {
    const socket = net.connect({ port, host });
    const done = (ok) => {
      socket.destroy();
      resolve(ok);
    };
    socket.setTimeout(2_000, () => done(false));
    socket.once("connect", () => done(true));
    socket.once("error", () => done(false));
  });
}

function collectTrace(child) {
  const entries = [];
  let remainder = "";
  child.stderr.on("data", (chunk) => {
    const parts = (remainder + chunk.toString()).split(/\r?\n/);
    remainder = parts.pop() ?? "";
    for (const line of parts) {
      const match = TRACE_PATTERN.exec(line);
      if (match) entries.push({ method: match[1], outcome: match[2] });
    }
  });
  return entries;
}

async function removeRunDir() {
  for (let attempt = 0; attempt < 6; attempt += 1) {
    try {
      await rm(runDir, { recursive: true, force: true });
      return true;
    } catch {
      await delay(250 * (attempt + 1));
    }
  }
  return false;
}

/**
 * Scoped-port handling as the Harness renderer does it: every `scoped-service-port` event is
 * attached with its minted token, confirmed with `scopedPortReady`, then connected over RPC.
 */
function createScopedPortTracker({ modules, url, control }) {
  const attachments = [];
  const waiters = new Set();
  const settle = () => {
    for (const waiter of Array.from(waiters)) {
      const found = attachments.find(waiter.predicate);
      if (!found) continue;
      waiters.delete(waiter);
      clearTimeout(waiter.timer);
      waiter.resolve(found);
    }
  };
  control.onEvent((frame) => {
    if (frame.event !== "scoped-service-port") return;
    const { payload, token } = frame.payload ?? {};
    const entry = { payload, ready: null, error: null, closed: false };
    attachments.push(entry);
    entry.ready = (async () => {
      const step = "SCOPED_ATTACH";
      const port = await attachSocket(modules.WebSocket, url, ALLOWED_ORIGIN, { attachmentId: payload.attachmentId, token }, step);
      port.ws.on("close", () => {
        entry.closed = true;
      });
      entry.port = port;
      // 与 ZCode renderer 一致：先登记 services 再确认 ready，不等待 Initialize。
      await control.request("scopedPortReady", { sessionId: payload.sessionId, attachmentId: payload.attachmentId }, CALL_TIMEOUT_MS, step);
      const connection = await connectRpc(modules.rpc, port, `${step}_INITIALIZE`);
      entry.connection = connection;
      return entry;
    })().catch((error) => {
      entry.error = error;
      throw error;
    });
    entry.ready.catch(() => {});
    settle();
  });
  return {
    attachments,
    waitFor(predicate, step, label) {
      const found = attachments.find(predicate);
      if (found) return found.ready;
      return new Promise((resolve, reject) => {
        const waiter = {
          predicate,
          resolve: (entry) => resolve(entry.ready),
          timer: setTimeout(() => {
            waiters.delete(waiter);
            reject(new GateError(step, `timed out waiting for scoped-service-port (${label})`));
          }, PORT_EVENT_TIMEOUT_MS),
        };
        waiters.add(waiter);
      });
    },
  };
}

async function waitForClosed(entry, step) {
  const deadline = Date.now() + CLOSE_TIMEOUT_MS;
  while (!entry.closed) {
    if (Date.now() > deadline) fail(step, "scoped attachment socket did not close");
    await delay(50);
  }
}

async function callRpc(client, channel, method, params, step) {
  return withTimeout(client.getChannel(channel).call(method, [params]), CALL_TIMEOUT_MS, step, `${channel}.${method}`);
}

/** WSL end to end; returns the backend summary. */
async function runWsl({ control, tracker, trace }) {
  const distros = await control.request("listWSLDistros", undefined, DISCOVERY_TIMEOUT_MS, "WSL_DISCOVER");
  const names = Array.isArray(distros) ? distros.map((entry) => entry?.name).filter(Boolean) : [];
  if (!names.includes(WSL_DISTRO)) {
    return { status: "SKIP", reason: `WSL distro "${WSL_DISTRO}" is not installed (found ${names.length} distros)` };
  }

  const remoteDir = `/tmp/harness-phase4-${randomBytes(6).toString("hex")}`;
  const homeBefore = await wslSh('ls -A "$HOME"\n', 120_000);
  if (homeBefore.code !== 0) fail("WSL_SEED", `cannot list the distro home: ${homeBefore.stderr.trim()}`);
  const seed = await wslSh(
    [
      "set -e",
      `d='${remoteDir}'`,
      'mkdir -p "$d/src"',
      'cd "$d"',
      "printf 'hello\\n' > a.txt",
      "printf 'b\\n' > src/b.txt",
      "git init -q",
      "git add a.txt src/b.txt",
      "git -c user.email=gate@harness.test -c user.name=gate commit -q -m init",
      "printf 'changed\\n' >> a.txt",
      "printf 'new\\n' > untracked-marker.txt",
      "",
    ].join("\n"),
    120_000,
  );
  if (seed.code !== 0) fail("WSL_SEED", `seeding ${remoteDir} failed: ${seed.stderr.trim()}`);

  const summary = { status: "PASS", distro: WSL_DISTRO, remoteDir, timings: {} };
  const traceBefore = trace.length;
  let sessionId;
  try {
    // 1. Connect (as the remote dialog does; the install confirmation mirrors the user's consent).
    const target = { kind: "wsl", distro: WSL_DISTRO };
    let startedAt = Date.now();
    let connected = await control.request("connectRemote", { target, requestId: randomUUID() }, CONNECT_TIMEOUT_MS, "WSL_CONNECT");
    if (
      connected?.success === false &&
      (connected.reasonCode === "source-server-install-required" || connected.reasonCode === "source-server-upgrade-required")
    ) {
      summary.confirmedSourceServerInstall = connected.reasonCode;
      connected = await control.request(
        "connectRemote",
        { target, requestId: randomUUID(), confirmSourceServerInstall: true },
        CONNECT_TIMEOUT_MS,
        "WSL_CONNECT",
      );
    }
    if (connected?.success !== true || typeof connected.sessionId !== "string") {
      fail("WSL_CONNECT", `connectRemote failed: ${String(connected?.error)} (reasonCode ${String(connected?.reasonCode)})`);
    }
    sessionId = connected.sessionId;
    summary.timings.connectMs = Date.now() - startedAt;
    summary.connectionLogEvents = control.events.filter((frame) => frame.event === "remote-connection-log").length;

    // 2. Bind the canonical workspace; the manager re-issues a scoped port for it.
    startedAt = Date.now();
    const bound = await control.request(
      "bindRemoteWorkspaceSessionContext",
      { remoteSessionId: sessionId, workspacePath: remoteDir, workspacePurpose: "project" },
      BIND_TIMEOUT_MS,
      "WSL_BIND",
    );
    if (bound?.success !== true) fail("WSL_BIND", `bind failed: ${String(bound?.error)} (reasonCode ${String(bound?.reasonCode)})`);
    summary.timings.bindMs = Date.now() - startedAt;
    const scoped = await tracker.waitFor(
      (entry) => entry.payload?.sessionId === sessionId && entry.payload?.workspacePath === remoteDir,
      "WSL_SCOPED_PORT",
      remoteDir,
    );
    const identity = scoped.payload.workspaceIdentity;
    if (typeof identity !== "string" || identity.trim() === "" || identity === remoteDir) {
      fail("WSL_IDENTITY", `scoped port carried workspaceIdentity ${JSON.stringify(identity)}; expected a remote identity`);
    }
    if (scoped.payload.target?.kind !== "wsl") fail("WSL_IDENTITY", "scoped port target is not wsl");
    summary.workspaceIdentityKind = identity.split(":")[0];
    const client = scoped.connection.client;

    // 3. File list from inside the distro (the directory does not exist on Windows).
    startedAt = Date.now();
    const entries = await callRpc(client, "file", "readdir", { path: remoteDir, includeHidden: true }, "WSL_FILE");
    const entryNames = Array.isArray(entries) ? entries.map((entry) => entry?.name) : [];
    for (const expected of ["a.txt", "src", "untracked-marker.txt", ".git"]) {
      if (!entryNames.includes(expected)) fail("WSL_FILE", `readdir is missing ${expected}: ${JSON.stringify(entryNames)}`);
    }
    summary.timings.readdirMs = Date.now() - startedAt;
    summary.readdir = entryNames.sort();

    // 4. Git status of the remote repository.
    startedAt = Date.now();
    const repo = await callRpc(client, "git", "getRepositorySummary", { workspacePath: remoteDir }, "WSL_GIT");
    if (repo?.repoRoot !== remoteDir) fail("WSL_GIT", `repoRoot ${String(repo?.repoRoot)} != ${remoteDir}`);
    const changes = await callRpc(client, "git", "getChanges", { workspacePath: remoteDir, sourceId: "unstaged" }, "WSL_GIT");
    const changed = Array.isArray(changes) ? changes.map((change) => change?.path) : [];
    if (!changed.some((entry) => entry === "a.txt" || entry === `${remoteDir}/a.txt`)) {
      fail("WSL_GIT", `unstaged changes lack a.txt: ${JSON.stringify(changed)}`);
    }
    summary.timings.gitMs = Date.now() - startedAt;
    summary.git = { branchName: repo.branchName, unstaged: changed.sort() };

    // 5. Terminal: a shell in the distro, cwd = the remote workspace.
    startedAt = Date.now();
    const term = await openTerminal(client, { cols: 100, rows: 30, cwd: remoteDir });
    try {
      if (!(await waitQuiet(term))) fail("WSL_TERMINAL", "remote shell produced no prompt");
      const marker = `HX${randomBytes(3).toString("hex")}`;
      const seen = new Promise((resolve) => {
        const waiter = () => {
          const text = stripAnsi(term.raw);
          if (text.includes(`${marker}Linux:${remoteDir}`)) {
            term.dataWaiters.delete(waiter);
            resolve(true);
          }
        };
        term.dataWaiters.add(waiter);
      });
      await term.run(`printf '%s%s:%s\\n' '${marker}' "$(uname -s)" "$(pwd)"`);
      await withTimeout(seen, 20_000, "WSL_TERMINAL", "terminal output from the distro");
      summary.terminal = { shell: term.created.shell, spawnMs: Math.round(term.spawnMs) };
    } finally {
      term.unsubscribe();
      await term.dispose().catch(() => undefined);
    }
    summary.timings.terminalMs = Date.now() - startedAt;

    // 6. Fail closed: a scoped attachment cannot drive the control channel.
    const scopedControl = createControlChannel(scoped.port.ws);
    const rejected = await scopedControl
      .request("listWSLDistros", undefined, CALL_TIMEOUT_MS, "WSL_FAIL_CLOSED")
      .then(() => false, () => true);
    if (!rejected) fail("WSL_FAIL_CLOSED", "a remote-scoped attachment was allowed to send rw.req");

    // 7. Remote traffic never touched the Rust service bridge.
    const remoteTrace = trace.slice(traceBefore);
    if (remoteTrace.length > 0) {
      fail("WSL_RUST_BYPASS", `remote calls were routed through Rust: ${JSON.stringify(remoteTrace.slice(0, 5))}`);
    }
    summary.rustRoutesDuringRemote = 0;

    // 8. Dispose (renderer-initiated, so no remote-session-closed event, as in ZCode): the Host
    //    detaches the scoped attachment and its socket closes.
    await control.request("disposeRemoteSession", { sessionId }, CALL_TIMEOUT_MS, "WSL_DISPOSE");
    for (const entry of tracker.attachments.filter((item) => item.payload?.sessionId === sessionId && item.port)) {
      await waitForClosed(entry, "WSL_DISPOSE");
    }
    sessionId = undefined;
  } finally {
    if (sessionId) await control.request("disposeRemoteSession", { sessionId }, CALL_TIMEOUT_MS, "WSL_CLEANUP").catch(() => undefined);
    for (const entry of tracker.attachments) entry.connection?.dispose();
    const cleanup = await wslSh(`rm -rf '${remoteDir}'\n`, 60_000);
    summary.tempDirRemoved = cleanup.code === 0;
    const homeAfter = await wslSh('ls -A "$HOME"\n', 60_000);
    const before = new Set(homeBefore.stdout.split(/\r?\n/).filter(Boolean));
    const added = homeAfter.stdout.split(/\r?\n/).filter((name) => name && !before.has(name));
    summary.distroHomeAdded = added;
    if (process.env.PHASE4_REMOTE_CLEAN_RUNTIME === "1" && added.length > 0) {
      const quoted = added.map((name) => `"$HOME/${name.replace(/["\\$`]/g, "")}"`).join(" ");
      await wslSh(`rm -rf ${quoted}\n`, 120_000);
      summary.distroHomeCleaned = true;
    }
  }
  return summary;
}

async function main() {
  verifyArtifacts();
  if (!existsSync(remoteVendor)) {
    fail("BUILD_VENDOR", "Worse/sidecars/host-node/dist-vendor/remoteWorkspace.js is missing", "pnpm --dir Worse/sidecars/host-node build");
  }
  if (!existsSync(RUST_HOST)) fail("BUILD_RUST", `${RUST_HOST} is missing`, "cargo build -p harness-host");
  const modules = await loadModules();
  await removeRunDir();
  await mkdir(dataDir, { recursive: true });

  const sidecar = startSidecar({
    launcher: paths.launcher,
    cwd: repoRoot,
    env: {
      HARNESS_HOST_ENTRY: paths.hostEntry,
      HARNESS_ALLOWED_ORIGINS: ALLOWED_ORIGIN,
      ZCODE_ENV: "production",
      ZCODE_DATA_BASE_DIR: dataDir,
      ZCODE_BUILTIN_PROVIDER_CONFIG_FILE: paths.providerConfig,
      HARNESS_RUST_SERVICES: "all",
      HARNESS_RUST_HOST: RUST_HOST,
      HARNESS_RUST_BRIDGE_TRACE: "1",
    },
  });
  const trace = collectTrace(sidecar.child);
  const result = { backends: {} };
  try {
    const ready = await awaitReady(sidecar);
    const url = `ws://127.0.0.1:${ready.wsPort}/v1/attach`;

    // Window attachment (init-local), its RPC client and the control channel on the same socket.
    const grant = await openAttachment(sidecar, buildInitLocalMessage({ repoRoot, providerConfig: paths.providerConfig }), "BASE_OPEN");
    const basePort = await attachSocket(modules.WebSocket, url, ALLOWED_ORIGIN, grant, "BASE_ATTACH");
    const control = createControlChannel(basePort.ws);
    const base = await connectRpc(modules.rpc, basePort, "BASE_INITIALIZE");

    // Local control: a local file call is served by Rust (proves the trace is live).
    await callRpc(base.client, "file", "readdir", { path: repoRoot }, "LOCAL_CONTROL");
    // stderr 与 RPC 应答走不同管道，trace 行可能晚于应答到达：有界等待它出现。
    const traceDeadline = Date.now() + 5_000;
    while (!trace.some((entry) => entry.method === "file.readdir") && Date.now() < traceDeadline) await delay(25);
    const localRoutes = trace.filter((entry) => entry.method === "file.readdir").length;
    if (localRoutes === 0) fail("LOCAL_CONTROL", "local file.readdir produced no rust-bridge trace line");
    result.localRustRoutes = localRoutes;

    // Discovery (counts only: SSH aliases may name private hosts).
    const docker = await control.request("isDockerAvailable", undefined, DISCOVERY_TIMEOUT_MS, "DISCOVER");
    const sshAliases = await control.request("listSSHConfigAliases", undefined, DISCOVERY_TIMEOUT_MS, "DISCOVER");
    result.discovery = {
      dockerAvailable: docker === true,
      sshConfigAliasCount: Array.isArray(sshAliases) ? sshAliases.length : null,
    };

    const tracker = createScopedPortTracker({ modules, url, control });

    result.backends.wsl =
      process.platform === "win32"
        ? await runWsl({ control, tracker, trace })
        : { status: "SKIP", reason: "WSL is Windows-only" };

    result.backends.docker =
      docker === true
        ? { status: "SKIP", reason: "docker is available but the gate creates no containers without PHASE4_DOCKER_CONTAINER" }
        : { status: "SKIP", reason: "isDockerAvailable=false (docker CLI/daemon not available on this machine)" };
    if (docker === true && process.env.PHASE4_DOCKER_CONTAINER) {
      result.backends.docker = { status: "SKIP", reason: "PHASE4_DOCKER_CONTAINER scenario not implemented in this gate" };
    }

    const sshd = await canConnect(22);
    result.backends.ssh = !sshd
      ? { status: "SKIP", reason: "no sshd listening on 127.0.0.1:22 (not installed; the gate never uses ~/.ssh or remote hosts)" }
      : { status: "SKIP", reason: "localhost sshd found but no gate-provided test credentials; the user's keys are never used" };

    // Shutdown.
    base.dispose();
    basePort.close();
    sidecar.child.stdin.end();
    const exit = await sidecar.waitForExit(SHUTDOWN_TIMEOUT_MS, "SHUTDOWN");
    result.exitCode = exit.code;
  } catch (error) {
    const tail = sidecar.stderrTail(60);
    if (tail) process.stderr.write(`sidecar stderr (tail):\n${tail}\n`);
    sidecar.kill();
    throw error;
  }

  const failed = Object.entries(result.backends).filter(([, backend]) => backend.status === "FAIL");
  if (failed.length > 0) fail("BACKENDS", `failed backends: ${failed.map(([name]) => name).join(", ")}`);
  if (!Object.values(result.backends).some((backend) => backend.status === "PASS")) {
    fail("BACKENDS", "no remote backend was available to exercise");
  }
  result.runDirRemoved = await removeRunDir();
  result.elapsedMs = Date.now() - startTime;
  process.stdout.write(`PHASE4_REMOTE_OK\n${JSON.stringify(result, null, 2)}\n`);
}

main().then(
  () => process.exit(0),
  (error) => {
    const step = error instanceof GateError ? error.step : "INTERNAL";
    process.stdout.write(`PHASE4_REMOTE_FAIL ${step}\n${error instanceof Error ? error.message : String(error)}\n`);
    if (error instanceof GateError && error.command) process.stdout.write(`fix: ${error.command}\n`);
    process.exit(1);
  },
);
