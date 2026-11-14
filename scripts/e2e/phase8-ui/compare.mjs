#!/usr/bin/env node
// ZCode vs Harness side-by-side screenshots (tool, not a gate). Spec phase-8-ui.md §8.
//
//   node Worse/scripts/e2e/phase8-ui/zcode-reference.mjs      # once: ZCode shots -> .work/phase8/ref/zcode-*.png
//   node Worse/scripts/e2e/phase8-ui/compare.mjs              # Harness shots + .work/phase8/compare-<scene>.png
//   PHASE8_COMPARE_EXE=<path>    automation build of harness-slint (default target/release/harness-slint[.exe])
//   PHASE8_COMPARE_COMPOSE_ONLY=1  only rebuild compare-*.png from existing ref/*.png
//
// Safety: isolated home first; Harness runs with its built-in fake core (no agent processes at all). Only the
// process this script spawned is stopped.
import { installIsolatedHome } from "../lib/isolatedHome.mjs";

const { home } = installIsolatedHome("phase8-compare");

const { spawn, spawnSync } = await import("node:child_process");
const { existsSync, mkdirSync, readFileSync, writeFileSync } = await import("node:fs");
const net = await import("node:net");
const path = (await import("node:path")).default;
const process = (await import("node:process")).default;
const { fileURLToPath } = await import("node:url");
const { SlintMcp, sleep } = await import("./mcp.mjs");
const { decodePng, sideBySide } = await import("./png.mjs");
const { SCENES, captureScenes } = await import("./scenes.mjs");
const { killTrees, processTree, stillAlive } = await import("../phase7-perf/procs.mjs");

const worse = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
const outDir = path.join(worse, ".work/phase8");
const refDir = path.join(outDir, "ref");
const exe = process.env.PHASE8_COMPARE_EXE ?? path.join(worse, "target/release", process.platform === "win32" ? "harness-slint.exe" : "harness-slint");

function freePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      server.close(() => resolve(port));
    });
  });
}

async function capture() {
  const workspace = path.join(home, "workspace");
  mkdirSync(path.join(workspace, "src"), { recursive: true });
  writeFileSync(path.join(workspace, "README.md"), "# demo workspace\n");
  writeFileSync(path.join(workspace, "src", "main.rs"), "fn main() {}\n");
  const port = await freePort();
  const env = { ...process.env, HARNESS_HOME: path.join(home, ".harness"), HARNESS_WORKSPACE: workspace, HARNESS_UI_CORE: "fake", SLINT_MCP_PORT: String(port) };
  delete env.SLINT_BACKEND;
  const child = spawn(exe, [], { cwd: workspace, env, stdio: ["ignore", "ignore", "pipe"] });
  const exited = new Promise((resolve) => child.once("exit", resolve));
  try {
    const mcp = new SlintMcp(port);
    await mcp.connect();
    const sizes = {};
    const checks = await captureScenes(mcp, async (name) => {
      const png = await mcp.screenshot();
      const { width, height } = decodePng(png);
      sizes[name] = `${width}x${height}`;
      writeFileSync(path.join(refDir, `harness-${name}.png`), png);
    });
    process.stdout.write(`[compare] harness shots ${JSON.stringify(sizes)} checks ${JSON.stringify(checks)}\n`);
  } finally {
    const tree = await processTree([child.pid]).catch(() => []);
    spawnSync("taskkill", ["/PID", String(child.pid)], { stdio: "ignore", windowsHide: true });
    const graceful = await Promise.race([exited.then(() => true), sleep(10_000).then(() => false)]);
    if (!graceful) await killTrees([child.pid]);
    const alive = await stillAlive(tree);
    if (alive.length > 0) await killTrees(alive.map((p) => p.pid));
  }
}

mkdirSync(refDir, { recursive: true });
if (process.env.PHASE8_COMPARE_COMPOSE_ONLY !== "1") await capture();
for (const scene of SCENES) {
  const zcode = path.join(refDir, `zcode-${scene}.png`);
  const harness = path.join(refDir, `harness-${scene}.png`);
  if (!existsSync(zcode) || !existsSync(harness)) {
    process.stdout.write(`[compare] skip ${scene} (missing ${existsSync(zcode) ? "harness" : "zcode"} shot)\n`);
    continue;
  }
  writeFileSync(path.join(outDir, `compare-${scene}.png`), sideBySide(readFileSync(zcode), readFileSync(harness)));
  process.stdout.write(`[compare] wrote compare-${scene}.png (left ZCode, right Harness)\n`);
}
process.stdout.write("PHASE8_COMPARE_OK\n");
