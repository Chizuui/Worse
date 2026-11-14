#!/usr/bin/env node
// Bundles ZCode's Electron-Main browser owner (packages/desktop/src/main/browserView, unchanged) into
// dist-vendor/browserRuntime.js. Phase 5: Harness has no WebContentsView/<webview>, so every guest is a
// Chromium page target driven over CDP by src/browserGuest.ts. `electron` resolves to a stub whose
// `webContents.fromId` returns those CDP guests; Main's file logger resolves to a stderr logger.
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";

const packageDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

const browserStubs = {
  name: "harness-browser-stubs",
  setup(builder) {
    builder.onResolve({ filter: /^electron$/ }, () => ({
      path: path.join(packageDir, "vendor/browserElectronStub.ts"),
    }));
    // browserPlaywrightLocatorExecutor.ts imports Main's `../logger.js`（启动时写日志目录），这里替换为 stderr 版本。
    builder.onResolve({ filter: /^\.\.\/logger\.js$/ }, (args) =>
      args.importer.includes(`${path.sep}browserView${path.sep}`) || args.importer.includes("/browserView/")
        ? { path: path.join(packageDir, "vendor/browserLoggerStub.ts") }
        : undefined,
    );
  },
};

try {
  await build({
    entryPoints: [path.join(packageDir, "vendor/browserRuntime.entry.ts")],
    outfile: path.join(packageDir, "dist-vendor/browserRuntime.js"),
    bundle: true,
    platform: "node",
    format: "esm",
    target: "node24",
    plugins: [browserStubs],
    logLevel: "warning",
    legalComments: "none",
    // 依赖里的 CommonJS 代码会 require("process") 等内置模块；ESM 包内没有 require，需用 createRequire 提供。
    banner: {
      js: 'import { createRequire as __harnessCreateRequire } from "node:module";\nconst require = __harnessCreateRequire(import.meta.url);',
    },
  });
} catch (error) {
  process.stderr.write(`build-browser-vendor failed: ${error instanceof Error ? error.message : String(error)}\n`);
  process.exit(1);
}
