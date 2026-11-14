#!/usr/bin/env node
// ZCode vs Harness side by side for every settings page (tool, not a gate). Spec phase-9-parity.md §6.7.
//
//   node Worse/scripts/e2e/phase9-settings/zcode-reference.mjs   # once: ZCode shots -> .work/phase9/ref/
//   node Worse/scripts/e2e/phase9-settings/compare.mjs           # Harness shots + .work/phase9/compare-settings-<page>.png
//   PHASE9_COMPARE_EXE=<path>       automation build of harness-slint (default .work/tmp/phase9-ui-bin)
//   PHASE9_COMPARE_COMPOSE_ONLY=1   only rebuild compare-settings-*.png from existing ref/*.png
//
// Both sides use the same theme: zcode-reference.mjs forces ZCode light, so Harness starts with
// appearance.theme = "light" seeded into its isolated settings.json, and every Harness shot is checked to be light.
//
// Safety: isolated home first; Harness runs with its built-in fake core (no agent processes). Only the process this
// script spawned is stopped.
import { installIsolatedHome } from "../lib/isolatedHome.mjs";

const { home } = installIsolatedHome("phase9-compare");

const { spawn, spawnSync } = await import("node:child_process");
const { existsSync, mkdirSync, readFileSync, writeFileSync } = await import("node:fs");
const net = await import("node:net");
const path = (await import("node:path")).default;
const process = (await import("node:process")).default;
const { fileURLToPath } = await import("node:url");
const { SlintMcp, byLabel, label, sleep } = await import("../phase8-ui/mcp.mjs");
const { meanLuminance, sideBySide } = await import("../phase8-ui/png.mjs");
const { killTrees, processTree, stillAlive } = await import("../phase7-perf/procs.mjs");
const { PAGES } = await import("./pages.mjs");

const worse = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
const outDir = path.join(worse, ".work/phase9");
const refDir = path.join(outDir, "ref");
const exe =
  process.env.PHASE9_COMPARE_EXE ?? path.join(worse, ".work/tmp/phase9-ui-bin", process.platform === "win32" ? "harness-slint-automation.exe" : "harness-slint-automation");

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
  mkdirSync(workspace, { recursive: true });
  writeFileSync(path.join(workspace, "README.md"), "# demo workspace\n");
  // 9.2 复核 b：ZCode 参考图是浅色，Harness 原先按默认（深色系统主题）截图，左右主题不一致。启动前写入同一主题。
  const harnessHome = path.join(home, ".harness");
  mkdirSync(harnessHome, { recursive: true });
  writeFileSync(path.join(harnessHome, "settings.json"), JSON.stringify({ appearance: { theme: "light" } }, null, 2));
  const port = await freePort();
  const env = { ...process.env, HARNESS_HOME: harnessHome, HARNESS_WORKSPACE: workspace, HARNESS_UI_CORE: "fake", SLINT_MCP_PORT: String(port) };
  delete env.SLINT_BACKEND;
  delete env.HARNESS_UI_WINDOW_SIZE;
  const child = spawn(exe, [], { cwd: workspace, env, stdio: ["ignore", "ignore", "pipe"] });
  const exited = new Promise((resolve) => child.once("exit", resolve));
  try {
    const mcp = new SlintMcp(port);
    await mcp.connect();
    const press = async (text) => mcp.activate(await mcp.waitFor(`button ${text}`, (e) => label(e) === text && String(e.accessibleRole).toLowerCase() === "button"));
    await press("Settings");
    await mcp.waitFor("settings", byLabel("Close settings"));
    for (const [id, text] of PAGES) {
      await press(text);
      await sleep(700);
      const png = await mcp.screenshot();
      const lum = meanLuminance(png);
      if (lum < 150) throw new Error(`harness ${id} shot is not light (mean luminance ${lum.toFixed(0)})`);
      writeFileSync(path.join(refDir, `harness-settings-${id}.png`), png);
    }
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
if (process.env.PHASE9_COMPARE_COMPOSE_ONLY !== "1") await capture();
let written = 0;
for (const [id] of PAGES) {
  const zcode = path.join(refDir, `zcode-settings-${id}.png`);
  const harness = path.join(refDir, `harness-settings-${id}.png`);
  if (!existsSync(zcode) || !existsSync(harness)) {
    process.stdout.write(`[compare] skip ${id} (missing ${existsSync(zcode) ? "harness" : "zcode"} shot)\n`);
    continue;
  }
  writeFileSync(path.join(outDir, `compare-settings-${id}.png`), sideBySide(readFileSync(zcode), readFileSync(harness)));
  written++;
  process.stdout.write(`[compare] wrote compare-settings-${id}.png (left ZCode, right Harness)\n`);
}
process.stdout.write(`PHASE9_COMPARE_OK ${written}/${PAGES.length}\n`);
