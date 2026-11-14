// Shared paths, constants and guest-document checks for the Phase 5 plugins gate (phase5-plugins.mjs).
import path from "node:path";
import { repoRoot } from "../phase2/environment.mjs";
import { fail } from "../phase2/gate.mjs";

export const hostNodeDir = path.join(repoRoot, "Worse/sidecars/host-node");
export const ownerModule = path.join(hostNodeDir, "dist/pluginSandbox.js");
export const runtimeBundle = path.join(hostNodeDir, "dist-vendor/pluginSandboxRuntime.js");
export const shellBundle = path.join(hostNodeDir, "dist-vendor/pluginSandboxShell.js");
export const assetsDir = path.join(repoRoot, "packages/desktop/out/plugin-sandbox");
export const runDir = path.join(repoRoot, "Worse/.work/tmp/phase5-plugins");
export const BUILD_COMMAND = "pnpm --dir Worse/sidecars/host-node build";
export const EMBEDDER = "http://localhost:1430";
export const CALL_TIMEOUT_MS = 15_000;
export const RPC_TIMEOUT_MS = 120_000;

export const h32Of = (appIdentity) => appIdentity.slice(0, 32);
export const winOrigin = (kind, appIdentity) => `http://hsandbox.${kind}-${h32Of(appIdentity)}.localhost`;
export const hsandboxUrl = (kind, appIdentity, rest) => `hsandbox://${kind}-${h32Of(appIdentity)}.localhost${rest}`;
export const headerMap = (pairs) => Object.fromEntries(pairs.map(([key, value]) => [key.toLowerCase(), value]));
export const decode = (value) => Buffer.from(value.bodyBase64 ?? "", "base64");

export function cspDirectives(csp) {
  return Object.fromEntries((csp ?? "").split("; ").map((directive) => [directive.split(" ", 1)[0], directive]));
}

// Tokens that would mean a guest document was handed a privileged channel.
export const PRIVILEGED_MARKERS = ["__TAURI", "ipc.localhost", "plugin_sandbox_call", "remote_control_call", "harness-web-remote-", "phase5-token"];

export function assertGuestDocument(step, value, extraMarkers = []) {
  const body = decode(value).toString("utf8");
  for (const marker of [...PRIVILEGED_MARKERS, ...extraMarkers]) {
    if (body.includes(marker)) fail(step, `served guest document contains a privileged marker ${marker}`);
  }
  const csp = cspDirectives(headerMap(value.headers)["content-security-policy"]);
  const connect = csp["connect-src"] ?? "";
  if (/ipc:|localhost|127\.0\.0\.1|ws:/.test(connect)) fail(step, `guest connect-src reaches the host: ${connect}`);
}
