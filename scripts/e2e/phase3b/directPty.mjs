// B2: explicit shells (`cmd.exe` on Windows, `sh` elsewhere) are not reachable through the
// renderer API, so they are checked directly: harness-host `pty.spawn` over NDJSON against a
// node-pty oracle (the library the TS Host uses). Same commands, same expected exit code.
import { createRequire } from "node:module";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { repoRoot } from "../phase2/environment.mjs";
import { stripAnsi, waitUntil } from "./terminal.mjs";

const BRIDGE_DIST = path.join(repoRoot, "Worse/sidecars/rust-bridge/dist/index.js");

function plan() {
  if (process.platform === "win32") {
    return { shell: "cmd.exe", lines: ["echo XP^LICIT", "exit 3"], eol: "\r\n", marker: "XPLICIT" };
  }
  return { shell: "sh", lines: ["printf '%s%s\\n' XP LICIT", "exit 3"], eol: "\n", marker: "XPLICIT" };
}

async function runRust(binaryPath, cwd, p) {
  const bridge = await import(pathToFileURL(BRIDGE_DIST).href);
  const host = await bridge.startRustHostProcess({ binaryPath, cwd, onStderr: () => undefined, onDiagnostic: () => undefined });
  try {
    let output = "";
    let exitCode;
    host.connection.onNotification((method, params) => {
      if (method === "$/pty/output") output += params.data;
      if (method === "$/pty/exit") exitCode = params.exitCode;
    });
    const startedAt = performance.now();
    const spawned = await host.connection.call("pty.spawn", { shell: p.shell, args: [], cwd, cols: 80, rows: 24 }, { timeoutMs: 15_000 });
    const spawnMs = performance.now() - startedAt;
    await new Promise((resolve) => setTimeout(resolve, 500));
    for (const line of p.lines) {
      await host.connection.call("pty.write", { sessionId: spawned.sessionId, data: `${line}${p.eol}` }, { timeoutMs: 10_000 });
    }
    await waitUntil(() => exitCode !== undefined, 15_000);
    return { marker: stripAnsi(output).includes(p.marker), exitCode, spawnMs };
  } finally {
    await host.dispose();
  }
}

async function runNodePty(cwd, p) {
  const requireFromServices = createRequire(path.join(repoRoot, "packages/services/package.json"));
  const pty = requireFromServices("node-pty");
  const startedAt = performance.now();
  const term = pty.spawn(p.shell, [], { cols: 80, rows: 24, cwd, env: process.env });
  const spawnMs = performance.now() - startedAt;
  let output = "";
  let exitCode;
  term.onData((data) => {
    output += data;
  });
  term.onExit((event) => {
    exitCode = event.exitCode;
  });
  await new Promise((resolve) => setTimeout(resolve, 500));
  for (const line of p.lines) term.write(`${line}${p.eol}`);
  await waitUntil(() => exitCode !== undefined, 15_000);
  if (exitCode === undefined) term.kill();
  return { marker: stripAnsi(output).includes(p.marker), exitCode, spawnMs };
}

export async function runExplicitShellCheck(binaryPath, cwd) {
  const p = plan();
  const oracle = await runNodePty(cwd, p);
  const rust = await runRust(binaryPath, cwd, p);
  return { shell: p.shell, oracle, rust };
}
