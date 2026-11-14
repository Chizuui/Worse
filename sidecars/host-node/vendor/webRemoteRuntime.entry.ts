// Bundle entry: ZCode's web remote-control owner (Electron Main's `webRemoteControlManager`) and the
// small services it needs, re-exported verbatim from the ZCode source (read-only; never edited from
// Harness). Built by `scripts/build-webRemote-vendor.mjs` into `dist-vendor/webRemoteRuntime.js` and
// loaded lazily by `src/webRemoteHost.ts`, so a Host that never starts remote control never loads it.
export { createWebRemoteControlManager } from "../../../../packages/desktop/src/main/webRemoteControlManager.ts";
export { createWebRemoteControlFeatureGate } from "../../../../packages/desktop/src/main/webRemoteControlFeatureGate.ts";
export { createWebRemoteControlDeviceStorage } from "../../../../packages/desktop/src/main/web-remote-relay/webRemoteControlDeviceStorage.ts";
export { createCredentialService } from "../../../../packages/services/src/credential/credentialService.ts";
export { createSettingService } from "../../../../packages/services/src/setting/settingService.ts";
export {
  DEFAULT_ZCODE_ENDPOINT_ORIGIN,
  buildZCodeEndpointUrls,
  resolveRuntimeZCodeEndpointOrigin,
  resolveWebRemoteControlRelayWsUrl
} from "../../../../packages/shared/src/zcodeEndpoint.ts";
export {
  webRemoteControlReconnectWorkspaceResultSchema,
  webRemoteControlRendererActionResultSchema,
  webRemoteControlWorkspaceTargetsSchema
} from "../../../../packages/shared/src/validation.ts";
export {
  WEB_REMOTE_CONTROL_WORKSPACE_RECONNECT_TIMEOUT_MS,
  webRemoteControlAuthStateChangeSchema
} from "../../../../packages/shared/src/web-remote-control.ts";
export { WEB_REMOTE_CONTROL_RENDERER_ACTION_TIMEOUT_MS } from "../../../../packages/shared/src/web-remote-relay/domain/webRemoteControlActions.ts";
