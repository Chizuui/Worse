import path from "node:path";
import { fileURLToPath } from "node:url";
import type { BridgeLogger } from "../src/logger.ts";
import { createRustHostSupervisor, type RustHostSupervisor } from "../src/process/supervisor.ts";

const testDir = path.dirname(fileURLToPath(import.meta.url));
export const FAKE_HOST = path.join(testDir, "fixtures/fake-rust-host.mjs");

export const silentLogger: BridgeLogger = () => undefined;

/** Supervisor that runs the Node fixture instead of the Rust binary. */
export function createFakeSupervisor(mode = "normal", binaryPath = process.execPath): RustHostSupervisor {
  process.env.FAKE_RUST_HOST_MODE = mode;
  return createRustHostSupervisor({
    binaryPath,
    args: ["--no-warnings", FAKE_HOST],
    cwd: testDir,
    logger: silentLogger
  });
}

export function nextTick(ms = 0): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Polls `predicate` every 10 ms until it holds or `timeoutMs` elapses. */
export async function waitFor(predicate: () => boolean, timeoutMs = 5_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error("waitFor timed out");
    await nextTick(10);
  }
}
