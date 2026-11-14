// Simulated phone for the Phase 4 gate. Speaks the external relay V2 contract as the web client
// does (terminal role: HTTP token, ws2 auth_init, `data` payloads) and uses ZCode's real
// acknowledged rpc-frame adapter + ChannelClient for the replayable bridge. It holds no state the
// desktop needs; everything it knows comes from relay payloads.
import { GateError, withTimeout } from "../phase2/gate.mjs";

const PAYLOAD_TIMEOUT_MS = 20_000;
const INITIALIZE_TIMEOUT_MS = 30_000;

export function createPhone({ kit, WebSocket, relay, jwt }) {
  let ws = null;
  let generation = 0;
  let bridge = null;
  const payloads = [];
  const waiters = new Set();
  const pendingBridges = new Map();
  const relayMessages = [];

  const headers = { Authorization: `Bearer ${jwt}`, "Content-Type": "application/json" };

  const sendData = (payload) => {
    if (!ws || ws.readyState !== WebSocket.OPEN) return false;
    ws.send(JSON.stringify({ type: "data", payload, client_ts: Math.floor(Date.now() / 1000) }));
    return true;
  };

  const notify = (payload) => {
    payloads.push(payload);
    for (const waiter of Array.from(waiters)) {
      if (!waiter.predicate(payload)) continue;
      waiters.delete(waiter);
      clearTimeout(waiter.timer);
      waiter.resolve(payload);
    }
  };

  const waitForPayload = (predicate, step, label, timeoutMs = PAYLOAD_TIMEOUT_MS) => {
    const existing = payloads.find(predicate);
    if (existing) return Promise.resolve(existing);
    return new Promise((resolve, reject) => {
      const waiter = { predicate, resolve };
      waiter.timer = setTimeout(() => {
        waiters.delete(waiter);
        reject(new GateError(step, `phone timed out waiting for ${label}`));
      }, timeoutMs);
      waiters.add(waiter);
    });
  };

  const createBridgeClient = (ready) => {
    const identity = ready.bridge;
    const relayProtocol = kit.createAcknowledgedWebRemoteControlRelayProtocol({
      bridgeSessionId: identity.bridgeSessionId,
      bridgeGeneration: identity.bridgeGeneration,
      recoveryId: identity.recoveryId,
      measureFrameBytes: (frame) => kit.measureWebRemoteControlRpcRelayEnvelopeBytes(frame),
      sendFrame: (frame) => sendData(frame),
    });
    const client = new kit.ChannelClient(relayProtocol.protocol);
    const initialized = new Promise((resolve) => {
      const subscription = client.onDidInitialize(() => {
        subscription.dispose();
        resolve(Date.now());
      });
    });
    const degraded = [];
    relayProtocol.onDegraded((fault) => degraded.push(fault.reasonCode));
    return { identity, relayProtocol, client, initialized, degraded };
  };

  const onRelayMessage = (raw) => {
    let message;
    try {
      message = JSON.parse(raw.toString());
    } catch {
      return;
    }
    relayMessages.push(message.type);
    if (message.type !== "data") {
      notify({ relay: message });
      return;
    }
    const payload = message.payload;
    const kind = payload?.zcode_type;
    if (kind === "rpc-frame" || kind === "rpc-frame-ack") {
      bridge?.relayProtocol.acceptPayload(payload);
      return;
    }
    if (kind === "workspace-bridge-ready" && pendingBridges.has(payload.bridgeSessionId)) {
      // 必须在同一消息回调里建立 adapter：桌面在 ready 之后紧接着放行排队的 rpc-frame。
      bridge = createBridgeClient(payload);
      pendingBridges.get(payload.bridgeSessionId)(bridge);
      pendingBridges.delete(payload.bridgeSessionId);
    }
    notify(payload);
  };

  return {
    payloads,
    relayMessages,
    get bridge() {
      return bridge;
    },
    get connected() {
      return ws?.readyState === WebSocket.OPEN;
    },
    waitForPayload,
    sendData,

    /** Pairs with the online desktop device of the same account (relay V2 account pairing). */
    async connect(step) {
      const devices = await (await fetch(`${relay.httpUrl}/api/v1/remote/device`, { headers })).json();
      const device = devices.data?.find((entry) => entry.online);
      if (!device) throw new GateError(step, "no online desktop device is registered on the relay");
      const tokenResponse = await (
        await fetch(`${relay.httpUrl}/api/v1/remote/token`, {
          method: "POST",
          headers,
          body: JSON.stringify({ role: "terminal" }),
        })
      ).json();
      const token = tokenResponse.data?.token;
      if (typeof token !== "string") throw new GateError(step, "relay did not issue a terminal token");
      generation += 1;
      const socket = new WebSocket(`${relay.wsUrl}?token=${encodeURIComponent(token)}`);
      ws = socket;
      socket.on("message", onRelayMessage);
      await withTimeout(
        new Promise((resolve, reject) => {
          socket.once("open", resolve);
          socket.once("error", reject);
        }),
        10_000,
        step,
        "phone relay socket open",
      );
      const marker = payloads.length;
      socket.send(JSON.stringify({ type: "auth_init", device_sid: device.device_sid }));
      await waitForPayload(
        (payload) =>
          payloads.indexOf(payload) >= marker &&
          payload.relay?.type === "auth_ack" &&
          payload.relay.pair_status === "matched",
        step,
        "auth_ack matched",
      );
      return { deviceSid: device.device_sid, generation };
    },

    /** Opens a replayable workspace bridge and waits for the Host's Initialize over rpc-frame. */
    async openBridge({ workspaceKey, bridgeSessionId, bridgeGeneration, recoveryId }, step) {
      const ready = new Promise((resolve) => pendingBridges.set(bridgeSessionId, resolve));
      const requestId = `${bridgeSessionId}-open`;
      const sent = sendData({
        zcode_type: "workspace-bridge-open",
        requestId,
        bridgeSessionId,
        bridgeGeneration,
        ...(recoveryId ? { recoveryId } : {}),
        workspaceKey,
      });
      if (!sent) throw new GateError(step, "phone could not send workspace-bridge-open");
      const outcome = await Promise.race([
        ready,
        waitForPayload(
          (payload) => payload.zcode_type === "workspace-bridge-error" && payload.requestId === requestId,
          step,
          "workspace-bridge-ready",
        ).then((error) => {
          throw new GateError(step, `workspace-bridge-error: ${String(error.error)}`);
        }),
      ]);
      await withTimeout(outcome.initialized, INITIALIZE_TIMEOUT_MS, step, "bridge ChannelClient Initialize");
      return outcome;
    },

    /** Drops the bridge client after `bridge-degraded`, as the web page does before rebuilding. */
    discardBridge() {
      if (!bridge) return;
      bridge.client.dispose();
      bridge.relayProtocol.dispose();
      bridge = null;
    },

    close() {
      bridge?.client.dispose();
      bridge?.relayProtocol.dispose();
      ws?.terminate();
    },
  };
}
