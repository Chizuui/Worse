// Phase 4 web remote gate, scenarios W9-W11 (spec phase-4-web-remote-control.md sections 5.5, 5.6, 10):
// the phone's Docker/WSL/SSH listings, the phone on the desktop's WSL workspace through the
// existing remote session, and fail-closed after the desktop disconnects that session.
//
// The desktop side is driven exactly as the Harness renderer drives it (`rw.req` on the window
// attachment, `scoped-service-port` claimed with its minted token, `scopedPortReady`); the phone
// side uses ZCode's real acknowledged relay transport + ChannelClient (phone.mjs).
import { spawn } from "node:child_process";
import { randomBytes, randomUUID } from "node:crypto";
import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { GateError, fail, withTimeout } from "../phase2/gate.mjs";
import { connectRpc } from "../phase2/rpcClient.mjs";
import { ALLOWED_ORIGIN } from "../phase2/scenarios.mjs";
import { attachSocket } from "../phase2/wsClient.mjs";
import { openTerminal, stripAnsi, waitQuiet } from "../phase3b/terminal.mjs";
import { createControlChannel } from "../phase4-remote/controlChannel.mjs";

const WSL_DISTRO = process.env.PHASE4_WSL_DISTRO ?? "Ubuntu";
/** Fake alias written into the gate's isolated ~/.ssh/config; never a real host or key. */
export const FAKE_SSH_ALIAS = "harness-gate-ssh-alias";
const DISCOVERY_TIMEOUT_MS = 60_000;
// 首次连接可能需要部署远端运行时，耗时数分钟。
const CONNECT_TIMEOUT_MS = Number(process.env.PHASE4_CONNECT_TIMEOUT_MS ?? 15 * 60_000);
const BIND_TIMEOUT_MS = 120_000;
const CALL_TIMEOUT_MS = 60_000;
const PORT_EVENT_TIMEOUT_MS = 60_000;
const CLOSE_TIMEOUT_MS = 30_000;

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

/** Runs a POSIX script inside the WSL distro (fed on stdin, no Windows quoting involved). */
function wslSh(script, timeoutMs) {
  return run("wsl.exe", ["-d", WSL_DISTRO, "--", "sh", "-s"], { input: script, timeoutMs });
}

/** Writes the fake SSH config the phone listing must find; only inside the isolated home. */
export async function writeFakeSshConfig(home) {
  const dir = path.join(home, ".ssh");
  await mkdir(dir, { recursive: true });
  const config = [`Host ${FAKE_SSH_ALIAS}`, "  HostName 127.0.0.1", "  Port 2222", "  User harness-gate", ""].join("\n");
  await writeFile(path.join(dir, "config"), config, { mode: 0o600 });
}

/** Phone `platform-request` -> `platform-response` over the relay (the web page's platform service). */
async function phonePlatformResponse(phone, method, step) {
  const requestId = `phase4-platform-${method}-${randomUUID()}`;
  if (!phone.sendData({ zcode_type: "platform-request", requestId, method })) fail(step, "phone could not send platform-request");
  return phone.waitForPayload(
    (payload) => payload.zcode_type === "platform-response" && payload.requestId === requestId,
    step,
    `platform-response ${method}`,
    DISCOVERY_TIMEOUT_MS,
  );
}

async function phonePlatform(phone, method, step) {
  const response = await phonePlatformResponse(phone, method, step);
  if (response.success !== true) fail(step, `${method} failed on the desktop: ${String(response.error)}`);
  return response.result;
}

/** W9: Docker/WSL/SSH listings asked by the phone, answered from the desktop OS. */
export async function runPhoneListings(phone) {
  const step = "W9_LISTINGS";
  const docker = await phonePlatform(phone, "isDockerAvailable", step);
  if (typeof docker !== "boolean") fail(step, `isDockerAvailable returned ${JSON.stringify(docker)}`);
  // 与 ZCode Main 相同：没有 docker CLI 时 listDockerContainers 抛错并以 success:false 回给手机；
  // 只有 Docker 可用时才要求列表成功。
  const containersResponse = await phonePlatformResponse(phone, "listDockerContainers", step);
  if (docker && (containersResponse.success !== true || !Array.isArray(containersResponse.result))) {
    fail(step, `listDockerContainers failed with Docker available: ${String(containersResponse.error)}`);
  }
  if (!docker && containersResponse.success === true && !Array.isArray(containersResponse.result)) {
    fail(step, "listDockerContainers did not return an array");
  }
  const containers = containersResponse.success === true ? containersResponse.result : null;
  const distros = await phonePlatform(phone, "listWSLDistros", step);
  if (!Array.isArray(distros)) fail(step, "listWSLDistros did not return an array");
  const distroNames = distros.map((entry) => entry?.name).filter(Boolean);
  if (process.platform === "win32" && !distroNames.includes(WSL_DISTRO)) {
    fail(step, `listWSLDistros lacks ${WSL_DISTRO}: ${JSON.stringify(distroNames)}`);
  }
  const aliases = await phonePlatform(phone, "listSSHConfigAliases", step);
  if (!Array.isArray(aliases)) fail(step, "listSSHConfigAliases did not return an array");
  const aliasNames = aliases.map((entry) => entry?.alias);
  // 隔离 home 中只有本 gate 写入的假配置，因此结果必须恰好是这一个别名（证明没有读到真实 ~/.ssh）。
  if (aliasNames.length !== 1 || aliasNames[0] !== FAKE_SSH_ALIAS) {
    fail(step, `listSSHConfigAliases returned ${aliasNames.length} aliases, expected only ${FAKE_SSH_ALIAS}`);
  }
  return {
    dockerAvailable: docker,
    dockerContainers: containers === null ? { success: false, error: String(containersResponse.error) } : containers.length,
    wslDistros: distroNames,
    sshAliases: aliasNames,
    sshAliasHost: aliases[0]?.host ?? null,
  };
}

/** Claims `scoped-service-port` events as the Harness renderer does. */
function createScopedPortTracker({ modules, url, control }) {
  const attachments = [];
  control.onEvent((frame) => {
    if (frame.event !== "scoped-service-port") return;
    const { payload, token } = frame.payload ?? {};
    const entry = { payload, closed: false };
    attachments.push(entry);
    entry.ready = (async () => {
      const step = "W10_SCOPED_ATTACH";
      const port = await attachSocket(modules.WebSocket, url, ALLOWED_ORIGIN, { attachmentId: payload.attachmentId, token }, step);
      port.ws.on("close", () => {
        entry.closed = true;
        entry.onClosed?.();
      });
      entry.port = port;
      await control.request("scopedPortReady", { sessionId: payload.sessionId, attachmentId: payload.attachmentId }, CALL_TIMEOUT_MS, step);
      entry.connection = await connectRpc(modules.rpc, port, `${step}_INITIALIZE`);
      return entry;
    })();
    entry.ready.catch(() => {});
  });
  return {
    attachments,
    waitFor(predicate, step, label) {
      const found = attachments.find(predicate);
      if (found) return found.ready;
      // 事件驱动：tracker 的监听先注册，因此本监听运行时新条目已入列。
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          off();
          reject(new GateError(step, `timed out waiting for scoped-service-port (${label})`));
        }, PORT_EVENT_TIMEOUT_MS);
        const off = control.onEvent(() => {
          const hit = attachments.find(predicate);
          if (!hit) return;
          clearTimeout(timer);
          off();
          resolve(hit.ready);
        });
      });
    },
  };
}

function waitClosed(entry, step) {
  if (entry.closed) return Promise.resolve();
  return withTimeout(
    new Promise((resolve) => {
      entry.onClosed = resolve;
    }),
    CLOSE_TIMEOUT_MS,
    step,
    "desktop scoped attachment close",
  );
}

const callRpc = (client, channel, method, params, step) =>
  withTimeout(client.getChannel(channel).call(method, [params]), CALL_TIMEOUT_MS, step, `${channel}.${method}`);

const remoteOpened = (sidecar) =>
  sidecar.lines.filter(
    (line) =>
      line.t === "host" &&
      line.message?.type === "harness-web-remote-event" &&
      line.message.event === "attachment" &&
      line.message.payload?.state === "opened" &&
      line.message.payload?.scope === "remote",
  );

/**
 * W10 + W11. Returns `{status: "SKIP", reason}` without the WSL distro. `call` is the shell-side
 * `harness-web-remote-call` client; `localTargets` are the targets synced before (kept in the sync).
 */
export async function runPhoneOnRemoteWorkspace({ sidecar, modules, url, desktopWs, phone, call, localTargets }) {
  if (process.platform !== "win32") return { status: "SKIP", reason: "WSL is Windows-only" };
  const control = createControlChannel(desktopWs);
  const distros = await control.request("listWSLDistros", undefined, DISCOVERY_TIMEOUT_MS, "W10_DISCOVER");
  const names = Array.isArray(distros) ? distros.map((entry) => entry?.name).filter(Boolean) : [];
  if (!names.includes(WSL_DISTRO)) return { status: "SKIP", reason: `WSL distro "${WSL_DISTRO}" is not installed` };

  const tracker = createScopedPortTracker({ modules, url, control });
  const remoteDir = `/tmp/harness-phase4-phone-${randomBytes(6).toString("hex")}`;
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
      "printf 'new\\n' > phone-marker.txt",
      "",
    ].join("\n"),
    120_000,
  );
  if (seed.code !== 0) fail("W10_SEED", `seeding ${remoteDir} failed: ${seed.stderr.trim()}`);

  const summary = { status: "PASS", distro: WSL_DISTRO, remoteDir, timings: {} };
  let sessionId;
  try {
    // Desktop: connect + bind once, exactly like the remote dialog (install consent mirrored).
    const target = { kind: "wsl", distro: WSL_DISTRO };
    let startedAt = Date.now();
    let connected = await control.request("connectRemote", { target, requestId: randomUUID() }, CONNECT_TIMEOUT_MS, "W10_CONNECT");
    if (
      connected?.success === false &&
      (connected.reasonCode === "source-server-install-required" || connected.reasonCode === "source-server-upgrade-required")
    ) {
      connected = await control.request(
        "connectRemote",
        { target, requestId: randomUUID(), confirmSourceServerInstall: true },
        CONNECT_TIMEOUT_MS,
        "W10_CONNECT",
      );
    }
    if (connected?.success !== true || typeof connected.sessionId !== "string") {
      fail("W10_CONNECT", `connectRemote failed: ${String(connected?.error)} (reasonCode ${String(connected?.reasonCode)})`);
    }
    sessionId = connected.sessionId;
    summary.timings.desktopConnectMs = Date.now() - startedAt;
    const bound = await control.request(
      "bindRemoteWorkspaceSessionContext",
      { remoteSessionId: sessionId, workspacePath: remoteDir, workspacePurpose: "project" },
      BIND_TIMEOUT_MS,
      "W10_BIND",
    );
    if (bound?.success !== true) fail("W10_BIND", `bind failed: ${String(bound?.error)}`);
    const scoped = await tracker.waitFor(
      (entry) => entry.payload?.sessionId === sessionId && entry.payload?.workspacePath === remoteDir,
      "W10_SCOPED_PORT",
      remoteDir,
    );
    const workspaceIdentity = scoped.payload.workspaceIdentity;
    if (typeof workspaceIdentity !== "string" || workspaceIdentity.trim() === "" || workspaceIdentity === remoteDir) {
      fail("W10_IDENTITY", `desktop scoped port carried workspaceIdentity ${JSON.stringify(workspaceIdentity)}`);
    }
    summary.workspaceIdentityKind = workspaceIdentity.split(":")[0];

    // Desktop renderer publishes the remote target (as syncWebRemoteControlWorkspaces does).
    const remoteTarget = {
      workspacePath: remoteDir,
      workspaceIdentity,
      remoteSessionId: sessionId,
      label: "phase4-wsl",
      workspacePurpose: "project",
      kind: "remote",
      connectionState: "connected",
    };
    await call("syncWorkspaces", { workspaces: [...localTargets, remoteTarget] }, "W10_SYNC");
    const before = await call("getRemoteWorkspaceStats", {}, "W10_STATS");
    if (before?.connectRequests < 1 || before?.manager?.activeSessionCount !== 1) {
      fail("W10_STATS", `unexpected stats before the phone: ${JSON.stringify(before)}`);
    }
    const openedBefore = remoteOpened(sidecar).length;

    // Phone: bridge keyed by workspaceIdentity (身份 key = workspaceIdentity?.trim() || workspacePath).
    startedAt = Date.now();
    const bridge = await phone.openBridge(
      { workspaceKey: workspaceIdentity, bridgeSessionId: "phase4-remote-bridge-1", bridgeGeneration: 3 },
      "W10_BRIDGE",
    );
    summary.timings.phoneBridgeMs = Date.now() - startedAt;
    const opened = remoteOpened(sidecar).slice(openedBefore);
    if (opened.length !== 1 || opened[0].message.payload.clientMode !== "web-remote-replayable") {
      fail("W10_ATTACH", `expected one remote-scoped phone attachment, saw ${opened.length}`);
    }
    const phoneAttachmentId = opened[0].message.payload.attachmentId;
    summary.phoneAttachment = { attachmentId: phoneAttachmentId, remoteKind: opened[0].message.payload.remoteKind };
    const client = bridge.client;

    // Files from inside the distro (the directory does not exist on Windows).
    startedAt = Date.now();
    const entries = await callRpc(client, "file", "readdir", { path: remoteDir, includeHidden: true }, "W10_FILE");
    const entryNames = Array.isArray(entries) ? entries.map((entry) => entry?.name) : [];
    for (const expected of ["a.txt", "src", "phone-marker.txt", ".git"]) {
      if (!entryNames.includes(expected)) fail("W10_FILE", `phone readdir is missing ${expected}: ${JSON.stringify(entryNames)}`);
    }
    summary.timings.phoneReaddirMs = Date.now() - startedAt;
    summary.readdir = entryNames.sort();

    // Git status of the remote repository.
    startedAt = Date.now();
    const repo = await callRpc(client, "git", "getRepositorySummary", { workspacePath: remoteDir }, "W10_GIT");
    if (repo?.repoRoot !== remoteDir) fail("W10_GIT", `repoRoot ${String(repo?.repoRoot)} != ${remoteDir}`);
    const changes = await callRpc(client, "git", "getChanges", { workspacePath: remoteDir, sourceId: "unstaged" }, "W10_GIT");
    const changed = Array.isArray(changes) ? changes.map((change) => change?.path) : [];
    if (!changed.some((entry) => entry === "a.txt" || entry === `${remoteDir}/a.txt`)) {
      fail("W10_GIT", `phone unstaged changes lack a.txt: ${JSON.stringify(changed)}`);
    }
    summary.timings.phoneGitMs = Date.now() - startedAt;
    summary.git = { branchName: repo.branchName, unstaged: changed.sort() };

    // Terminal in the distro, cwd = the remote workspace.
    startedAt = Date.now();
    const term = await openTerminal(client, { cols: 100, rows: 30, cwd: remoteDir });
    try {
      if (!(await waitQuiet(term))) fail("W10_TERMINAL", "remote shell produced no prompt");
      const marker = `HP${randomBytes(3).toString("hex")}`;
      const seen = new Promise((resolve) => {
        const waiter = () => {
          if (stripAnsi(term.raw).includes(`${marker}Linux:${remoteDir}`)) {
            term.dataWaiters.delete(waiter);
            resolve(true);
          }
        };
        term.dataWaiters.add(waiter);
      });
      await term.run(`printf '%s%s:%s\\n' '${marker}' "$(uname -s)" "$(pwd)"`);
      await withTimeout(seen, 20_000, "W10_TERMINAL", "phone terminal output from the distro");
      summary.terminal = { shell: term.created.shell };
    } finally {
      term.unsubscribe();
      await term.dispose().catch(() => undefined);
    }
    summary.timings.phoneTerminalMs = Date.now() - startedAt;

    // Exactly one connection: the phone reused the desktop's session.
    const after = await call("getRemoteWorkspaceStats", {}, "W10_SINGLE_SESSION");
    const phoneAttaches = (after.remoteScopedAttachRequests?.["web-remote-replayable"] ?? 0) - (before.remoteScopedAttachRequests?.["web-remote-replayable"] ?? 0);
    if (after.connectRequests !== before.connectRequests || after.connectedEvents !== before.connectedEvents) {
      fail("W10_SINGLE_SESSION", `the phone caused a new connection: ${JSON.stringify({ before, after })}`);
    }
    if (after.manager?.activeSessionCount !== 1 || after.manager?.activeTargetCount !== 1) {
      fail("W10_SINGLE_SESSION", `expected one remote session for the profile, saw ${JSON.stringify(after.manager)}`);
    }
    if (phoneAttaches !== 1) fail("W10_SINGLE_SESSION", `expected one phone remote attachment, saw ${phoneAttaches}`);
    summary.singleSession = {
      connectRequests: after.connectRequests,
      connectedEvents: after.connectedEvents,
      activeSessionCount: after.manager.activeSessionCount,
      activeTargetCount: after.manager.activeTargetCount,
      phoneRemoteAttachments: phoneAttaches,
    };

    // W11: the desktop disconnects; the phone fails cleanly and nothing reconnects.
    const disposedAt = Date.now();
    const payloadMark = phone.payloads.length;
    const relayMark = phone.relayMessages.length;
    // 修复说明（W11 偶发超时）：此前谓词在闭包里延迟读取可变的 `sessionId`，而下面在 dispose 应答后立即
    // 把它置为 undefined。sidecar 在 dispose 处理中同步发出 workspace-bridge-error，但它经 relay 到达手机
    // 与 dispose 应答经桌面控制通道返回是两条独立链路；负载下 relay 那条更慢时，谓词比较的是
    // `remote-session-closed:undefined`，正确的 payload 被忽略直到 20 s 超时。复现证据：负载下 11 次中 5 次失败，
    // 诊断显示手机已收到 requestId=remote-session-closed:<该会话> 的 workspace-bridge-error。
    // 这是 gate 自身的绑定错误，不是产品竞态，也不是超时过短：在注册等待前固定会话 id。
    const closedSessionId = sessionId;
    const bridgeError = phone.waitForPayload(
      (payload) => payload.zcode_type === "workspace-bridge-error" && payload.requestId === `remote-session-closed:${closedSessionId}`,
      "W11_BRIDGE_ERROR",
      "workspace-bridge-error after the desktop disconnect",
    );
    await control.request("disposeRemoteSession", { sessionId: closedSessionId }, CALL_TIMEOUT_MS, "W11_DISPOSE");
    sessionId = undefined;
    const errorPayload = await bridgeError.catch(async (error) => {
      // 诊断：超时时附上手机在 dispose 前后收到的 payload、relay 消息与 sidecar 的 web-remote 日志。
      const status = await call("getStatus", {}, "W11_DIAG").catch((e) => String(e));
      const webRemoteLog = sidecar
        .stderrTail(4000)
        .split(/\r?\n/)
        .filter((line) => line.includes("[web-remote-control]") || line.includes("remote-session") || line.includes("window-host-remote"))
        .slice(-40);
      const diag = {
        payloadsAfterDispose: phone.payloads.slice(payloadMark),
        payloadsBeforeDispose: phone.payloads.slice(Math.max(0, payloadMark - 15), payloadMark).map((p) => ({ t: p.zcode_type ?? p.relay?.type, r: p.requestId })),
        relayAfterDispose: phone.relayMessages.slice(relayMark),
        phoneConnected: phone.connected,
        status,
        webRemoteLog,
      };
      error.message += `\nW11 diagnostics: ${JSON.stringify(diag, null, 2)}`;
      throw error;
    });
    summary.failClosed = { bridgeErrorReason: errorPayload.reason, bridgeErrorMs: Date.now() - disposedAt };
    phone.discardBridge();
    for (const entry of tracker.attachments.filter((item) => item.payload?.sessionId === closedSessionId && item.port)) {
      await waitClosed(entry, "W11_DESKTOP_DETACHED");
    }
    const attachStats = await call("getAttachmentStats", {}, "W11_RELEASED");
    const phoneEntry = attachStats.find((entry) => entry.attachmentId === phoneAttachmentId);
    if (!phoneEntry || phoneEntry.open) fail("W11_RELEASED", "the phone's remote attachment was not released");

    // A new phone bridge for the same (stale) target must fail without touching the Host.
    const reopen = await phone
      .openBridge({ workspaceKey: workspaceIdentity, bridgeSessionId: "phase4-remote-bridge-2", bridgeGeneration: 4 }, "W11_REOPEN")
      .then(
        () => ({ ok: true }),
        (error) => ({ ok: false, message: error instanceof Error ? error.message : String(error) }),
      );
    if (reopen.ok) fail("W11_REOPEN", "a phone bridge opened on a disposed remote session");
    if (!reopen.message.includes("workspace-bridge-error")) fail("W11_REOPEN", `unexpected failure: ${reopen.message}`);
    summary.failClosed.reopenError = reopen.message.replace(/^workspace-bridge-error: /, "");
    const final = await call("getRemoteWorkspaceStats", {}, "W11_NO_RECONNECT");
    if (final.connectRequests !== after.connectRequests || final.connectedEvents !== after.connectedEvents) {
      fail("W11_NO_RECONNECT", "a new remote connection was created after the desktop disconnected");
    }
    if (JSON.stringify(final.remoteScopedAttachRequests) !== JSON.stringify(after.remoteScopedAttachRequests)) {
      fail("W11_NO_RECONNECT", "a remote attachment was requested after the desktop disconnected");
    }
    if (final.manager?.activeSessionCount !== 0) fail("W11_NO_RECONNECT", `sessions left: ${JSON.stringify(final.manager)}`);
    if (remoteOpened(sidecar).length !== openedBefore + 1) fail("W11_NO_RECONNECT", "the phone got another remote attachment");
    summary.failClosed.connectRequests = final.connectRequests;
    summary.failClosed.activeSessionCount = final.manager.activeSessionCount;
    // 恢复桌面工作区列表（去掉已断开的远程目标），后续场景只看到本地工作区。
    await call("syncWorkspaces", { workspaces: localTargets }, "W11_SYNC");
  } finally {
    if (sessionId) await control.request("disposeRemoteSession", { sessionId }, CALL_TIMEOUT_MS, "W10_CLEANUP").catch(() => undefined);
    for (const entry of tracker.attachments) entry.connection?.dispose();
    const cleanup = await wslSh(`rm -rf '${remoteDir}'\n`, 60_000);
    summary.tempDirRemoved = cleanup.code === 0;
  }
  return summary;
}
