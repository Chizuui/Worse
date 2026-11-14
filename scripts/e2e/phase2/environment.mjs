import { existsSync } from "node:fs";
import { rm } from "node:fs/promises";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { delay, fail } from "./gate.mjs";

const moduleDir = path.dirname(fileURLToPath(import.meta.url));

/** Repository root: Worse/scripts/e2e/phase2 -> four levels up. */
export const repoRoot = path.resolve(moduleDir, "../../../..");

export const paths = {
  launcher: path.join(repoRoot, "Worse/sidecars/host-node/dist/main.js"),
  hostEntry: path.join(repoRoot, "packages/desktop/out/host/index.js"),
  agentCli: path.join(repoRoot, "apps/zcode-cli/packages/cli/dist/zcode.cjs"),
  rpcDist: path.join(repoRoot, "packages/rpc/dist/index.js"),
  providerConfig: path.join(repoRoot, "config/provider/zcode-builtin.json"),
  runDir: path.join(repoRoot, "Worse/.work/tmp/phase2-e2e"),
  dataDir: path.join(repoRoot, "Worse/.work/tmp/phase2-e2e/data"),
};

const BUILD_LAUNCHER_COMMAND = "node Worse/scripts/build-host-sidecar.mjs";
const BUILD_HOST_COMMAND = "node Worse/scripts/build-host-sidecar.mjs --rebuild-host";
const BUILD_AGENT_COMMAND = "pnpm build:desktop-agent-cli";
const BUILD_RPC_COMMAND = "pnpm exec tsc -b packages/rpc";

/** Fails with the exact build command when a prerequisite artifact is missing. */
export function verifyArtifacts() {
  if (!existsSync(paths.launcher)) {
    fail("BUILD_LAUNCHER", "Worse/sidecars/host-node/dist/main.js is missing", BUILD_LAUNCHER_COMMAND);
  }
  if (!existsSync(paths.hostEntry)) {
    fail("BUILD_HOST", "packages/desktop/out/host/index.js is missing", BUILD_HOST_COMMAND);
  }
  if (!existsSync(paths.agentCli)) {
    fail("BUILD_AGENT", "apps/zcode-cli/packages/cli/dist/zcode.cjs is missing", BUILD_AGENT_COMMAND);
  }
  if (!existsSync(paths.providerConfig)) {
    fail("BUILD_PROVIDER", "config/provider/zcode-builtin.json is missing");
  }
  if (!existsSync(paths.rpcDist)) {
    fail("BUILD_RPC", "packages/rpc/dist/index.js is missing", BUILD_RPC_COMMAND);
  }
}

/** Loads `ws` (resolved from the repo root) and the built `@zcode/rpc`. */
export async function loadModules() {
  const requireFromRepo = createRequire(path.join(repoRoot, "package.json"));
  let wsEntry;
  try {
    wsEntry = requireFromRepo.resolve("ws");
  } catch {
    fail("BUILD_WS", "the 'ws' package does not resolve from the repository root", "pnpm install");
  }
  const wsModule = await import(pathToFileURL(wsEntry).href);
  const WebSocket = wsModule.WebSocket ?? wsModule.default?.WebSocket ?? wsModule.default;
  if (typeof WebSocket !== "function") {
    fail("BUILD_WS", "the resolved 'ws' package does not export WebSocket");
  }
  const rpc = await import(pathToFileURL(paths.rpcDist).href);
  if (typeof rpc.MessagePortProtocol !== "function" || typeof rpc.ChannelClient !== "function") {
    fail("BUILD_RPC", "packages/rpc/dist/index.js lacks MessagePortProtocol/ChannelClient", BUILD_RPC_COMMAND);
  }
  return { WebSocket, rpc };
}

/** Removes the isolated run directory; retries because Windows may hold SQLite handles briefly. */
export async function removeRunDir() {
  for (let attempt = 0; attempt < 6; attempt += 1) {
    try {
      await rm(paths.runDir, { recursive: true, force: true });
      return true;
    } catch {
      await delay(250 * (attempt + 1));
    }
  }
  return false;
}
