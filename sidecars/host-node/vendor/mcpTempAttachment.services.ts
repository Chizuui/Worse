// Narrow stand-in for `@zcode/services/node` inside the MCP / temp attachment bundle. The Main
// handlers import only these three helpers from the services barrel; re-exporting them from their
// own ZCode source files keeps the code verbatim while leaving out the barrel's unrelated services
// (the full barrel tripled the bundle size). Aliased by scripts/build-mcpTempAttachment-vendor.mjs.
export { getZCodeDataRootDir } from "../../../../packages/services/src/paths.ts";
export {
  readJsonObjectFileForUpdate,
  updateJsonObjectFile
} from "../../../../packages/services/src/fs/atomicFileUtils.ts";
