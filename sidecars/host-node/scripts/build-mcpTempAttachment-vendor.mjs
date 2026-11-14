#!/usr/bin/env node
// Bundles ZCode's Main handlers for temporary text attachments and the MCP user directory into
// dist-vendor/ so host-node can run them unchanged (spec phase-4-web-remote-control.md, section 12).
// Main's `logger.ts` creates the Electron log directory and starts log retention when it is
// imported; Harness replaces it with a stderr logger in the host-node log format. Nothing else is
// stubbed: the handlers only use node:fs/path/os and @zcode/services/node file helpers.
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";

const packageDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

const MAIN_LOGGER_STUB = [
  "const write = (level, args) => {",
  "  const text = args.map((value) => value instanceof Error ? (value.stack ?? value.message) : typeof value === 'string' ? value : (() => { try { return JSON.stringify(value); } catch { return String(value); } })()).join(' ');",
  "  process.stderr.write(`[harness-host-node] ${level} ${text}\\n`);",
  "};",
  "export const logger = {",
  "  debug: () => {},",
  "  info: (...args) => write('info', args),",
  "  warn: (...args) => write('warn', args),",
  "  error: (...args) => write('error', args)",
  "};",
  "export default logger;"
].join("\n");

const stubMainLogger = {
  name: "stub-main-logger",
  setup(builder) {
    // Only Main's own logger module (packages/desktop/src/main/logger.ts), imported as "../logger.js".
    builder.onResolve({ filter: /(^|\/)logger(\.js)?$/ }, (args) => {
      const resolved = path.resolve(args.resolveDir, args.path.replace(/\.js$/, ".ts"));
      const mainLogger = path.resolve(packageDir, "../../../packages/desktop/src/main/logger.ts");
      return resolved === mainLogger ? { path: "main-logger", namespace: "harness-stub" } : undefined;
    });
    builder.onLoad({ filter: /.*/, namespace: "harness-stub" }, () => ({ contents: MAIN_LOGGER_STUB, loader: "js" }));
  }
};

const narrowServicesBarrel = {
  name: "narrow-services-barrel",
  setup(builder) {
    // The handlers use three helpers from @zcode/services/node; resolve them from their source files.
    builder.onResolve({ filter: /^@zcode\/services\/node$/ }, () => ({
      path: path.join(packageDir, "vendor/mcpTempAttachment.services.ts")
    }));
  }
};

try {
  const result = await build({
    entryPoints: [path.join(packageDir, "vendor/mcpTempAttachment.entry.ts")],
    outfile: path.join(packageDir, "dist-vendor/mcpTempAttachmentRuntime.js"),
    bundle: true,
    platform: "node",
    format: "esm",
    target: "node24",
    plugins: [stubMainLogger, narrowServicesBarrel],
    logLevel: "warning",
    legalComments: "none",
    metafile: true,
    // 依赖里的 CommonJS 代码会 require 内置模块；ESM 包内没有 require，需用 createRequire 提供。
    banner: {
      js: 'import { createRequire as __harnessCreateRequire } from "node:module";\nconst require = __harnessCreateRequire(import.meta.url);'
    }
  });
  // 修复依据：Main logger 在 import 时会创建 Electron 日志目录；若桩没有命中，bundle 会带上它并在 sidecar 里写目录。
  const inputs = Object.keys(result.metafile.inputs);
  if (inputs.some((input) => /packages[\\/]desktop[\\/]src[\\/]main[\\/]logger\.ts$/.test(input)) || !inputs.includes("harness-stub:main-logger")) {
    throw new Error("Main logger.ts was not replaced by the Harness stub");
  }
  if (inputs.some((input) => /node_modules[\\/]electron[\\/]/.test(input) || input === "electron")) {
    throw new Error("the MCP / temp attachment bundle must not depend on electron");
  }
} catch (error) {
  process.stderr.write(
    `build-mcpTempAttachment-vendor failed: ${error instanceof Error ? error.message : String(error)}\n`
  );
  process.exit(1);
}
