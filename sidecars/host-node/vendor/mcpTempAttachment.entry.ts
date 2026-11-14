// Bundle entry: the Electron Main handlers for temporary text attachments and the MCP user
// directory, re-exported verbatim from the ZCode source (read-only; never edited from Harness).
// Built by `scripts/build-mcpTempAttachment-vendor.mjs` into `dist-vendor/mcpTempAttachmentRuntime.js`
// and loaded lazily by `src/mcpTempAttachmentHandlers.ts`, the single Harness owner of these
// requests (spec docs/specs/harness/phase-4-web-remote-control.md, section 12).
export { createTempTextAttachment } from "../../../../packages/desktop/src/main/tempTextAttachment.ts";
export {
  loadCliMcpFromUserDirectory,
  migrateLegacyCommonMcp,
  saveCliMcpToUserDirectory
} from "../../../../packages/desktop/src/main/mcpUserDirectory/index.ts";
