// Bundle entry: ZCode's Electron-Main browser owner (BrowserGuestManager and the CDP executors it
// drives), re-exported verbatim from the ZCode source (read-only; never edited from Harness). Built by
// `scripts/build-browser-vendor.mjs` into `dist-vendor/browserRuntime.js` and loaded by
// `src/browserVendor.ts`. `electron` resolves to `./browserElectronStub.ts`, Main's file logger to
// `./browserLoggerStub.ts`.
export { BrowserGuestManager } from "../../../../packages/desktop/src/main/browserView/browserGuestManager.ts";
export { buildBrowserViewCloseTabNotification } from "../../../../packages/desktop/src/main/browserView/browserCloseTabNotification.ts";
// The Host-side bridge is bundled only so e2e gates can drive the exact Host → Main request shapes.
export { createBrowserControlMainBridge } from "../../../../packages/desktop/src/host/browserControlMainBridge.ts";
export { browserCommandResultSchema } from "../../../../packages/shared/src/browser-use/result.ts";
export { browserCommandSchema } from "../../../../packages/shared/src/browser-use/commands.ts";
export { registerGuestWebContents } from "./browserElectronStub.ts";
