#!/usr/bin/env node
// ZCode reference shots of every settings page that Harness has (Phase 9.1, agent B; spec phase-9-parity.md §6.7).
// Launches ZCode exactly like phase8-ui/zcode-reference.mjs (isolated home, local mock provider, dead cloud), 1100x720,
// light theme. Not a gate: it only writes PNGs.
//
//   node Worse/scripts/e2e/phase9-settings/zcode-reference.mjs
//
// Output: Worse/.work/phase9/ref/zcode-settings-<page>.png (Harness page ids). Needs packages/desktop/out.
import { installIsolatedHome, isolatedHomeDir } from "../lib/isolatedHome.mjs";

installIsolatedHome("phase9-zcode-ref");

const { mkdir } = await import("node:fs/promises");
const path = (await import("node:path")).default;
const process = (await import("node:process")).default;
const { launchApp, startDeadCloud, stopDevServers, stopStarted, trackedAlive, worseRoot } = await import("../phase7-parity/apps.mjs");
const { startMockProvider } = await import("../phase7-parity/mockProvider.mjs");
const { createWorkspace, seedZcodeData } = await import("../phase7-parity/fixtures.mjs");
const { waitForWorkspace, tid } = await import("../phase7-parity/ui.mjs");
const { ZCODE_SECTION } = await import("./pages.mjs");

const outDir = path.join(worseRoot, ".work", "phase9", "ref");
const runDir = path.join(worseRoot, ".work", "tmp", `phase9-zcode-ref-${process.pid}`);
const log = (line) => process.stdout.write(`[zcode-ref] ${line}\n`);
const settle = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

let app;
let provider;
let cloud;
let failure = null;
const missing = [];
try {
  await mkdir(outDir, { recursive: true });
  await mkdir(runDir, { recursive: true });
  provider = await startMockProvider();
  cloud = await startDeadCloud();
  const home = path.join(isolatedHomeDir(), "zcode");
  await mkdir(home, { recursive: true });
  const ws = await createWorkspace(home);
  await seedZcodeData({ runDir, providerBaseUrl: provider.baseUrl, workspace: ws, home });
  app = await launchApp("zcode", { cloud, evidenceDir: runDir, label: "zcode-settings-ref", home });
  const { page } = app;
  await page.setViewportSize({ width: 1100, height: 720 });
  await waitForWorkspace(page);
  await page.evaluate(() => {
    localStorage.setItem("zcode-theme", "zai-light");
    const root = document.documentElement;
    root.classList.remove("dark", "theme-zai-dark");
    root.classList.add("theme-zai-light");
  });
  await page.locator(tid("task-settings-button")).first().click();
  await page.waitForSelector(tid("settings-page"), { timeout: 30_000 });
  for (const [id, section] of Object.entries(ZCODE_SECTION)) {
    const nav = page.locator(`[data-settings-section-nav="${section}"]`).first();
    if ((await nav.count()) === 0) {
      missing.push(`${id}->${section}`);
      continue;
    }
    await nav.click();
    await settle(900);
    const file = path.join(outDir, `zcode-settings-${id}.png`);
    await page.screenshot({ path: file });
    log(`wrote ${path.relative(worseRoot, file)}`);
  }
  if (missing.length) log(`sections not shown by ZCode here: ${missing.join(", ")}`);
} catch (error) {
  failure = error;
  if (app) await app.page.screenshot({ path: path.join(outDir, "zcode-settings-fail.png") }).catch(() => {});
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
  process.stdout.write(`ZCODE_SETTINGS_REF_FAIL ${String(failure.message ?? failure).split("\n")[0]}\n`);
  process.exitCode = 1;
} else {
  process.stdout.write("ZCODE_SETTINGS_REF_OK\n");
}
