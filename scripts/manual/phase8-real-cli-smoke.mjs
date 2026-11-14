#!/usr/bin/env node
// MANUAL smoke test of harness-agent-hub against the REAL agent CLIs (spec phase-8-core.md §13, "manual").
// NOT part of any automated gate: it starts real Claude Code / Codex / agy sessions through their ACP adapters
// and therefore uses the user's own logins and consumes subscription quota.
//
// Usage (from Worse/):
//   node scripts/manual/phase8-real-cli-smoke.mjs --i-understand-this-uses-my-subscription [claude-code] [codex] [agy]
//
// Isolation: Harness data (sqlite, MEMORY.md) goes to a throwaway HARNESS_HOME and the prompt runs in a throwaway
// workspace; the CLIs keep their normal logins (HOME is NOT redirected, otherwise they would be logged out).
// Prerequisites: the ACP adapters on PATH (claude-agent-acp from @agentclientprotocol/claude-agent-acp, codex-acp from @agentclientprotocol/codex-acp; agy is expected to speak `agy --acp`, unverified).
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const CONFIRM = "--i-understand-this-uses-my-subscription";
const args = process.argv.slice(2);
if (!args.includes(CONFIRM)) {
  process.stderr.write(`refusing to run: this talks to real agent CLIs and uses your subscription. Pass ${CONFIRM}.\n`);
  process.exit(2);
}
const agents = args.filter((a) => a !== CONFIRM);
const selected = agents.length > 0 ? agents : ["claude-code", "codex", "agy"];
const worse = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const targetDir = process.env.CARGO_TARGET_DIR ? path.resolve(process.env.CARGO_TARGET_DIR) : path.join(worse, "target");
const exe = (name) => path.join(targetDir, "release", process.platform === "win32" ? `${name}.exe` : name);

const build = spawnSync("cargo", ["build", "--release", "-p", "harness-core-gate", "-p", "harness-agent-hub", "--bins"], {
  cwd: worse,
  stdio: "inherit",
  windowsHide: true,
});
if (build.status !== 0) process.exit(1);

const scratch = mkdtempSync(path.join(os.tmpdir(), "harness-smoke-"));
const harnessHome = path.join(scratch, "harness-home");
const workspace = path.join(scratch, "workspace");
mkdirSync(workspace, { recursive: true });
try {
  const run = spawnSync(exe("harness-core-gate"), ["smoke", ...selected, "--workspace", workspace], {
    cwd: workspace,
    env: { ...process.env, HARNESS_HOME: harnessHome, HARNESS_MEMORY_MCP: exe("harness-memory-mcp") },
    stdio: "inherit",
    windowsHide: true,
    timeout: 20 * 60 * 1000,
  });
  process.exitCode = run.status ?? 1;
} finally {
  rmSync(scratch, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
}
