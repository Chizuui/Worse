#!/usr/bin/env node
// Phase 3c gate (spec: Worse/docs/specs/harness/phase-3c-packaging-tools.md, section 8).
//
// Nothing was ported for tool discovery or diff computation (spec sections 6 and 7), so this gate
// proves the packaged layout instead: it stages the `host/` resource into a directory outside the
// repository, checks every file of the layout (K1), runs the staged Host from the staged root with
// all dev-tree overrides removed (K2), opens a Rust PTY whose OpenConsole.exe must be the staged
// copy (K3), runs the staged ripgrep (K4) and checks that no staged process survives shutdown (K5).
//
// Usage: node Worse/scripts/e2e/phase3c-tools-diff.mjs [--keep] [--layout <existing host/ dir>]
// --layout skips staging and checks an existing layout, e.g. the `host/` of an extracted installer.
import { execFile, execFileSync } from "node:child_process";
import { mkdtemp, readFile, rm, stat } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { pathToFileURL } from "node:url";
import { promisify } from "node:util";
import { delay, fail, GateError, withTimeout } from "./phase2/gate.mjs";
import { loadModules, repoRoot, verifyArtifacts } from "./phase2/environment.mjs";
import { ALLOWED_ORIGIN, SHUTDOWN_TIMEOUT_MS, awaitReady, buildInitLocalMessage, openAttachment } from "./phase2/scenarios.mjs";
import { startSidecar } from "./phase2/sidecarProcess.mjs";
import { connectRpc } from "./phase2/rpcClient.mjs";
import { attachSocket } from "./phase2/wsClient.mjs";
import { openTerminal, waitUntil } from "./phase3b/terminal.mjs";
import { installIsolatedHome } from "./lib/isolatedHome.mjs";

// 修复说明：Host/Agent 按 HOME/USERPROFILE 解析 ~/.zcode，曾把测试数据写进用户真实设置；
// 在任何子进程启动前切到一次性 home，并在退出时检查真实 ~/.zcode/v2 文件未被改动（HOME_LEAK）。
installIsolatedHome("phase3c-tools-diff");

const execFileAsync = promisify(execFile);
const isWindows = process.platform === "win32";
const exe = isWindows ? ".exe" : "";
const toolKey = `${process.platform}-${process.arch}`;
const CALL_TIMEOUT_MS = 60_000;
const TRACE = /rust-bridge route (\S+) (rust|surfaced-error|fallback|cancelled)/;

/** Spec section 4: every file a packaged Host needs, relative to the layout root. */
const REQUIRED = [
  "harness-host-manifest.json",
  "Worse/sidecars/host-node/dist/main.js",
  "Worse/sidecars/host-node/package.json",
  "Worse/sidecars/rust-bridge/dist/index.js",
  "Worse/sidecars/rust-bridge/package.json",
  "Worse/sidecars/rust-bridge/dist-vendor/terminalPolicy.js",
  `Worse/target/release/harness-host${exe}`,
  "packages/desktop/out/host/index.js",
  `packages/desktop/bundled-tools/${toolKey}/ripgrep/rg${exe}`,
  "apps/zcode-cli/packages/cli/dist/zcode.cjs",
  "config/provider/zcode-builtin.json",
  "config/provider/zcode-builtin.test.json",
  "node_modules/node-pty/package.json",
  "node_modules/ws/package.json",
  ...(isWindows ? ["Worse/target/release/conpty.dll", "Worse/target/release/OpenConsole.exe"] : []),
];
/** Variables that could point the staged Host back into the checkout. */
const DEV_OVERRIDES = [
  "HARNESS_RUST_HOST",
  "HARNESS_CONPTY_DIR",
  "HARNESS_REPO_ROOT",
  "HARNESS_HOST_ENTRY",
  "NODE_PATH",
  "ZCODE_RG_BINARY",
  "ZCODE_UGREP_BINARY",
  "ZCODE_BFS_BINARY",
  "ZCODE_SERVER_RUNTIME_ROOT",
];

const log = (message) => process.stdout.write(`${message}\n`);
const isFile = (target) => stat(target).then((s) => s.isFile(), () => false);
const within = (child, parent) => {
  const rel = path.relative(parent.toLowerCase(), child.toLowerCase());
  return rel !== "" && !rel.startsWith("..") && !path.isAbsolute(rel);
};

/** Processes whose image lives under `root` (Windows: WMI ExecutablePath; Unix: /proc exe links). */
async function processesUnder(root) {
  if (isWindows) {
    const script =
      "Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath } | " +
      "ForEach-Object { [pscustomobject]@{ pid = $_.ProcessId; name = $_.Name; path = $_.ExecutablePath } } | ConvertTo-Json -Compress";
    const { stdout } = await execFileAsync("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", script], {
      windowsHide: true,
      maxBuffer: 64 * 1024 * 1024,
    });
    const rows = JSON.parse(stdout || "[]");
    return (Array.isArray(rows) ? rows : [rows]).filter((row) => within(row.path, root));
  }
  const { stdout } = await execFileAsync("ps", ["-eo", "pid=,args="]);
  return stdout
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.includes(root))
    .map((line) => ({ pid: Number(line.split(/\s+/)[0]), name: line.split(/\s+/)[1], path: line }));
}

/** Waits (bounded, polling a process list that takes ~0.5 s per sample) until `predicate` holds. */
async function waitForProcesses(root, predicate, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  let rows = await processesUnder(root);
  while (!predicate(rows) && Date.now() < deadline) {
    await delay(250);
    rows = await processesUnder(root);
  }
  return rows;
}

function stage(outDir) {
  log(`[stage] node Worse/scripts/stage-sidecar-resources.mjs --out ${outDir}`);
  const output = execFileSync(process.execPath, [path.join(repoRoot, "Worse/scripts/stage-sidecar-resources.mjs"), "--out", outDir], {
    encoding: "utf8",
    windowsHide: true,
  });
  log(`[stage] ${output.trim().split("\n")[0]}`);
}

async function checkStaticLayout(root) {
  const missing = [];
  for (const file of REQUIRED) if (!(await isFile(path.join(root, file)))) missing.push(file);
  if (missing.length > 0) fail("K1_LAYOUT", `staged layout lacks: ${missing.join(", ")}`, "node Worse/scripts/stage-sidecar-resources.mjs");
  const manifest = JSON.parse(await readFile(path.join(root, "harness-host-manifest.json"), "utf8"));
  if (!manifest.sources.includes("Worse/sidecars/rust-bridge/dist-vendor")) fail("K1_MANIFEST", "manifest does not list dist-vendor");
  if (!manifest.tools.some((tool) => tool.endsWith(`/rg${exe}`))) fail("K1_MANIFEST", "manifest does not list ripgrep");
  if (isWindows && (manifest.conpty?.length ?? 0) !== 2) fail("K1_MANIFEST", "manifest does not list the ConPTY pair");
  log(`[K1] layout complete: ${REQUIRED.length} required files, ${manifest.packages.length} packages, tools=${manifest.tools.length}`);
}

/** The staged bridge's own config resolution: binary from the staged tree, no PATH sideload needed. */
async function checkBridgeResolution(root) {
  const bridge = await import(pathToFileURL(path.join(root, "Worse/sidecars/rust-bridge/dist/config.js")).href);
  const env = { ...process.env };
  for (const key of DEV_OVERRIDES) delete env[key];
  const config = bridge.readBridgeConfig(env, root);
  const expectedHost = path.join(root, `Worse/target/release/harness-host${exe}`);
  if (path.resolve(config.binaryPath).toLowerCase() !== expectedHost.toLowerCase()) {
    fail("K2_BRIDGE_PATH", `staged bridge resolved harness-host to ${config.binaryPath}, expected ${expectedHost}`);
  }
  if (isWindows && config.conptyDir !== null) {
    fail("K2_CONPTY_PATH", `staged bridge prepends ${config.conptyDir}; the staged conpty.dll next to harness-host should win`);
  }
  log(`[K2] staged bridge -> ${path.relative(root, config.binaryPath)}, conpty sideload ${isWindows ? "via application directory" : "n/a"}`);
}

async function checkRipgrep(root) {
  const rg = path.join(root, `packages/desktop/bundled-tools/${toolKey}/ripgrep/rg${exe}`);
  const { stdout } = await execFileAsync(rg, ["--version"], { windowsHide: true });
  const version = stdout.split("\n")[0].trim();
  if (!/^ripgrep \d+/.test(version)) fail("K4_RG", `unexpected rg --version output: ${version}`);
  log(`[K4] staged ${version}`);
}

async function runStagedHost(root, dataDir, modules) {
  const env = { HARNESS_RUST_SERVICES: "all", HARNESS_RUST_BRIDGE_TRACE: "1", ZCODE_ENV: "production", ZCODE_DATA_BASE_DIR: dataDir };
  const providerConfig = path.join(root, "config/provider/zcode-builtin.json");
  // startSidecar merges process.env, so the dev-tree overrides are removed from it for the spawn.
  const saved = {};
  for (const key of DEV_OVERRIDES) {
    saved[key] = process.env[key];
    delete process.env[key];
  }
  const sidecar = startSidecar({
    launcher: path.join(root, "Worse/sidecars/host-node/dist/main.js"),
    cwd: root,
    env: {
      ...env,
      HARNESS_HOST_ENTRY: path.join(root, "packages/desktop/out/host/index.js"),
      HARNESS_ALLOWED_ORIGINS: ALLOWED_ORIGIN,
      ZCODE_BUILTIN_PROVIDER_CONFIG_FILE: providerConfig,
    },
  });
  for (const [key, value] of Object.entries(saved)) if (value !== undefined) process.env[key] = value;
  const trace = [];
  let remainder = "";
  sidecar.child.stderr.on("data", (chunk) => {
    const parts = (remainder + chunk.toString()).split(/\r?\n/);
    remainder = parts.pop() ?? "";
    for (const line of parts) {
      const match = TRACE.exec(line);
      if (match) trace.push({ method: match[1], outcome: match[2] });
    }
  });
  let connection;
  let port;
  try {
    const ready = await awaitReady(sidecar);
    const grant = await openAttachment(sidecar, buildInitLocalMessage({ repoRoot: root, providerConfig }), "K2_ATTACH_OPEN");
    port = await attachSocket(modules.WebSocket, `ws://127.0.0.1:${ready.wsPort}/v1/attach`, ALLOWED_ORIGIN, grant, "K2_ATTACH_WS");
    connection = await connectRpc(modules.rpc, port, "K2_INITIALIZE");
    const call = (channel, method, params) =>
      withTimeout(connection.client.getChannel(channel).call(method, [params]), CALL_TIMEOUT_MS, "K2_CALL", `${channel}.${method}`);

    const entries = await call("file", "readdir", { path: root });
    if (!entries.some((entry) => entry.name === "harness-host-manifest.json")) fail("K2_READDIR", "readdir of the staged root lacks the manifest");
    const hits = await call("file", "searchWorkspaceFiles", { rootPath: path.join(root, "config"), query: "zcode-builtin", limit: 10 });
    if (!Array.isArray(hits) || hits.length === 0) fail("K2_SEARCH", "workspace file search in the staged tree found nothing");
    await call("git", "getWorkspaceRepositoryInfo", { workspacePath: root });
    const routed = (method) => trace.some((entry) => entry.method === method && entry.outcome === "rust");
    // The bridge writes its trace line to stderr after answering; give the pipe a moment.
    const expected = ["file.readdir", "file.searchWorkspaceFiles", "git.getWorkspaceRepositoryInfo"];
    await waitUntil(() => expected.every(routed), 5_000);
    for (const method of expected) {
      if (!routed(method)) fail("K2_ROUTED", `${method} was not served by the staged harness-host (trace: ${JSON.stringify(trace)})`);
    }
    log(`[K2] staged Host ready; file/search/git served by Rust (${trace.filter((e) => e.outcome === "rust").length} routed calls)`);

    const term = await openTerminal(connection.client, { cols: 100, rows: 30, cwd: root });
    if (!String(term.id).startsWith("rpty-")) {
      const bridgeLines = sidecar.stderrTail(400).split("\n").filter((line) => /rust-bridge|terminal|pty/i.test(line));
      fail("K3_PTY", `terminal.create returned ${term.id}; the Rust PTY route was not used\n${bridgeLines.slice(-15).join("\n")}`);
    }
    const marker = `P3C_${Date.now()}`;
    const half = Math.floor(marker.length / 2);
    await term.write(`${term.commands.print(marker.slice(0, half), marker.slice(half))}${term.commands.eol}`);
    if (!(await waitUntil(() => term.text().includes(marker)))) fail("K3_ECHO", "marker was not echoed by the staged PTY");
    const live = await processesUnder(root);
    const names = live.map((row) => row.name.toLowerCase());
    if (!names.includes(`harness-host${exe}`)) fail("K3_PROC", `no staged harness-host running (${names.join(", ")})`);
    if (isWindows && !names.includes("openconsole.exe")) {
      fail("K3_CONPTY", `the PTY does not run the staged OpenConsole.exe (staged processes: ${names.join(", ")})`);
    }
    log(`[K3] terminal ${term.id} (shell ${term.created.shell}) echoed marker; staged processes: ${[...new Set(names)].join(", ")}`);
    await term.dispose();
  } finally {
    connection?.dispose();
    port?.close();
    sidecar.child.stdin.end();
    const exit = await sidecar.waitForExit(SHUTDOWN_TIMEOUT_MS, "K5_EXIT").catch(() => null);
    sidecar.kill();
    if (exit === null) fail("K5_EXIT", "staged Host did not exit after stdin closed");
    if (exit.code !== 0) fail("K5_EXIT", `staged Host exited with code ${String(exit.code)}\n${sidecar.stderrTail(30)}`);
  }
}

async function main() {
  const keep = process.argv.includes("--keep");
  const layoutIndex = process.argv.indexOf("--layout");
  const existing = layoutIndex > 0 ? process.argv[layoutIndex + 1] : undefined;
  if (layoutIndex > 0 && !existing) fail("ARGS", "--layout needs a directory");
  verifyArtifacts();
  const modules = await loadModules();
  const outer = await mkdtemp(path.join(os.tmpdir(), "harness-p3c-"));
  const root = existing ? path.resolve(existing) : path.join(outer, "host");
  if (within(root, repoRoot)) fail("K1_OUTSIDE", `layout ${root} is inside the repository`);
  try {
    if (existing) log(`[layout] checking existing layout ${root}`);
    else stage(root);
    await checkStaticLayout(root);
    await checkBridgeResolution(root);
    await checkRipgrep(root);
    await runStagedHost(root, path.join(outer, "data"), modules);
    const left = await waitForProcesses(root, (rows) => rows.length === 0, 10_000);
    if (left.length > 0) fail("K5_ORPHANS", `processes from the staged tree survived: ${JSON.stringify(left)}`);
    log("[K5] no staged process left after shutdown");
    log("PHASE3C_TOOLS_DIFF_OK");
  } finally {
    // --layout: the checked layout is not ours and stays; `outer` then only holds the Host data dir.
    if (keep) log(`[keep] layout left at ${root}`);
    else {
      for (let attempt = 0; attempt < 6; attempt += 1) {
        try {
          await rm(outer, { recursive: true, force: true });
          break;
        } catch {
          await delay(250 * (attempt + 1));
        }
      }
    }
  }
}

main().catch((error) => {
  if (error instanceof GateError) {
    process.stderr.write(`PHASE3C_TOOLS_DIFF_FAIL ${error.step}: ${error.message}\n${error.command ? `run: ${error.command}\n` : ""}`);
  } else {
    process.stderr.write(`PHASE3C_TOOLS_DIFF_FAIL ${error instanceof Error ? error.stack : String(error)}\n`);
  }
  process.exit(1);
});

