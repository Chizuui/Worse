// One real ZCode Host (via the host-node sidecar) with an init-local attachment and an RPC
// client, configured by the given Rust-bridge env. Used once per run mode (ts / rust / fallback).
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { paths, repoRoot } from "../phase2/environment.mjs";
import { withTimeout } from "../phase2/gate.mjs";
import { ALLOWED_ORIGIN, SHUTDOWN_TIMEOUT_MS, awaitReady, buildInitLocalMessage, openAttachment } from "../phase2/scenarios.mjs";
import { startSidecar } from "../phase2/sidecarProcess.mjs";
import { connectRpc } from "../phase2/rpcClient.mjs";
import { attachSocket } from "../phase2/wsClient.mjs";

const CALL_TIMEOUT_MS = 90_000;
const TRACE_PATTERN = /rust-bridge route (\S+) (rust|surfaced-error|fallback|cancelled)(?: \((.*)\))?$/;

/** Errors cross the RPC boundary as `{name, message, code}` (contract section 8). */
function toErrorShape(error) {
  if (error instanceof Error) {
    const code = /** @type {{code?: unknown}} */ (error).code;
    return { name: error.name, message: error.message, ...(code === undefined ? {} : { code }) };
  }
  return { name: "NonError", message: String(error) };
}

/** Collects bridge trace lines from the sidecar's stderr (the phase2 tail buffer is capped). */
function collectTrace(child) {
  const entries = [];
  let remainder = "";
  child.stderr.on("data", (chunk) => {
    const parts = (remainder + chunk.toString()).split(/\r?\n/);
    remainder = parts.pop() ?? "";
    for (const line of parts) {
      const match = TRACE_PATTERN.exec(line);
      if (match) entries.push({ method: match[1], outcome: match[2], detail: match[3] });
    }
  });
  return entries;
}

export async function openHostSession({ modules, mode, env, step }) {
  const dataDir = path.join(paths.runDir, `data-${mode}`);
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
      ...env,
    },
  });
  const trace = collectTrace(sidecar.child);
  try {
    const ready = await awaitReady(sidecar);
    const url = `ws://127.0.0.1:${ready.wsPort}/v1/attach`;
    const initLocal = buildInitLocalMessage({ repoRoot, providerConfig: paths.providerConfig });
    const grant = await openAttachment(sidecar, initLocal, `${step}_ATTACH_OPEN`);
    const port = await attachSocket(modules.WebSocket, url, ALLOWED_ORIGIN, grant, `${step}_ATTACH_WS`);
    const connection = await connectRpc(modules.rpc, port, `${step}_INITIALIZE`);

    const call = async (channel, method, params) => {
      try {
        const value = await withTimeout(
          connection.client.getChannel(channel).call(method, [params]),
          CALL_TIMEOUT_MS,
          `${step}_CALL`,
          `${channel}.${method}`,
        );
        return { ok: true, value };
      } catch (error) {
        return { ok: false, error: toErrorShape(error) };
      }
    };

    const close = async () => {
      connection.dispose();
      port.close();
      sidecar.child.stdin.end();
      const exit = await sidecar.waitForExit(SHUTDOWN_TIMEOUT_MS, `${step}_EXIT`).catch(() => null);
      sidecar.kill();
      return exit;
    };

    /** Opens another attachment on the same Host (Phase 3b: re-subscription across attachments). */
    const attach = async (hostMessage, attachStep) => {
      const extraGrant = await openAttachment(sidecar, hostMessage, `${attachStep}_ATTACH_OPEN`);
      const extraPort = await attachSocket(modules.WebSocket, url, ALLOWED_ORIGIN, extraGrant, `${attachStep}_ATTACH_WS`);
      const extra = await connectRpc(modules.rpc, extraPort, `${attachStep}_INITIALIZE`);
      return {
        client: extra.client,
        close() {
          extra.dispose();
          extraPort.close();
        },
      };
    };

    return { call, close, trace, sidecar, client: connection.client, attach };
  } catch (error) {
    const tail = sidecar.stderrTail(40);
    if (tail) process.stderr.write(`[${mode}] sidecar stderr (tail):\n${tail}\n`);
    sidecar.kill();
    throw error;
  }
}
