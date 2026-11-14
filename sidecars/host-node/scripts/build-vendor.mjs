#!/usr/bin/env node
// Bundles ZCode's Main-side remote workspace session manager (and its discovery helpers) into
// dist-vendor/remoteWorkspace.js so the Host sidecar can play the Electron Main role for remote
// workspaces without editing ZCode sources. `electron` is replaced by a one-window stub; the
// build-time defines match the Host bundle's (same build metadata, same ZCODE_ENV default as the
// Harness launch).
import path from "node:path";
import process from "node:process";
import { fileURLToPath, pathToFileURL } from "node:url";
import { build } from "esbuild";

const packageDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = path.resolve(packageDir, "../../..");

const { getBuildMetadata } = await import(
  pathToFileURL(path.join(repoRoot, "packages/desktop/scripts/build-metadata.mjs")).href
);
const metadata = getBuildMetadata();
const zcodeEnv = process.env.ZCODE_ENV?.trim().toLowerCase() === "test" ? "test" : "production";

const electronStub = {
  name: "harness-electron-stub",
  setup(builder) {
    builder.onResolve({ filter: /^electron$/ }, () => ({
      path: path.join(packageDir, "vendor/electronStub.ts"),
    }));
  },
};

try {
  await build({
    entryPoints: [path.join(packageDir, "vendor/remoteWorkspace.entry.ts")],
    outfile: path.join(packageDir, "dist-vendor/remoteWorkspace.js"),
    bundle: true,
    platform: "node",
    format: "esm",
    target: "node24",
    plugins: [electronStub],
    // 与 Host bundle 相同的运行时外部依赖（desktopNodeRuntimeExternals）：ssh2 等原生/CJS 依赖不能内联。
    external: ["ssh2", "node-pty", "undici", "yaml", "node-forge", "yauzl", "cpu-features"],
    define: {
      __ZCODE_VERSION__: JSON.stringify(metadata.appVersion),
      __ZCODE_MIN_COMPATIBLE_VERSION__: JSON.stringify(metadata.minCompatibleVersion),
      __ZCODE_COMMIT__: JSON.stringify(metadata.buildCommitId),
      __ZCODE_ENV__: JSON.stringify(zcodeEnv),
    },
    logLevel: "warning",
    legalComments: "none",
    // 依赖里的 CommonJS 代码会 require("process") 等内置模块；ESM 包内没有 require，需用 createRequire 提供。
    banner: {
      js: 'import { createRequire as __harnessCreateRequire } from "node:module";\nconst require = __harnessCreateRequire(import.meta.url);',
    },
  });
} catch (error) {
  process.stderr.write(`build-vendor failed: ${error instanceof Error ? error.message : String(error)}\n`);
  process.exit(1);
}
