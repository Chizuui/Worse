#!/usr/bin/env node
// ZCode visual reference for the Phase 9.2 chat screens (agent B, tool, not a gate). Spec phase-9-parity.md §7.
// Launches ZCode exactly like the Phase 7 parity gate (isolated home, local mock provider, dead cloud) and captures
// the chat screens in the LIGHT theme at the Slint window size (1100x720). compare.mjs puts Harness next to them.
//
//   node Worse/scripts/e2e/phase9-chat/zcode-reference.mjs
//
// Output: Worse/.work/phase9/ref/zcode-chat-<scene>.png. Needs packages/desktop/out (ZCode main build).
// A scene ZCode does not show here is logged and skipped (compare.mjs then skips it too).
import { installIsolatedHome, isolatedHomeDir } from "../lib/isolatedHome.mjs";

installIsolatedHome("phase9-chat-zcode-ref");

const { mkdir } = await import("node:fs/promises");
const path = (await import("node:path")).default;
const process = (await import("node:process")).default;
const { launchApp, startDeadCloud, stopDevServers, stopStarted, trackedAlive, worseRoot } = await import("../phase7-parity/apps.mjs");
const { startMockProvider } = await import("../phase7-parity/mockProvider.mjs");
const { createWorkspace, seedZcodeData } = await import("../phase7-parity/fixtures.mjs");
const { send, waitForWorkspace, waitTimeline, tid } = await import("../phase7-parity/ui.mjs");

const outDir = path.join(worseRoot, ".work", "phase9", "ref");
const runDir = path.join(worseRoot, ".work", "tmp", `phase9-chat-zcode-ref-${process.pid}`);
const log = (line) => process.stdout.write(`[zcode-chat-ref] ${line}\n`);
const settle = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const missing = [];

async function light(page) {
  await page.evaluate(() => {
    localStorage.setItem("zcode-theme", "zai-light");
    const root = document.documentElement;
    root.classList.remove("dark", "theme-zai-dark");
    root.classList.add("theme-zai-light");
  });
  await settle(300);
}

async function shot(page, name) {
  const file = path.join(outDir, `zcode-chat-${name}.png`);
  await page.screenshot({ path: file });
  log(`wrote ${path.relative(worseRoot, file)}`);
}

/** One scene; a failure is recorded and the next scene still runs. */
async function scene(page, name, fn) {
  try {
    await fn();
    await shot(page, name);
  } catch (error) {
    missing.push(`${name}: ${String(error?.message ?? error).split("\n")[0]}`);
  } finally {
    await page.keyboard.press("Escape").catch(() => {});
    await settle(300);
  }
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
  app = await launchApp("zcode", { cloud, evidenceDir: runDir, label: "zcode-chat-ref", home });
  const { page } = app;
  await page.setViewportSize({ width: 1100, height: 720 });
  await waitForWorkspace(page);
  await light(page);

  await send(page, "P7_MD_Za show me the plan");
  await waitTimeline(page, "Codex");
  await settle(800);
  await scene(page, "transcript", async () => {});
  await scene(page, "command-center", async () => {
    await page.keyboard.press("Control+K");
    await settle(700);
  });
  await scene(page, "find", async () => {
    await page.keyboard.press("Control+F");
    await settle(400);
    await page.keyboard.type("plan");
    await settle(600);
  });
  await scene(page, "session-menu", async () => {
    const region = page.locator(tid("sidebar-task-list-region")).first();
    await region.getByText("P7_MD_Za", { exact: false }).first().click({ button: "right" });
    await settle(600);
  });
  await scene(page, "mention", async () => {
    const input = page.locator("[contenteditable='true'], textarea").first();
    await input.click();
    await page.keyboard.type("@");
    await settle(800);
  });
  await page.keyboard.press("Control+A").catch(() => {});
  await page.keyboard.press("Backspace").catch(() => {});
  // Round 9.2B-2 scenes (best effort: a control ZCode does not show in this fixture is logged as missing).
  await scene(page, "command-center-sections", async () => {
    await page.keyboard.press("Control+K");
    await settle(700);
  });
  await scene(page, "table", async () => {
    const table = page.locator("table").first();
    await table.waitFor({ timeout: 3000 });
    await table.scrollIntoViewIfNeeded();
    await settle(400);
  });
  await scene(page, "tool-detail", async () => {
    const tool = page.locator(tid("tool-call-header")).first();
    await tool.waitFor({ timeout: 3000 });
    await tool.click();
    await settle(500);
  });
  await scene(page, "schedules", async () => {
    await page.getByText(/scheduled|automation|定时/i).first().click({ timeout: 3000 });
    await settle(800);
  });
  await scene(page, "whiteboard", async () => {
    await page.getByRole("button", { name: /whiteboard|draw|白板/i }).first().click({ timeout: 3000 });
    await settle(800);
  });
  await page.setViewportSize({ width: 420, height: 800 });
  await settle(800);
  await scene(page, "narrow", async () => {});
  if (missing.length) log(`scenes not captured: ${missing.join(" | ")}`);
} catch (error) {
  failure = error;
  if (app) await app.page.screenshot({ path: path.join(outDir, "zcode-chat-fail.png") }).catch(() => {});
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
  process.stdout.write(`ZCODE_CHAT_REF_FAIL ${String(failure.message ?? failure).split("\n")[0]}\n`);
  process.exitCode = 1;
} else {
  process.stdout.write(`ZCODE_CHAT_REF_OK missing=${missing.length}\n`);
}
