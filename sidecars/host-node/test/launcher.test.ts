import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const MAIN = fileURLToPath(new URL("../src/main.ts", import.meta.url));
const fixture = (name: string): string => fileURLToPath(new URL(`./fixtures/${name}`, import.meta.url));

interface LauncherRun {
  code: number | null;
  stdoutLines: string[];
  stderr: string;
}

function runLauncher(
  hostEntry: string,
  stdinLines: string[],
  extraEnv: Record<string, string> = {}
): Promise<LauncherRun> {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, ["--no-warnings", "--experimental-strip-types", MAIN], {
      env: { ...process.env, HARNESS_HOST_ENTRY: hostEntry, HARNESS_SHUTDOWN_GRACE_MS: "5000", ...extraEnv },
      stdio: ["pipe", "pipe", "pipe"]
    });
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8").on("data", (chunk: string) => {
      stdout += chunk;
      if (stdout.includes('"t":"ready"') && !child.stdin.writableEnded) {
        for (const line of stdinLines) child.stdin.write(`${line}\n`);
        child.stdin.end();
      }
    });
    child.stderr.setEncoding("utf8").on("data", (chunk: string) => {
      stderr += chunk;
    });
    const timer = setTimeout(() => {
      child.kill();
      reject(new Error("launcher did not exit in time"));
    }, 15_000);
    child.once("exit", (code) => {
      clearTimeout(timer);
      resolve({ code, stdoutLines: stdout.split("\n").filter((line) => line.length > 0), stderr });
    });
  });
}

test("reports host_import_failed and exits 2 when the Host entry throws", async () => {
  const run = await runLauncher(fixture("throwing-host.mjs"), []);
  assert.equal(run.code, 2);
  const lines = run.stdoutLines.map((line) => JSON.parse(line) as Record<string, unknown>);
  assert.equal(lines.length, 1);
  assert.equal(lines[0]?.t, "fatal");
  assert.equal(lines[0]?.code, "host_import_failed");
});

test("keeps stdout NDJSON-only while the Host writes to stdout", async () => {
  const run = await runLauncher(fixture("noisy-host.mjs"), [JSON.stringify({ v: 1, t: "ping", id: "p1" })]);
  assert.equal(run.code, 0);
  const lines = run.stdoutLines.map((line) => JSON.parse(line) as Record<string, unknown>);
  assert.deepEqual(
    lines.map((line) => line.t),
    ["ready", "pong"]
  );
  assert.ok(lines.every((line) => line.v === 1));
  assert.match(run.stderr, /stdout diagnostics must be redirected/);
  assert.match(run.stderr, /noisy host: disposing/);
});

test("drains the event loop instead of hard-exiting when the Host calls process.exit", async () => {
  const startedAt = Date.now();
  const run = await runLauncher(fixture("lingering-host.mjs"), []);
  // Host 请求的退出码必须保留，且无需等到 5 s 宽限期兜底。
  assert.equal(run.code, 7);
  assert.ok(Date.now() - startedAt < 4_500, "the launcher waited for the shutdown grace fallback");
  assert.match(run.stderr, /lingering host: exit returned/);
  assert.doesNotMatch(run.stderr, /did not drain/);
});

test("falls back to a bounded hard exit when the Host leaves a ref'd timer", async () => {
  const run = await runLauncher(fixture("lingering-host.mjs"), [], { HARNESS_TEST_LINGER: "timer" });
  assert.equal(run.code, 7);
  assert.match(run.stderr, /event loop did not drain in time; forcing exit/);
});
