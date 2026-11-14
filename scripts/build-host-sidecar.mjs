#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { existsSync, statSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, "../..");

const SIDECAR_DIR = path.join(repoRoot, "Worse/sidecars/host-node");
const SIDECAR_ENTRY = path.join(SIDECAR_DIR, "dist/main.js");
const TERMINAL_POLICY = path.join(repoRoot, "Worse/sidecars/rust-bridge/dist-vendor/terminalPolicy.js");
const HOST_ENTRY = path.join(repoRoot, "packages/desktop/out/host/index.js");
const AGENT_CLI = path.join(repoRoot, "apps/zcode-cli/packages/cli/dist/zcode.cjs");

const SIDECAR_BUILD_COMMAND = "pnpm --dir Worse/sidecars/host-node build";
const HOST_REBUILD_COMMAND =
  "pnpm --filter @zcode/desktop exec tsup --config tsup.config.ts";
const AGENT_CLI_BUILD_COMMAND = "pnpm build:desktop-agent-cli";

function write(message) {
  process.stdout.write(`${message}\n`);
}

function writeError(message) {
  process.stderr.write(`${message}\n`);
}

function fail(message, command) {
  writeError(`error: ${message}`);
  if (command) {
    writeError(`run: ${command}`);
  }
  process.exit(1);
}

function isFile(target) {
  try {
    return statSync(target).isFile();
  } catch {
    return false;
  }
}

function run(command, args) {
  const commandLine = [command, ...args].join(" ");
  write(`$ ${commandLine}`);
  // pnpm is a .cmd shim on Windows and needs a shell; Node 24 deprecates passing an
  // args array together with `shell: true` (DEP0190), so the shell gets one command
  // line. Every argument here is a fixed literal, so nothing needs escaping.
  const useShell = process.platform === "win32";
  const result = useShell
    ? spawnSync(commandLine, { cwd: repoRoot, env: process.env, stdio: "inherit", shell: true })
    : spawnSync(command, args, { cwd: repoRoot, env: process.env, stdio: "inherit" });
  if (result.error) {
    fail(
      `failed to run command (${result.error.message})`,
      [command, ...args].join(" "),
    );
  }
  return result.status ?? 1;
}

const args = process.argv.slice(2);
const rebuildHost = args.includes("--rebuild-host");
const knownArgs = new Set(["--rebuild-host", "--help", "-h"]);
for (const arg of args) {
  if (!knownArgs.has(arg)) {
    writeError(`error: unknown argument '${arg}'`);
    writeError(
      "usage: node Worse/scripts/build-host-sidecar.mjs [--rebuild-host]",
    );
    process.exit(1);
  }
}

if (args.includes("--help") || args.includes("-h")) {
  write("usage: node Worse/scripts/build-host-sidecar.mjs [--rebuild-host]");
  write("");
  write("Builds the Node Host sidecar and verifies the Host and agent artifacts.");
  write(
    "  --rebuild-host  rebuild packages/desktop/out/host/index.js with tsup",
  );
  process.exit(0);
}

if (!existsSync(path.join(SIDECAR_DIR, "package.json"))) {
  fail(
    "the host sidecar package is missing at Worse/sidecars/host-node",
    SIDECAR_BUILD_COMMAND,
  );
}

write("[host-sidecar] building node launcher");
const sidecarStatus = run("pnpm", ["--dir", "Worse/sidecars/host-node", "build"]);
if (sidecarStatus !== 0) {
  fail(
    `sidecar build failed with exit code ${sidecarStatus}`,
    SIDECAR_BUILD_COMMAND,
  );
}

if (!isFile(SIDECAR_ENTRY)) {
  fail(
    "sidecar build did not produce Worse/sidecars/host-node/dist/main.js",
    SIDECAR_BUILD_COMMAND,
  );
}
write("[host-sidecar] dist/main.js ok");

// Phase 4: web remote control loads ZCode's manager bundle produced by the same host-node build.
const WEB_REMOTE_RUNTIME = path.join(SIDECAR_DIR, "dist-vendor/webRemoteRuntime.js");
if (!isFile(WEB_REMOTE_RUNTIME)) {
  fail(
    "sidecar build did not produce Worse/sidecars/host-node/dist-vendor/webRemoteRuntime.js",
    SIDECAR_BUILD_COMMAND,
  );
}
write("[host-sidecar] dist-vendor/webRemoteRuntime.js ok");

// Phase 4 fix B: temp text attachments + MCP user directory run ZCode Main's handlers from this bundle.
const MCP_TEMP_ATTACHMENT_RUNTIME = path.join(SIDECAR_DIR, "dist-vendor/mcpTempAttachmentRuntime.js");
if (!isFile(MCP_TEMP_ATTACHMENT_RUNTIME)) {
  fail(
    "sidecar build did not produce Worse/sidecars/host-node/dist-vendor/mcpTempAttachmentRuntime.js",
    SIDECAR_BUILD_COMMAND,
  );
}
write("[host-sidecar] dist-vendor/mcpTempAttachmentRuntime.js ok");

// Phase 4: remote workspaces load ZCode Main's remote session manager bundle (scripts/build-vendor.mjs).
const REMOTE_WORKSPACE_RUNTIME = path.join(SIDECAR_DIR, "dist-vendor/remoteWorkspace.js");
if (!isFile(REMOTE_WORKSPACE_RUNTIME)) {
  fail(
    "sidecar build did not produce Worse/sidecars/host-node/dist-vendor/remoteWorkspace.js",
    SIDECAR_BUILD_COMMAND,
  );
}
write("[host-sidecar] dist-vendor/remoteWorkspace.js ok");

// Phase 5 (browser): ZCode Main's BrowserGuestManager bundle (scripts/build-browser-vendor.mjs).
const BROWSER_RUNTIME = path.join(SIDECAR_DIR, "dist-vendor/browserRuntime.js");
if (!isFile(BROWSER_RUNTIME)) {
  fail("sidecar build did not produce Worse/sidecars/host-node/dist-vendor/browserRuntime.js", SIDECAR_BUILD_COMMAND);
}
write("[host-sidecar] dist-vendor/browserRuntime.js ok");

// The Rust PTY route needs ZCode's terminal policy bundle; the rust-bridge build produces it.
if (!isFile(TERMINAL_POLICY)) {
  fail(
    "rust-bridge build did not produce Worse/sidecars/rust-bridge/dist-vendor/terminalPolicy.js",
    "pnpm --dir Worse/sidecars/rust-bridge build",
  );
}
write("[host-sidecar] rust-bridge dist-vendor/terminalPolicy.js ok");

if (!isFile(HOST_ENTRY)) {
  if (!rebuildHost) {
    fail(
      "the ZCode Host bundle is missing at packages/desktop/out/host/index.js",
      `${HOST_REBUILD_COMMAND}\n  or: node Worse/scripts/build-host-sidecar.mjs --rebuild-host`,
    );
  }
  write("[host-sidecar] rebuilding the ZCode Host bundle");
  const hostStatus = run("pnpm", [
    "--filter",
    "@zcode/desktop",
    "exec",
    "tsup",
    "--config",
    "tsup.config.ts",
  ]);
  if (hostStatus !== 0) {
    fail(`host rebuild failed with exit code ${hostStatus}`, HOST_REBUILD_COMMAND);
  }
  if (!isFile(HOST_ENTRY)) {
    fail(
      "host rebuild did not produce packages/desktop/out/host/index.js",
      HOST_REBUILD_COMMAND,
    );
  }
}
write("[host-sidecar] packages/desktop/out/host/index.js ok");

if (!isFile(AGENT_CLI)) {
  fail(
    "the desktop agent CLI is missing at apps/zcode-cli/packages/cli/dist/zcode.cjs",
    AGENT_CLI_BUILD_COMMAND,
  );
}
write("[host-sidecar] apps/zcode-cli/packages/cli/dist/zcode.cjs ok");

write("[host-sidecar] ready");