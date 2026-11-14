// Bundle entry: ZCode's Main-side remote workspace session manager and the discovery helpers the
// Electron IPC layer uses, re-exported verbatim from the ZCode source (read-only; never edited from
// Harness). Built by `scripts/build-vendor.mjs` into `dist-vendor/remoteWorkspace.js` and loaded by
// `src/remoteWorkspaceVendor.ts`. `electron` resolves to `./electronStub.ts`.
export { createRemoteWorkspaceSessionManager } from "../../../../packages/desktop/src/main/desktopRemoteSessions.ts";
export { createCanonicalWslTargetResolver } from "../../../../packages/desktop/src/main/desktopWslTargetResolver.ts";
export { resolveRemoteCdnBaseUrls } from "../../../../packages/desktop/src/main/remoteCdn.ts";
export { isDockerAvailable, listDockerContainers } from "../../../../packages/server/src/remote/docker-detect.ts";
export { listWSLDistros } from "../../../../packages/server/src/remote/wsl-detect.ts";
export { listSSHConfigAliasesFromLocalConfig } from "../../../../packages/services/src/system/sshConfigAlias.ts";
export {
  formatZodError,
  hostRemoteWorkspaceConnectFailedResponseSchema,
  remoteTargetSchema
} from "../../../../packages/shared/src/validation.ts";
export { normalizeRemoteConnectionError } from "../../../../packages/shared/src/source-server/connectionErrors.ts";
export { isPublicZCodeErrorCode } from "../../../../packages/shared/src/source-server/errorCodes.ts";
export { normalizeUnknownError } from "../../../../packages/shared/src/errors.ts";
export { ZCODE_VERSION } from "../../../../packages/shared/src/version.ts";
export { ZCODE_ENV } from "../../../../packages/shared/src/env.ts";
export { registerStubWindow } from "./electronStub.ts";
