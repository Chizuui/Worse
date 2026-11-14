#!/usr/bin/env node
// Bundles the plugin UI sandbox (spec docs/specs/harness/phase-5-plugins.md):
// - dist-vendor/pluginSandboxRuntime.js: ZCode Main's registry / CSP / protocol handler (Node, ESM),
//   bundled unchanged. They import only node:crypto / node:fs / node:path, zod and @zcode/shared.
// - dist-vendor/pluginSandboxShell.js: the Harness relay shell for the browser, which reuses ZCode's
//   renderer relay.ts unchanged.
// Both bundles must not depend on electron; the build fails if they do.
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";

const packageDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

function assertNoElectron(result, label) {
  const inputs = Object.keys(result.metafile.inputs);
  if (inputs.some((input) => /node_modules[\\/]electron[\\/]/.test(input) || input === "electron")) {
    throw new Error(`the ${label} bundle must not depend on electron`);
  }
  // host.ts / session.ts / guestPolicy.ts 依赖 Electron session 与 webContents；它们出现在 bundle 中说明入口导错了模块。
  if (inputs.some((input) => /pluginSandbox[\\/](host|session|guestPolicy|permissionGate)\.ts$/.test(input))) {
    throw new Error(`the ${label} bundle pulled an Electron-only pluginSandbox module`);
  }
}

try {
  const runtime = await build({
    entryPoints: [path.join(packageDir, "vendor/pluginSandbox.entry.ts")],
    outfile: path.join(packageDir, "dist-vendor/pluginSandboxRuntime.js"),
    bundle: true,
    platform: "node",
    format: "esm",
    target: "node24",
    logLevel: "warning",
    legalComments: "none",
    metafile: true,
    // 依赖里的 CommonJS 代码会 require 内置模块；ESM 包内没有 require，需用 createRequire 提供。
    banner: {
      js: 'import { createRequire as __harnessCreateRequire } from "node:module";\nconst require = __harnessCreateRequire(import.meta.url);'
    }
  });
  assertNoElectron(runtime, "plugin sandbox runtime");

  const shell = await build({
    entryPoints: [path.join(packageDir, "vendor/pluginSandboxShell.entry.ts")],
    outfile: path.join(packageDir, "dist-vendor/pluginSandboxShell.js"),
    bundle: true,
    platform: "browser",
    format: "iife",
    target: "es2022",
    logLevel: "warning",
    legalComments: "none",
    metafile: true
  });
  assertNoElectron(shell, "plugin sandbox shell");
} catch (error) {
  process.stderr.write(
    `build-pluginSandbox-vendor failed: ${error instanceof Error ? error.message : String(error)}\n`
  );
  process.exit(1);
}
