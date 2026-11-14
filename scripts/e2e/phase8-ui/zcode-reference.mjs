#!/usr/bin/env node
// ZCode visual reference for the Phase 8 UI (round 8.2, agent B): launches ZCode exactly like the Phase 7
// parity gate (isolated home, local mock provider, dead cloud) and captures the screens the Slint UI is compared
// with, at the Slint window size (1100x720), light and dark. Not a gate: it only writes PNGs.
//
//   node Worse/scripts/e2e/phase8-ui/zcode-reference.mjs
//
// Output: Worse/.work/phase8/ref/zcode-<scene>.png. Needs packages/desktop/out (ZCode main build).
import { installIsolatedHome, isolatedHomeDir } from "../lib/isolatedHome.mjs";

installIsolatedHome("phase8-zcode-ref");

const { mkdir } = await import("node:fs/promises");
const path = (await import("node:path")).default;
const process = (await import("node:process")).default;
const { launchApp, startDeadCloud, stopDevServers, stopStarted, trackedAlive, worseRoot } = await import("../phase7-parity/apps.mjs");
const { startMockProvider } = await import("../phase7-parity/mockProvider.mjs");
const { createWorkspace, seedZcodeData } = await import("../phase7-parity/fixtures.mjs");
const { PERMISSION, send, waitForWorkspace, waitTimeline, tid } = await import("../phase7-parity/ui.mjs");

const outDir = path.join(worseRoot, ".work", "phase8", "ref");
const runDir = path.join(worseRoot, ".work", "tmp", `phase8-zcode-ref-${process.pid}`);
const log = (line) => process.stdout.write(`[zcode-ref] ${line}\n`);
const settle = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** Same classes ZCode's applyTheme() sets (useTheme.ts); the store value is written too so it survives renders. */
async function setTheme(page, dark) {
  await page.evaluate((isDark) => {
    const value = isDark ? "zai-dark" : "zai-light";
    localStorage.setItem("zcode-theme", value);
    const root = document.documentElement;
    root.classList.toggle("dark", isDark);
    root.classList.toggle("theme-zai-light", !isDark);
    root.classList.toggle("theme-zai-dark", isDark);
  }, dark);
  await settle(400);
}

async function shot(page, name) {
  const file = path.join(outDir, `zcode-${name}.png`);
  await page.screenshot({ path: file });
  log(`wrote ${path.relative(worseRoot, file)}`);
}

let app;
let provider;
let cloud;
let failure = null;
try {
  await mkdir(outDir, { recursive: true });
  await mkdir(runDir, { recursive: true });
  provider = await startMockProvider();
  cloud = await startDeadCloud();
  const home = path.join(isolatedHomeDir(), "zcode");
  await mkdir(home, { recursive: true });
  const ws = await createWorkspace(home);
  await seedZcodeData({ runDir, providerBaseUrl: provider.baseUrl, workspace: ws, home });
  app = await launchApp("zcode", { cloud, evidenceDir: runDir, label: "zcode-ref", home });
  const { page } = app;
  await page.setViewportSize({ width: 1100, height: 720 });
  await waitForWorkspace(page);
  await setTheme(page, false);
  await shot(page, "light-empty");

  await send(page, "P7_MD_Za show me the plan");
  await waitTimeline(page, "Codex");
  await settle(800);
  await shot(page, "light-chat");
  await setTheme(page, true);
  await shot(page, "dark-chat");

  provider.plan("P7_WRITE_Zb", { id: "toolu_phase8_ref", name: "Write", input: { file_path: path.join(ws, "demo.txt"), content: "demo\n" } });
  await send(page, "P7_WRITE_Zb write demo.txt");
  await page.waitForSelector(PERMISSION, { timeout: 60_000 });
  await settle(600);
  await shot(page, "dark-permission");
  await setTheme(page, false);
  await shot(page, "light-permission");

  await page.locator(tid("task-settings-button")).first().click();
  await page.waitForSelector(tid("settings-page"), { timeout: 30_000 });
  await settle(600);
  await shot(page, "light-settings");
  await setTheme(page, true);
  await shot(page, "dark-settings");
  await setTheme(page, false);
} catch (error) {
  failure = error;
  if (app) await app.page.screenshot({ path: path.join(outDir, "zcode-fail.png") }).catch(() => {});
} finally {
  if (app) log(`close: ${JSON.stringify(await app.close())}`);
  const stopped = [...stopStarted(), ...stopDevServers()];
  if (stopped.length) log(`stopped: ${JSON.stringify(stopped)}`);
  await provider?.close();
  cloud?.server.close();
  const alive = trackedAlive();
  if (alive.length) log(`STILL ALIVE: ${alive.join(", ")}`);
}
if (failure) {
  process.stdout.write(`ZCODE_REF_FAIL ${String(failure.message ?? failure).split("\n")[0]}\n`);
  process.exitCode = 1;
} else {
  process.stdout.write("ZCODE_REF_OK\n");
}
