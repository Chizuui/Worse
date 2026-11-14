// The bridge is consumed from its build output by relative path: the Worse sidecars are not
// linked into node_modules by the root install, and the relative layout (src/ and dist/ at the
// same depth) resolves identically from host-node's src/ (tests) and dist/ (runtime).
import { createRustBridge, readBridgeConfig, type RustBridge } from "../../rust-bridge/dist/index.js";
import type { PortDecorator } from "./attachBroker.ts";
import { log } from "./log.ts";

export interface RustServices {
  readonly decoratePort: PortDecorator;
  dispose(): Promise<void>;
  killNow(): void;
}

/**
 * Phase 3a Rust services (`HARNESS_RUST_SERVICES`). Inert when no service flag is
 * on: ports are returned unchanged and no `harness-host` is spawned.
 */
export function createRustServices(env: NodeJS.ProcessEnv, cwd: string): RustServices {
  const bridge: RustBridge = createRustBridge(readBridgeConfig(env, cwd), log);
  return {
    decoratePort: (port, hostMessage) => bridge.decoratePort(port, hostMessage),
    dispose: () => bridge.dispose(),
    killNow: () => bridge.killNow()
  };
}
