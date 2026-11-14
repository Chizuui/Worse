// Bundle entry: Electron Main's plugin UI sandbox registry, CSP builders and protocol handler,
// re-exported verbatim from the ZCode source (read-only; never edited from Harness). Only the
// Electron-free modules are included: host.ts / session.ts / guestPolicy.ts / permissionGate.ts
// need Electron sessions and webContents and are replaced by Harness (spec
// docs/specs/harness/phase-5-plugins.md). Built by `scripts/build-pluginSandbox-vendor.mjs` into
// `dist-vendor/pluginSandboxRuntime.js` and loaded lazily by `src/pluginSandbox.ts`.
export {
  PLUGIN_SANDBOX_PARTITION_PREFIX,
  PluginSandboxQuotaError,
  buildPluginSandboxPartition,
  buildPluginSandboxPluginOrigin,
  buildPluginSandboxShellUrl
} from "../../../../packages/desktop/src/main/pluginSandbox/contract.ts";
export {
  PLUGIN_SANDBOX_SHELL_CSP,
  buildPluginSandboxCsp,
  buildPluginSandboxPermissionsPolicy
} from "../../../../packages/desktop/src/main/pluginSandbox/csp.ts";
export {
  PLUGIN_SANDBOX_USER_GESTURE_TTL_MS,
  createPluginSandboxRegistry
} from "../../../../packages/desktop/src/main/pluginSandbox/registry.ts";
export {
  createPluginSandboxProtocolHandler,
  parsePluginSandboxUrl
} from "../../../../packages/desktop/src/main/pluginSandbox/protocol.ts";
export { createGenUiVendorResources } from "../../../../packages/desktop/src/main/pluginSandbox/genUiVendor.ts";
export {
  hostPluginSandboxRegisterRequestResponseSchema,
  hostPluginSandboxRegisterResultMessageSchema
} from "../../../../packages/shared/src/validation.ts";
export { buildMcpAppsIframeAllow } from "../../../../packages/shared/src/mcp-apps/contract.ts";
