#!/usr/bin/env node
// Phase 4 web remote-control gate (spec docs/specs/harness/phase-4-web-remote-control.md, section 10).
//
// Runs the real ZCode Host inside the real host-node launcher, drives the vendored web
// remote-control manager through the same `harness-web-remote-*` channel the Tauri shell uses, and
// pairs a simulated phone through ZCode's own local relay fixture (`createFakeRelayV2`; no relay
// was written for Harness, and the real ZCode cloud relay is never contacted). Credentials are
// test-only fixture values written into an isolated data directory.
//
// Prints PHASE4_WEB_REMOTE_OK plus a JSON summary, or PHASE4_WEB_REMOTE_FAIL <step>.
import { existsSync } from "node:fs";
import { mkdir, rm, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import path from "node:path";
import process from "node:process";
import { fileURLToPath, pathToFileURL } from "node:url";
import { loadModules, paths as phase2Paths, repoRoot, verifyArtifacts } from "./phase2/environment.mjs";
import { GateError, delay, fail, withTimeout } from "./phase2/gate.mjs";
import { callSettingGet } from "./phase2/rpcClient.mjs";
import { ALLOWED_ORIGIN, attachAndCallSettings, awaitReady, buildInitLocalMessage } from "./phase2/scenarios.mjs";
import { startSidecar } from "./phase2/sidecarProcess.mjs";
import { createPhone } from "./phase4-web-remote/phone.mjs";
import { runPhoneListings, runPhoneOnRemoteWorkspace, writeFakeSshConfig } from "./phase4-web-remote/remoteWorkspace.mjs";
import { runPhoneTempAttachmentAndMcp } from "./phase4-web-remote/mcpTempAttachment.mjs";
import { installIsolatedHome, isolatedHomeDir } from "./lib/isolatedHome.mjs";

// 修复说明：Host/Agent 按 HOME/USERPROFILE 解析 ~/.zcode，曾把测试数据写进用户真实设置；
// 在任何子进程启动前切到一次性 home，并在退出时检查真实 ~/.zcode/v2 文件未被改动（HOME_LEAK）。
installIsolatedHome("phase4-web-remote");

const moduleDir = path.dirname(fileURLToPath(import.meta.url));
const runDir = path.join(repoRoot, "Worse/.work/tmp/phase4-web-remote");
const dataDir = path.join(runDir, "data");
const workspaceDir = path.join(runDir, "workspace");
const kitFile = path.join(runDir, "phoneKit.mjs");
const webRemoteRuntime = path.join(repoRoot, "Worse/sidecars/host-node/dist-vendor/webRemoteRuntime.js");

// 仅用于本 gate 的假账号：与 ZCode desktop e2e fixture 相同的 mock 值，绝不读取用户真实凭据。
const FIXTURE_JWT = "mock-token";
const FIXTURE_USER = "zcode-e2e-user";
const FIXTURE_CREDENTIALS = {
  "oauth:active_provider": "zai",
  "oauth:zai:access_token": FIXTURE_JWT,
  "oauth:zai:user_info": JSON.stringify({ id: FIXTURE_USER, username: "Harness Phase4", displayName: "Harness Phase4" }),
  zcodejwttoken: FIXTURE_JWT,
};
const APP_VERSION = "3.15.1";
const CALL_TIMEOUT_MS = 45_000;
const RPC_TIMEOUT_MS = 30_000;
const INPUT_BURST = 8;

const startTime = Date.now();

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

async function buildPhoneKit() {
  const requireFromRepo = createRequire(path.join(repoRoot, "package.json"));
  const { build } = await import(pathToFileURL(requireFromRepo.resolve("esbuild")).href);
  await build({
    entryPoints: [path.join(moduleDir, "phase4-web-remote/phoneKit.entry.ts")],
    outfile: kitFile,
    bundle: true,
    platform: "node",
    format: "esm",
    target: "node24",
    external: ["ws"],
    logLevel: "warning",
    banner: {
      js: 'import { createRequire as __gateCreateRequire } from "node:module";\nconst require = __gateCreateRequire(import.meta.url);',
    },
  });
  return import(pathToFileURL(kitFile).href);
}

/** Shell → sidecar `harness-web-remote-call`, exactly as the Rust remote_control module sends it. */
function createWebRemoteClient(sidecar) {
  let nextId = 0;
  return async (method, params, step) => {
    nextId += 1;
    const id = `gate-${nextId}`;
    sidecar.sendLine({ t: "host", message: { type: "harness-web-remote-call", id, method, params, appVersion: APP_VERSION } });
    const line = await sidecar.waitForLine(
      (candidate) => candidate.t === "host" && candidate.message?.type === "harness-web-remote-result" && candidate.message.id === id,
      CALL_TIMEOUT_MS,
      step,
      `harness-web-remote-result ${method}`,
    );
    if (!line.message.ok) fail(step, `${method} failed: ${String(line.message.error)}`);
    return line.message.value;
  };
}

const webRemoteEvents = (sidecar, event) =>
  sidecar.lines
    .filter((line) => line.t === "host" && line.message?.type === "harness-web-remote-event" && line.message.event === event)
    .map((line) => line.message.payload);

const settingCall = (client, command, args, step) =>
  withTimeout(client.getChannel("setting").call(command, args), RPC_TIMEOUT_MS, step, `setting.${command}`);

async function statsFor(call, attachmentId, step) {
  const stats = await call("getAttachmentStats", {}, step);
  const entry = stats.find((candidate) => candidate.attachmentId === attachmentId);
  if (!entry) fail(step, `no stats for attachment ${attachmentId}`);
  return entry;
}

async function runScenarios({ sidecar, modules, kit, relay, dropNext }) {
  const ready = await awaitReady(sidecar);
  const ctx = { sidecar, ...modules, url: `ws://127.0.0.1:${ready.wsPort}/v1/attach` };
  const call = createWebRemoteClient(sidecar);

  // W0: the desktop renderer's continuous attachment (init-local) exists before remote control.
  const desktop = await attachAndCallSettings(
    ctx,
    buildInitLocalMessage({ repoRoot, providerConfig: phase2Paths.providerConfig }),
    "init-local",
    "W0",
  );
  const agentSpawnsBefore = sidecar.lines.filter((line) => line.message?.type === "agent-process-spawned").length;

  // W1: pairing — the desktop registers on the local relay and reaches QR-ready.
  const workspaceKey = workspaceDir;
  await call(
    "syncWorkspaces",
    { workspaces: [{ workspacePath: workspaceDir, label: "phase4-workspace", kind: "local" }] },
    "W1_SYNC",
  );
  const started = await call("start", { context: { workspacePath: workspaceDir } }, "W1_START");
  if (started?.status !== "running" || typeof started.qrUrl !== "string" || !started.qrUrl.includes("sid=")) {
    fail("W1_START", `unexpected start result ${JSON.stringify(started)}`);
  }
  const registered = relay.httpCalls.filter((entry) => entry.path === "/api/v1/remote/device" && entry.method === "POST");
  if (registered.length !== 1 || registered[0].userId !== FIXTURE_USER) {
    fail("W1_REGISTER", "the desktop device was not registered exactly once for the fixture account");
  }
  const qr = new URL(started.qrUrl);

  const phone = createPhone({ kit, WebSocket: modules.WebSocket, relay, jwt: FIXTURE_JWT });
  try {
    const pairing = await phone.connect("W1_PAIR");
    if (qr.searchParams.get("sid") !== pairing.deviceSid) fail("W1_PAIR", "QR sid does not match the paired device");

    // W2: the phone attaches to the existing Host as one replayable attachment.
    const bridge1 = await phone.openBridge(
      { workspaceKey, bridgeSessionId: "phase4-bridge-1", bridgeGeneration: 1 },
      "W2_BRIDGE",
    );
    const attached1 = webRemoteEvents(sidecar, "attachment").filter((event) => event.state === "opened");
    if (attached1.length !== 1 || attached1[0].clientMode !== "web-remote-replayable") {
      fail("W2_ATTACH", `expected one web-remote-replayable attachment, saw ${JSON.stringify(attached1)}`);
    }
    const attachment1 = attached1[0].attachmentId;
    const phoneSettings = await settingCall(bridge1.client, "get", [], "W2_SETTING_GET");
    if (!phoneSettings || typeof phoneSettings !== "object") fail("W2_SETTING_GET", "phone setting.get returned nothing");
    const statusActive = await call("getStatus", {}, "W2_STATUS");
    if (statusActive.status !== "active" || statusActive.mobileConnected !== true) {
      fail("W2_STATUS", `status after bridge: ${JSON.stringify({ status: statusActive.status, mobile: statusActive.mobileConnected })}`);
    }

    // W3: input burst with a forced phone disconnect + reconnect mid-flight.
    const before = await statsFor(call, attachment1, "W3_STATS");
    const values = Array.from({ length: INPUT_BURST }, (_, index) => `phase4-input-${index + 1}`);
    const updates = values.map((value) => settingCall(bridge1.client, "update", [{ terminalFontFamily: value }], "W3_INPUT"));
    relay.disconnect({ role: "terminal" });
    await delay(300);
    await phone.connect("W3_RECONNECT");
    bridge1.relayProtocol.replayUnacknowledged();
    await Promise.all(updates);
    const after = await statsFor(call, attachment1, "W3_STATS");
    const delivered = after.messagesToHost - before.messagesToHost;
    if (delivered !== INPUT_BURST) {
      fail("W3_NO_DUP_NO_LOSS", `Host received ${delivered} RPC messages for ${INPUT_BURST} inputs`);
    }
    const desktopView = await callSettingGet(desktop.connection.client, "W3_DESKTOP_SEES");
    if (desktopView.terminalFontFamily !== values.at(-1)) {
      fail("W3_ORDER", `desktop sees ${String(desktopView.terminalFontFamily)}, expected ${values.at(-1)}`);
    }

    // W4: a dropped rpc-frame is detected as a gap; the bridge is replaced and state is re-read.
    dropNext.armed = true;
    const lost = settingCall(bridge1.client, "update", [{ terminalFontFamily: "phase4-gap-a" }], "W4_GAP").catch(() => "aborted");
    await delay(200);
    const lostToo = settingCall(bridge1.client, "update", [{ terminalFontFamily: "phase4-gap-b" }], "W4_GAP").catch(() => "aborted");
    const degraded = await phone.waitForPayload(
      (payload) => payload.zcode_type === "bridge-degraded" && payload.bridgeSessionId === "phase4-bridge-1",
      "W4_GAP",
      "bridge-degraded",
    );
    if (dropNext.dropped !== 1) fail("W4_GAP", `relay dropped ${dropNext.dropped} frames, expected 1`);
    phone.discardBridge();
    const abandoned = await Promise.all([lost, lostToo]);
    const statsAfterGap = await statsFor(call, attachment1, "W4_RELEASE");
    if (statsAfterGap.open) fail("W4_RELEASE", "the degraded bridge's attachment was not released");
    if (statsAfterGap.messagesToHost !== after.messagesToHost) {
      fail("W4_GAP", "frames after the gap reached the Host");
    }
    const bridge2 = await phone.openBridge(
      {
        workspaceKey,
        bridgeSessionId: "phase4-bridge-2",
        bridgeGeneration: 2,
        recoveryId: degraded.recoveryId ?? "phase4-recovery-1",
      },
      "W4_REBUILD",
    );
    const snapshot = await settingCall(bridge2.client, "get", [], "W4_SNAPSHOT");
    if (snapshot.terminalFontFamily !== values.at(-1)) {
      fail("W4_SNAPSHOT", `snapshot shows ${String(snapshot.terminalFontFamily)}, expected committed ${values.at(-1)}`);
    }
    // 手机恢复后重新提交未确认的输入（对应网页恢复草稿后重发），只应被执行一次。
    const attached2 = webRemoteEvents(sidecar, "attachment").filter((event) => event.state === "opened");
    const attachment2 = attached2.at(-1)?.attachmentId;
    if (attached2.length !== 2 || attachment2 === attachment1) fail("W4_REBUILD", "the rebuilt bridge did not get a fresh attachment");
    const before2 = await statsFor(call, attachment2, "W4_RESEND");
    await settingCall(bridge2.client, "update", [{ terminalFontFamily: "phase4-gap-b" }], "W4_RESEND");
    const after2 = await statsFor(call, attachment2, "W4_RESEND");
    if (after2.messagesToHost - before2.messagesToHost !== 1) fail("W4_RESEND", "re-sent input was not delivered exactly once");

    // W5: the desktop continuous attachment was never touched.
    const desktopFinal = await callSettingGet(desktop.connection.client, "W5_DESKTOP");
    if (desktopFinal.terminalFontFamily !== "phase4-gap-b") fail("W5_DESKTOP", "desktop does not see the phone's final input");
    const desktopClosed = sidecar.lines.some((line) => line.t === "attach.closed" && line.attachmentId === desktop.summary.attachmentId);
    if (desktopClosed) fail("W5_DESKTOP", "the desktop continuous attachment closed during phone recovery");
    const readyLines = sidecar.lines.filter((line) => line.t === "ready").length;
    const agentSpawnsAfter = sidecar.lines.filter((line) => line.message?.type === "agent-process-spawned").length;
    if (readyLines !== 1 || sidecar.exitInfo || ready.hostPid !== sidecar.child.pid) fail("W5_SINGLE_HOST", "the Host process changed");
    if (agentSpawnsAfter !== agentSpawnsBefore) fail("W5_SINGLE_HOST", "remote control spawned an Agent process");
    const relayHostConnections = relay.messages.filter((entry) => entry.role === "device" && entry.message?.type === "auth_init").length;

    // W12-W15 (fix B): temp text attachments + MCP user directory, phone and desktop paths, isolated home only.
    const mainPlatform = await runPhoneTempAttachmentAndMcp({
      phone,
      sidecar,
      kit,
      dataDir,
      homeDir: isolatedHomeDir(),
      workspaceDir,
    });

    // W9: the phone's Docker/WSL/SSH listings, answered from the desktop OS (fake ssh config only).
    const listings = await runPhoneListings(phone);
    // W10 + W11: the phone on the desktop's WSL workspace through the existing session, then fail closed.
    const remoteWorkspace = await runPhoneOnRemoteWorkspace({
      sidecar,
      modules,
      url: ctx.url,
      desktopWs: desktop.port.ws,
      phone,
      call,
      localTargets: [{ workspacePath: workspaceDir, label: "phase4-workspace", kind: "local" }],
    });

    // W6: stop releases the relay session; the shell and relay keep no state behind.
    await call("stop", {}, "W6_STOP");
    const stopped = await call("getStatus", {}, "W6_STOP");
    if (stopped.status !== "idle") fail("W6_STOP", `status after stop: ${String(stopped.status)}`);
    const openAfterStop = (await call("getAttachmentStats", {}, "W6_STOP")).filter((entry) => entry.open);
    if (openAfterStop.length !== 0) fail("W6_STOP", "phone attachments survived stop");

    desktop.connection.dispose();
    desktop.port.close();
    return {
      status: "ok",
      relay: { kind: "local fixture createFakeRelayV2", httpUrl: relay.httpUrl, deviceAuthInits: relayHostConnections },
      pairing: { deviceSid: pairing.deviceSid, qrPath: qr.pathname },
      bridge1: { attachmentId: attachment1, clientMode: attached1[0].clientMode },
      inputBurst: { sent: INPUT_BURST, deliveredToHost: delivered, forcedDisconnect: true, final: values.at(-1) },
      gap: {
        dropped: dropNext.dropped,
        degradedReason: degraded.reason,
        abandonedCalls: abandoned.filter((value) => value === "aborted").length,
        oldAttachmentReleased: !statsAfterGap.open,
        snapshot: snapshot.terminalFontFamily,
        rebuiltAttachment: attachment2,
        resentDelivered: after2.messagesToHost - before2.messagesToHost,
      },
      desktopContinuous: { attachmentId: desktop.summary.attachmentId, closed: desktopClosed, sawPhoneInput: true },
      singleHost: { hostPid: ready.hostPid, readyLines, agentSpawnsDuringRemote: agentSpawnsAfter - agentSpawnsBefore },
      stopped: stopped.status,
      phoneListings: listings,
      phoneRemoteWorkspace: remoteWorkspace,
      mainPlatformHandlers: mainPlatform,
    };
  } finally {
    phone.close();
  }
}

async function main() {
  verifyArtifacts();
  if (!existsSync(webRemoteRuntime)) {
    fail("BUILD_WEB_REMOTE", "Worse/sidecars/host-node/dist-vendor/webRemoteRuntime.js is missing", "pnpm --dir Worse/sidecars/host-node build");
  }
  const modules = await loadModules();
  await removeRunDir();
  await mkdir(path.join(dataDir, ".zcode", "v2"), { recursive: true });
  await mkdir(workspaceDir, { recursive: true });
  await writeFile(path.join(dataDir, ".zcode", "v2", "credentials.json"), JSON.stringify(FIXTURE_CREDENTIALS), { mode: 0o600 });
  const kit = await buildPhoneKit();
  // W9 只读取隔离 home 里的假 ~/.ssh/config，绝不接触用户真实 SSH 配置或密钥。
  await writeFakeSshConfig(isolatedHomeDir());

  const dropNext = { armed: false, dropped: 0 };
  const relay = await kit.createFakeRelayV2({
    users: { [FIXTURE_JWT]: FIXTURE_USER },
    onMessage: (record) => {
      if (dropNext.armed && record.role === "terminal" && record.message?.payload?.zcode_type === "rpc-frame") {
        dropNext.armed = false;
        dropNext.dropped += 1;
        return { drop: true };
      }
      return undefined;
    },
  });

  const sidecar = startSidecar({
    launcher: phase2Paths.launcher,
    cwd: repoRoot,
    env: {
      HARNESS_HOST_ENTRY: phase2Paths.hostEntry,
      HARNESS_ALLOWED_ORIGINS: ALLOWED_ORIGIN,
      ZCODE_ENV: "production",
      ZCODE_DATA_BASE_DIR: dataDir,
      ZCODE_BUILTIN_PROVIDER_CONFIG_FILE: phase2Paths.providerConfig,
      // 只指向本地 relay fixture；真实 ZCode 云端 relay 不会被访问。
      ZCODE_ENDPOINT_ORIGIN: relay.httpUrl,
      ZCODE_WEB_REMOTE_CONTROL_RELAY_WS_URL: relay.wsUrl,
      HARNESS_APP_VERSION: APP_VERSION,
    },
  });

  try {
    const summary = await runScenarios({ sidecar, modules, kit, relay, dropNext });
    sidecar.child.stdin.end();
    const exit = await sidecar.waitForExit(15_000, "W7_EXIT");
    summary.shutdown = { exitCode: exit.code };
    if (exit.code !== 0) fail("W7_EXIT", `sidecar exited with code ${String(exit.code)}`);
    summary.elapsedMs = Date.now() - startTime;
    process.stdout.write(`PHASE4_WEB_REMOTE_OK\n${JSON.stringify(summary, null, 2)}\n`);
  } catch (error) {
    const tail = sidecar.stderrTail(40);
    if (tail) process.stderr.write(`sidecar stderr (tail):\n${tail}\n`);
    throw error;
  } finally {
    sidecar.kill();
    await relay.close().catch(() => {});
    if (!(await removeRunDir())) process.stderr.write(`note: could not remove ${runDir}\n`);
  }
}

main().then(
  () => process.exit(0),
  (error) => {
    const step = error instanceof GateError ? error.step : "UNEXPECTED";
    process.stderr.write(`PHASE4_WEB_REMOTE_FAIL ${step}\nreason: ${error instanceof Error ? error.stack ?? error.message : String(error)}\n`);
    if (error instanceof GateError && error.command) process.stderr.write(`run: ${error.command}\n`);
    process.exit(1);
  },
);
