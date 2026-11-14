import { randomBytes, randomUUID } from "node:crypto";
import { fail, isProcessAlive } from "./gate.mjs";
import { callSettingGet, connectRpc } from "./rpcClient.mjs";
import { attachSocket, connectSocket, helloCloseCode } from "./wsClient.mjs";

export const ALLOWED_ORIGIN = "http://harness.test";
const BAD_ORIGIN = "https://evil.example";
const ATTACHMENT_TTL_MS = 30_000;
const READY_TIMEOUT_MS = 60_000;
const ATTACH_TIMEOUT_MS = 10_000;
const ACTIVE_TIMEOUT_MS = 20_000;
export const SHUTDOWN_TIMEOUT_MS = 15_000;

/** `init-local` exactly as the shell builds it (contract section 7): no hostId, no deliveryKind. */
export function buildInitLocalMessage({ repoRoot, providerConfig }) {
  return {
    type: "init-local",
    databaseStartupId: randomUUID(),
    locale: "en-US",
    zcodeBuiltinProviderConfigFilePath: providerConfig,
    deviceMid: "harness-phase2-e2e",
    agentSpawnFallbackCwd: repoRoot,
  };
}

/** The shell's reload message (contract section 4.2 / p2c). */
export function buildAttachServicePortMessage() {
  return {
    type: "attach-service-port",
    requestId: randomUUID(),
    attachmentId: randomUUID(),
    clientMode: "desktop-continuous",
    scope: { kind: "local" },
  };
}

/** S1: `ready` must carry a valid port and the launcher pid. */
export async function awaitReady(sidecar) {
  const ready = await sidecar.waitForLine((line) => line.t === "ready", READY_TIMEOUT_MS, "S1_READY", "ready");
  if (!Number.isInteger(ready.wsPort) || ready.wsPort <= 0 || ready.wsPort > 65_535) {
    fail("S1_READY", `ready carried an invalid wsPort ${String(ready.wsPort)}`);
  }
  if (ready.protocol !== 1) fail("S1_READY", `ready carried protocol ${String(ready.protocol)}`);
  if (ready.hostPid !== sidecar.child.pid) {
    fail("S1_READY", `ready.hostPid ${String(ready.hostPid)} != launcher pid ${String(sidecar.child.pid)}`);
  }
  return ready;
}

/** Sends `attach.open` as Rust does and waits for `attach.opened`. */
export async function openAttachment(sidecar, hostMessage, step) {
  const attachmentId = randomUUID();
  const requestId = randomUUID();
  const token = randomBytes(32).toString("hex");
  const startedAt = Date.now();
  sidecar.sendLine({ t: "attach.open", requestId, attachmentId, token, ttlMs: ATTACHMENT_TTL_MS, hostMessage });
  const result = await sidecar.waitForLine(
    (line) => (line.t === "attach.opened" || line.t === "attach.failed") && line.requestId === requestId,
    ATTACH_TIMEOUT_MS,
    step,
    "attach.opened",
  );
  if (result.t === "attach.failed") fail(step, `attach.failed ${String(result.code)}: ${String(result.message)}`);
  if (result.attachmentId !== attachmentId) fail(step, `attach.opened echoed ${String(result.attachmentId)}`);
  return { attachmentId, token, startedAt };
}

/**
 * S1/S2: open a grant, attach over WS, require `attach.active` with the
 * expected message type, then Initialize and `setting.get`.
 */
export async function attachAndCallSettings(ctx, hostMessage, expectedType, prefix) {
  const { sidecar, WebSocket, rpc, url } = ctx;
  const grant = await openAttachment(sidecar, hostMessage, `${prefix}_ATTACH_OPEN`);
  const port = await attachSocket(WebSocket, url, ALLOWED_ORIGIN, grant, `${prefix}_ATTACH_WS`);
  const active = await sidecar.waitForLine(
    (line) => line.t === "attach.active" && line.attachmentId === grant.attachmentId,
    ACTIVE_TIMEOUT_MS,
    `${prefix}_ATTACH_ACTIVE`,
    "attach.active",
  );
  if (active.messageType !== expectedType) {
    fail(`${prefix}_ATTACH_ACTIVE`, `attach.active carried ${String(active.messageType)}, expected ${expectedType}`);
  }
  const connection = await connectRpc(rpc, port, `${prefix}_INITIALIZE`);
  const settings = await callSettingGet(connection.client, `${prefix}_SETTING_GET`);
  return {
    port,
    connection,
    summary: {
      attachmentId: grant.attachmentId,
      messageType: active.messageType,
      initializeMs: connection.initializedAt - grant.startedAt,
      settingKeys: Object.keys(settings).length,
    },
  };
}

/** S3: a wrong token closes 4401 and a foreign Origin gets HTTP 403. */
export async function assertRejections(ctx) {
  const { sidecar, WebSocket, url } = ctx;
  const grant = await openAttachment(sidecar, buildAttachServicePortMessage(), "S3_TOKEN_ATTACH");
  const tokenConnection = await connectSocket(WebSocket, url, ALLOWED_ORIGIN, "S3_TOKEN");
  if (tokenConnection.statusCode !== undefined) {
    fail("S3_TOKEN", `valid-origin upgrade was rejected with HTTP ${tokenConnection.statusCode}`);
  }
  const wrongTokenCloseCode = await helloCloseCode(
    tokenConnection.ws,
    { attachmentId: grant.attachmentId, token: "0".repeat(64) },
    "S3_TOKEN",
  );
  if (wrongTokenCloseCode !== 4401) fail("S3_TOKEN", `wrong token closed with ${wrongTokenCloseCode}, expected 4401`);

  const originConnection = await connectSocket(WebSocket, url, BAD_ORIGIN, "S3_ORIGIN");
  if (originConnection.statusCode !== 403) {
    originConnection.ws?.terminate();
    fail("S3_ORIGIN", "a foreign Origin was upgraded instead of answered with HTTP 403");
  }
  return { wrongTokenCloseCode, badOriginStatusCode: originConnection.statusCode };
}

/** S4: every agent process the Host reported spawning must be gone after shutdown. */
export function assertNoAgentSurvivors(sidecar) {
  const spawned = sidecar.lines
    .filter((line) => line.t === "host" && line.message?.type === "agent-process-spawned")
    .map((line) => line.message.pid)
    .filter((pid) => Number.isInteger(pid));
  const survivors = spawned.filter((pid) => isProcessAlive(pid));
  if (survivors.length > 0) fail("S4_AGENT_SURVIVORS", `agent processes still running: ${survivors.join(", ")}`);
  return { agentProcessesSpawned: spawned.length, agentSurvivors: 0 };
}
