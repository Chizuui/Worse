import { GateError, fail, withTimeout } from "./gate.mjs";

const INITIALIZE_TIMEOUT_MS = 60_000;
const RPC_TIMEOUT_MS = 60_000;

/** Builds `MessagePortProtocol` + `ChannelClient` and waits for the Host's Initialize. */
export async function connectRpc(rpc, port, step) {
  const protocol = new rpc.MessagePortProtocol(port);
  const client = new rpc.ChannelClient(protocol);
  let subscription;
  const initialized = new Promise((resolve) => {
    subscription = client.onDidInitialize(() => resolve(Date.now()));
  });
  try {
    const initializedAt = await withTimeout(initialized, INITIALIZE_TIMEOUT_MS, step, "ChannelClient Initialize");
    return {
      client,
      initializedAt,
      dispose() {
        client.dispose();
        protocol.disconnect();
      },
    };
  } catch (error) {
    client.dispose();
    protocol.disconnect();
    throw error instanceof GateError ? error : new GateError(step, String(error));
  } finally {
    subscription?.dispose();
  }
}

/** Calls `setting.get` and requires a non-empty settings object. */
export async function callSettingGet(client, step) {
  const settings = await withTimeout(client.getChannel("setting").call("get"), RPC_TIMEOUT_MS, step, "setting.get");
  if (!settings || typeof settings !== "object" || Array.isArray(settings) || Object.keys(settings).length === 0) {
    fail(step, "setting.get did not return a non-empty object");
  }
  return settings;
}
