#!/usr/bin/env node
// Bundles ZCode's web remote-control manager (Electron Main code) into dist-vendor/ so host-node can
// run it unchanged next to the Host. Electron-only imports are stubbed: the manager itself only uses
// Electron types, and the two runtime users (renderer-action IPC, ARMS network telemetry) are
// replaced by Harness wiring in src/webRemoteHost.ts. `ws` stays external and resolves from the repo.
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";

const packageDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

const STUBS = {
  // webRemoteControlRendererAction.ts imports BrowserWindow/ipcMain; Harness injects its own
  // runRendererAction, so this path is never taken. The stub fails closed if it ever is.
  electron: [
    "const unavailable = () => { throw new Error('Electron is not available in Harness'); };",
    "export const BrowserWindow = { fromId: () => null };",
    "export const ipcMain = { on: unavailable, removeListener: () => {} };",
    "export class MessageChannelMain { constructor() { unavailable(); } }",
    "export default {};"
  ].join("\n"),
  // Desktop network telemetry reports through @arms/rum-electron; Harness has no ARMS reporter.
  telemetry: [
    "export function noteWebRemoteRelayConnectAttempt() {}",
    "export function noteWebRemoteRelayReconnect() {}"
  ].join("\n")
};

const stubElectronOnlyImports = {
  name: "stub-electron-only-imports",
  setup(builder) {
    builder.onResolve({ filter: /^electron$/ }, () => ({ path: "electron", namespace: "harness-stub" }));
    builder.onResolve({ filter: /desktopNetworkTelemetry(\.js)?$/ }, () => ({
      path: "telemetry",
      namespace: "harness-stub"
    }));
    builder.onLoad({ filter: /.*/, namespace: "harness-stub" }, (args) => ({
      contents: STUBS[args.path] ?? "export {};",
      loader: "js"
    }));
  }
};

try {
  await build({
    entryPoints: [path.join(packageDir, "vendor/webRemoteRuntime.entry.ts")],
    outfile: path.join(packageDir, "dist-vendor/webRemoteRuntime.js"),
    bundle: true,
    platform: "node",
    format: "esm",
    target: "node24",
    external: ["ws"],
    plugins: [stubElectronOnlyImports],
    logLevel: "warning",
    legalComments: "none",
    // 依赖里的 CommonJS 代码会 require("process") 等内置模块；ESM 包内没有 require，需用 createRequire 提供。
    banner: {
      js: 'import { createRequire as __harnessCreateRequire } from "node:module";\nconst require = __harnessCreateRequire(import.meta.url);'
    }
  });
} catch (error) {
  process.stderr.write(
    `build-webRemote-vendor failed: ${error instanceof Error ? error.message : String(error)}\n`
  );
  process.exit(1);
}
