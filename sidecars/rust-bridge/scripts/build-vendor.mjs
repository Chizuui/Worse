#!/usr/bin/env node
// Bundles ZCode's terminal policy (shell / cwd / env / font profile) into dist-vendor/ so the
// Rust PTY route computes exactly what the TS terminal service would. Runtime-only imports that
// those functions never use (`@zcode/rpc`, memory diagnostics) are stubbed to keep the bundle
// small and free of side effects; node-pty is only a type import there.
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";

const packageDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

const stubUnusedRuntimeImports = {
  name: "stub-unused-runtime-imports",
  setup(builder) {
    builder.onResolve({ filter: /^(@zcode\/rpc|#src\/memoryDiagnostics\.js)$/ }, (args) => ({
      path: args.path,
      namespace: "harness-stub",
    }));
    builder.onLoad({ filter: /.*/, namespace: "harness-stub" }, () => ({
      contents: "export class Emitter {}\nexport function registerMemoryDiagnosticsProvider() { return { dispose() {} }; }\n",
      loader: "js",
    }));
  },
};

try {
  await build({
    entryPoints: [path.join(packageDir, "vendor/terminalPolicy.entry.ts")],
    outfile: path.join(packageDir, "dist-vendor/terminalPolicy.js"),
    bundle: true,
    platform: "node",
    format: "esm",
    target: "node24",
    external: ["node-pty"],
    plugins: [stubUnusedRuntimeImports],
    logLevel: "warning",
    legalComments: "none",
    // 依赖里的 CommonJS 代码会 require("process") 等内置模块；ESM 包内没有 require，需用 createRequire 提供。
    banner: { js: 'import { createRequire as __harnessCreateRequire } from "node:module";\nconst require = __harnessCreateRequire(import.meta.url);' },
  });
} catch (error) {
  process.stderr.write(`build-vendor failed: ${error instanceof Error ? error.message : String(error)}\n`);
  process.exit(1);
}
