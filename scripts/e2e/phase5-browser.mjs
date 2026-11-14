#!/usr/bin/env node
// Phase 5 browser gate (spec docs/specs/harness/phase-5-browser.md, section 8).
//
// B1  Harness backend: ZCode's Host bridge (createBrowserControlMainBridge, bundled unchanged) posts
//     browser-execute-request through a parentPort shim; the Harness browser owner intercepts it and
//     answers via ZCode's BrowserGuestManager driving a sidecar Chromium over CDP.
// B2  ZCode reference: the same Host bridge → the same BrowserGuestManager inside real Electron with a
//     real <webview> guest (scripts/e2e/phase5-browser/electronReference.entry.ts).
// B3  Same command list on a loopback fixture page (navigate, snapshot, click/type, evaluate,
//     screenshot, getState, list, close); normalised results must match.
// B4  Harness-only contracts: recording → capability_unsupported, popup → claimable user tab,
//     HARNESS_BROWSER=off → no backend + backend_unavailable, Chromium exits on dispose.
// B5  Real host-node sidecar: browser owner loads, backend identity is injected into init-local, no
//     Chromium starts until a command needs a page, clean shutdown.
//
// No internet: the fixture is a 127.0.0.1 HTTP server. Prints PHASE5_BROWSER_OK + JSON summary, or
// PHASE5_BROWSER_FAIL <step>.
import { installIsolatedHome } from "./lib/isolatedHome.mjs";

// 修复说明：Host/Chromium/Electron 都按 HOME/USERPROFILE/APPDATA 解析数据目录；任何子进程启动前切到一次性
// home，退出时校验真实 ~/.zcode 未被改动（HOME_LEAK）。
installIsolatedHome("phase5-browser");

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdir, rm } from "node:fs/promises";
import http from "node:http";
import net from "node:net";
import { createRequire } from "node:module";
import path from "node:path";
import process from "node:process";
import { fileURLToPath, pathToFileURL } from "node:url";
import { loadModules, paths as phase2Paths, repoRoot, verifyArtifacts } from "./phase2/environment.mjs";
import { GateError, delay, fail, isProcessAlive, withTimeout } from "./phase2/gate.mjs";
import { ALLOWED_ORIGIN, SHUTDOWN_TIMEOUT_MS, attachAndCallSettings, awaitReady, buildInitLocalMessage } from "./phase2/scenarios.mjs";
import { startSidecar } from "./phase2/sidecarProcess.mjs";

const moduleDir = path.dirname(fileURLToPath(import.meta.url));
const hostNodeDir = path.join(repoRoot, "Worse/sidecars/host-node");
const ownerModule = path.join(hostNodeDir, "dist/browserOwner.js");
const browserRuntime = path.join(hostNodeDir, "dist-vendor/browserRuntime.js");
const runDir = path.join(repoRoot, "Worse/.work/tmp/phase5-browser");
const BUILD_COMMAND = "pnpm --dir Worse/sidecars/host-node build";
const COMMAND_TIMEOUT_MS = 45_000;
const startTime = Date.now();

// ---------------------------------------------------------------------------------------------
// Loopback fixture

const FIXTURE_PAGE = `<!doctype html><html><head><title>Phase5 Fixture</title>
<style>body{font:16px sans-serif;margin:24px;background:#fff}#out{color:#333}</style></head><body>
<h1>Harness browser fixture</h1>
<label>Name <input id="name" aria-label="Name"></label>
<button id="go" onclick="document.getElementById('out').textContent='clicked:'+document.getElementById('name').value">Go</button>
<p id="out">idle</p>
<a id="second" href="/second">Second page</a>
<a id="popup" href="/popup" target="_blank">Open popup</a>
</body></html>`;
const SECOND_PAGE = "<!doctype html><title>Second</title><p>second page</p>";
const POPUP_PAGE = "<!doctype html><title>Popup</title><p>popup page</p>";

async function startFixture() {
  const server = http.createServer((request, response) => {
    response.setHeader("content-type", "text/html; charset=utf-8");
    response.setHeader("cache-control", "no-store");
    const body = request.url === "/second" ? SECOND_PAGE : request.url === "/popup" ? POPUP_PAGE : FIXTURE_PAGE;
    response.end(body);
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return { server, base: `http://127.0.0.1:${server.address().port}` };
}

// ---------------------------------------------------------------------------------------------
// Backends: each exposes { name, bridge, close() }; the bridge is ZCode's Host-side bridge.

let requestCounter = 0;

function makeBridge(vendor, getIdentity, post) {
  const bridge = vendor.createBrowserControlMainBridge({ getBackendIdentity: getIdentity, postToMain: post });
  return bridge;
}

async function createHarnessBackend(vendor, { createBrowserOwner }, env) {
  const owner = createBrowserOwner({ env, loadVendor: async () => vendor });
  await owner.ready;
  const leaked = [];
  let bridge;
  // parentPort shim 替身：与 host-node parentPort.ts 相同的 postMessage / emitHostMessage 形状。
  const shim = {
    postMessage: (message) => leaked.push(message),
    emitHostMessage: (message) => {
      if (message?.type === "browser-execute-result") void bridge.handleResult({ requestId: message.requestId, result: message.result });
    },
  };
  owner.install(shim);
  bridge = makeBridge(vendor, () => owner.backendIdentity() ?? undefined, (message) => shim.postMessage(message));
  return {
    name: "harness",
    owner,
    bridge,
    leaked,
    close: async () => {
      bridge.dispose();
      await owner.dispose();
    },
  };
}

async function buildElectronReference() {
  const requireFromRepo = createRequire(path.join(repoRoot, "package.json"));
  const { build } = await import(pathToFileURL(requireFromRepo.resolve("esbuild")).href);
  const outfile = path.join(runDir, "electronReference.mjs");
  await build({
    entryPoints: [path.join(moduleDir, "phase5-browser/electronReference.entry.ts")],
    outfile,
    bundle: true,
    platform: "node",
    format: "esm",
    target: "node22",
    external: ["electron"],
    logLevel: "warning",
    plugins: [
      {
        name: "phase5-main-logger",
        setup(builder) {
          builder.onResolve({ filter: /^\.\.\/logger\.js$/ }, (args) =>
            args.importer.includes("browserView") ? { path: path.join(hostNodeDir, "vendor/browserLoggerStub.ts") } : undefined,
          );
        },
      },
    ],
    banner: { js: 'import { createRequire as __p5Require } from "node:module";\nconst require = __p5Require(import.meta.url);' },
  });
  return outfile;
}

function resolveElectronBinary() {
  const requireFromDesktop = createRequire(path.join(repoRoot, "packages/desktop/package.json"));
  try {
    const binary = requireFromDesktop("electron");
    return typeof binary === "string" && existsSync(binary) ? binary : null;
  } catch {
    return null;
  }
}

async function createElectronBackend(vendor, electronBinary, entry) {
  // 修复说明：Electron 在 Windows 上是 GUI 子系统程序，主进程 process.stdin 立即 end，stdin 行协议会让
  // 参考端在 ready 后直接退出；改为 gate 监听 loopback TCP，由 Electron 主进程主动连接。
  const control = net.createServer();
  await new Promise((resolve) => control.listen(0, "127.0.0.1", resolve));
  const connected = new Promise((resolve) => control.once("connection", resolve));
  const env = {
    ...process.env,
    PHASE5_ELECTRON_USER_DATA: path.join(runDir, "electron-user-data"),
    PHASE5_CONTROL_PORT: String(control.address().port),
  };
  delete env.ELECTRON_RUN_AS_NODE;
  const child = spawn(electronBinary, [entry], { env, stdio: ["ignore", "ignore", "pipe"], windowsHide: true });
  let stderrTail = "";
  child.stderr.setEncoding("utf8").on("data", (chunk) => {
    stderrTail = (stderrTail + chunk).slice(-8_000);
  });
  const exited = new Promise((resolve) => child.once("exit", resolve));
  const earlyExit = exited.then((code) => fail("B2_ELECTRON", `electron exited early (code ${code}): ${stderrTail.slice(-400)}`));
  const socket = await withTimeout(Promise.race([connected, earlyExit]), 60_000, "B2_ELECTRON", "the Electron reference to connect");
  control.close();
  socket.setEncoding("utf8");
  const waiters = new Map();
  let readyResolve;
  const ready = new Promise((resolve) => {
    readyResolve = resolve;
  });
  let remainder = "";
  socket.on("data", (chunk) => {
    const parts = (remainder + chunk).split("\n");
    remainder = parts.pop() ?? "";
    for (const raw of parts) {
      if (!raw.trim()) continue;
      const payload = JSON.parse(raw);
      if (payload.ready) readyResolve(payload);
      else waiters.get(payload.id)?.(payload);
    }
  });
  let nextId = 1;
  const call = (kind, request) =>
    withTimeout(
      new Promise((resolve) => {
        const id = nextId++;
        waiters.set(id, (payload) => {
          waiters.delete(id);
          resolve(payload);
        });
        socket.write(`${JSON.stringify({ id, kind, request })}\n`);
      }),
      COMMAND_TIMEOUT_MS,
      "B2_ELECTRON_CALL",
      `Electron reference ${kind}`,
    );
  const info = await withTimeout(Promise.race([ready, earlyExit]), 60_000, "B2_ELECTRON", "the Electron reference to start");
  const identity = (await call("identity")).identity;
  let bridge;
  bridge = makeBridge(vendor, () => identity, (message) => {
    void call("execute", message).then((payload) => bridge.handleResult({ requestId: message.requestId, result: payload.result }));
  });
  return {
    name: "zcode-electron",
    electron: info.electron,
    stderrTail: () => stderrTail,
    bridge,
    pid: child.pid,
    close: async () => {
      bridge.dispose();
      earlyExit.catch(() => undefined);
      socket.end();
      await Promise.race([exited, delay(10_000)]);
      if (child.exitCode === null) child.kill();
    },
  };
}

// ---------------------------------------------------------------------------------------------
// Scenario

function pngSize(base64) {
  const bytes = Buffer.from(base64.slice(0, 64), "base64");
  if (bytes.length < 24) return null;
  return { width: bytes.readUInt32BE(16), height: bytes.readUInt32BE(20) };
}

function findRef(snapshot, role, name) {
  return snapshot?.dom?.find((node) => node.role === role && node.name === name)?.ref;
}

function normaliseSnapshot(snapshot) {
  return (snapshot?.dom ?? []).map((node) => [node.tag, node.role ?? "", node.name ?? "", node.text ?? "", node.ref ?? ""].join("|"));
}

async function runScenario(backend, base) {
  const timings = {};
  const exec = async (label, command) => {
    requestCounter += 1;
    const startedAt = performance.now();
    const result = await withTimeout(
      backend.bridge.execute({
        sessionId: "phase5-session",
        turnId: "phase5-turn",
        workspacePath: path.join(runDir, "workspace"),
        clientMode: "desktop-continuous",
        command,
      }),
      COMMAND_TIMEOUT_MS,
      `B3_${backend.name}_${label}`,
      `${backend.name} ${command.method}`,
    );
    timings[label] = Math.round(performance.now() - startedAt);
    if (!result.ok) fail(`B3_${backend.name}_${label}`, `${command.method} failed: ${JSON.stringify(result.error)}`);
    return result;
  };

  const navigate = await exec("navigate", { method: "navigate", url: `${base}/` });
  const snapshot = (await exec("snapshot", { method: "snapshot" })).snapshot;
  const nameRef = findRef(snapshot, "textbox", "Name");
  const goRef = findRef(snapshot, "button", "Go");
  if (!nameRef || !goRef) fail(`B3_${backend.name}_refs`, "snapshot did not expose the Name textbox and Go button");
  await exec("clickInput", { method: "click", ref: nameRef });
  await exec("type", { method: "type", ref: nameRef, text: "harness" });
  await exec("clickButton", { method: "click", ref: goRef });
  const out = await exec("evaluate", { method: "evaluate", expression: "document.getElementById('out').textContent" });
  const shot = await exec("screenshot", { method: "screenshot" });
  const state = await exec("getState", { method: "getState" });
  const secondRef = findRef(snapshot, "link", "Second page");
  await exec("clickLink", { method: "click", ref: secondRef });
  const afterLink = await exec("getStateAfterLink", { method: "getState" });
  const back = await exec("back", { method: "back" });
  const listed = await exec("list", { method: "list" });
  const tabId = listed.tabs?.[0]?.tabId;
  await exec("close", { method: "close", ...(tabId ? { tabId } : {}) });
  const afterClose = await exec("listAfterClose", { method: "list" });

  return {
    normalised: {
      navigate: { url: navigate.state?.url, title: navigate.state?.title },
      snapshot: normaliseSnapshot(snapshot),
      typedAndClicked: out.value,
      screenshot: { mimeType: shot.image?.mimeType, size: shot.image ? pngSize(shot.image.base64) : null },
      state: { url: state.state?.url, title: state.state?.title, viewport: [state.state?.viewportWidth, state.state?.viewportHeight] },
      afterLink: { url: afterLink.state?.url, title: afterLink.state?.title },
      back: { url: back.state?.url },
      tabsBeforeClose: listed.tabs?.length ?? 0,
      tabsAfterClose: afterClose.tabs?.length ?? 0,
      backendType: navigate.meta?.backendType,
    },
    timings,
  };
}

function diff(left, right, prefix = "") {
  const differences = [];
  const a = JSON.stringify(left);
  const b = JSON.stringify(right);
  if (a === b) return differences;
  if (left && right && typeof left === "object" && typeof right === "object" && !Array.isArray(left)) {
    for (const key of new Set([...Object.keys(left), ...Object.keys(right)])) differences.push(...diff(left[key], right[key], `${prefix}${key}.`));
    return differences;
  }
  differences.push(`${prefix.replace(/\.$/, "")}: harness=${a?.slice(0, 300)} zcode=${b?.slice(0, 300)}`);
  return differences;
}

// ---------------------------------------------------------------------------------------------
// Harness-only contracts (B4)

async function runHarnessContracts(vendor, ownerExports, env, base) {
  const backend = await createHarnessBackend(vendor, ownerExports, env);
  const results = {};
  try {
    const run = (command) =>
      withTimeout(
        backend.bridge.execute({ sessionId: "phase5-contracts", workspacePath: path.join(runDir, "workspace"), command }),
        COMMAND_TIMEOUT_MS,
        "B4_COMMAND",
        command.method,
      );
    const recording = await run({ method: "recordingStart" });
    if (recording.ok || recording.error?.code !== "capability_unsupported") {
      fail("B4_RECORDING", `recordingStart must be capability_unsupported, got ${JSON.stringify(recording)}`);
    }
    results.recording = recording.error.code;

    const navigate = await run({ method: "navigate", url: `${base}/` });
    if (!navigate.ok) fail("B4_POPUP_NAV", JSON.stringify(navigate.error));
    const snapshot = (await run({ method: "snapshot" })).snapshot;
    const popupRef = findRef(snapshot, "link", "Open popup");
    const click = await run({ method: "click", ref: popupRef });
    if (!click.ok) fail("B4_POPUP_CLICK", JSON.stringify(click.error));
    let userTabs = [];
    for (let attempt = 0; attempt < 40 && userTabs.length === 0; attempt += 1) {
      const listed = await run({ method: "listUserTabs" });
      userTabs = (listed.userTabs ?? []).filter((tab) => String(tab.url ?? "").endsWith("/popup"));
      if (userTabs.length === 0) await delay(100);
    }
    if (userTabs.length === 0) fail("B4_POPUP", "the popup did not surface as a user tab");
    results.popupUserTab = { url: userTabs[0].url.replace(base, "<base>"), title: userTabs[0].title };

    const pid = backend.owner.stats().chromiumPid;
    if (pid !== null && backend.leaked.some((message) => message?.type === "browser-execute-request")) {
      fail("B4_LEAK", "a browser-execute-request leaked past the owner to the shell");
    }
    await backend.close();
    const exitDeadline = Date.now() + 5_000;
    while (pid !== null && isProcessAlive(pid) && Date.now() < exitDeadline) await delay(100);
    if (pid !== null && isProcessAlive(pid)) fail("B4_CHROMIUM_EXIT", `Chromium pid ${pid} survived owner dispose`);
    results.chromiumExitedOnDispose = true;
  } finally {
    await backend.close().catch(() => undefined);
  }

  const disabled = await createHarnessBackend(vendor, ownerExports, { ...env, HARNESS_BROWSER: "off" });
  try {
    const descriptors = await disabled.bridge.list();
    const result = await disabled.bridge.execute({ sessionId: "phase5-off", workspacePath: runDir, command: { method: "navigate", url: `${base}/` } });
    if (descriptors.length !== 0) fail("B4_DISABLED_LIST", "a disabled owner must not advertise a browser backend");
    if (result.ok || result.error?.code !== "backend_unavailable") fail("B4_DISABLED_EXEC", JSON.stringify(result));
    results.disabled = { descriptors: descriptors.length, error: result.error.code };
  } finally {
    await disabled.close();
  }
  return results;
}

// ---------------------------------------------------------------------------------------------
// Real sidecar (B5)

async function runSidecarSmoke() {
  verifyArtifacts();
  const modules = await loadModules();
  const dataDir = path.join(runDir, "sidecar-data");
  await mkdir(dataDir, { recursive: true });
  const sidecar = startSidecar({
    launcher: phase2Paths.launcher,
    cwd: repoRoot,
    env: {
      HARNESS_HOST_ENTRY: phase2Paths.hostEntry,
      HARNESS_ALLOWED_ORIGINS: ALLOWED_ORIGIN,
      HARNESS_LOG_LEVEL: "info",
      ZCODE_ENV: "production",
      ZCODE_DATA_BASE_DIR: dataDir,
      ZCODE_BUILTIN_PROVIDER_CONFIG_FILE: phase2Paths.providerConfig,
    },
  });
  try {
    const startedAt = Date.now();
    const ready = await awaitReady(sidecar);
    const readyMs = Date.now() - startedAt;
    const ctx = { sidecar, ...modules, url: `ws://127.0.0.1:${ready.wsPort}/v1/attach` };
    const first = await attachAndCallSettings(ctx, buildInitLocalMessage({ repoRoot, providerConfig: phase2Paths.providerConfig }), "init-local", "B5");
    const stderr = sidecar.stderrTail(400);
    if (!stderr.includes("[browser] in-app browser owner ready")) fail("B5_OWNER", `browser owner did not load:\n${sidecar.stderrTail(30)}`);
    if (!stderr.includes("[browser] backend identity injected into init-local")) fail("B5_IDENTITY", "init-local was dispatched without browserBackendIdentity");
    if (stderr.includes("[browser] chromium ready")) fail("B5_LAZY", "Chromium started before any browser command");
    first.connection.dispose();
    first.port.close();
    const shutdownAt = Date.now();
    sidecar.child.stdin.end();
    const exit = await sidecar.waitForExit(SHUTDOWN_TIMEOUT_MS, "B5_EXIT");
    if (exit.code !== 0) fail("B5_EXIT_CODE", `sidecar exited with code ${String(exit.code)}`);
    return { readyMs, identityInjected: true, chromiumLaunchedAtIdle: false, exitCode: exit.code, exitMs: exit.at - shutdownAt };
  } finally {
    sidecar.kill();
  }
}

// ---------------------------------------------------------------------------------------------

async function main() {
  if (!existsSync(ownerModule) || !existsSync(browserRuntime)) {
    fail("BUILD_BROWSER", "host-node dist/browserOwner.js or dist-vendor/browserRuntime.js is missing", BUILD_COMMAND);
  }
  await rm(runDir, { recursive: true, force: true });
  await mkdir(path.join(runDir, "workspace"), { recursive: true });
  const vendor = await import(pathToFileURL(browserRuntime).href);
  const ownerExports = await import(pathToFileURL(ownerModule).href);
  const env = {
    ...process.env,
    HARNESS_BROWSER_PROFILE_DIR: path.join(runDir, "chromium-profile"),
    HARNESS_BROWSER_DOWNLOAD_DIR: path.join(runDir, "downloads"),
  };
  const { server, base } = await startFixture();
  const summary = { fixture: "127.0.0.1 loopback" };
  try {
    // B1 + B3 (Harness)
    const harness = await createHarnessBackend(vendor, ownerExports, env);
    let harnessRun;
    let harnessStats;
    try {
      const descriptors = await harness.bridge.list();
      if (descriptors.length !== 1 || descriptors[0].type !== "iab") fail("B1_LIST", `unexpected descriptors ${JSON.stringify(descriptors).slice(0, 300)}`);
      harnessRun = await runScenario(harness, base);
      harnessStats = harness.owner.stats();
      if (harness.leaked.some((message) => message?.type === "browser-execute-request")) fail("B1_LEAK", "browser-execute-request reached the shell");
    } finally {
      await harness.close();
    }
    summary.harness = { timings: harnessRun.timings, chromium: { executable: harnessStats.executable, headless: harnessStats.headless, launches: harnessStats.chromiumLaunches } };

    // B2 + B3 (ZCode reference)
    const electronBinary = resolveElectronBinary();
    if (electronBinary === null) {
      if (process.env.HARNESS_PHASE5_ALLOW_NO_ELECTRON !== "1") fail("B2_ELECTRON", "node_modules electron binary is missing", "pnpm install");
      summary.zcode = { skipped: "electron binary missing" };
    } else {
      const entry = await buildElectronReference();
      const reference = await createElectronBackend(vendor, electronBinary, entry);
      let referenceRun;
      try {
        const descriptors = await reference.bridge.list();
        if (descriptors.length !== 1 || descriptors[0].type !== "iab") fail("B2_LIST", "the Electron reference did not advertise the iab backend");
        referenceRun = await runScenario(reference, base);
        if (process.env.PHASE5_DEBUG === "1") process.stderr.write(`${reference.stderrTail()}\n${JSON.stringify(referenceRun.normalised.navigate)}\n`);
      } finally {
        await reference.close();
      }
      summary.zcode = { electron: reference.electron, timings: referenceRun.timings };
      const differences = diff(harnessRun.normalised, referenceRun.normalised);
      if (differences.length > 0) fail("B3_PARITY", `Harness and ZCode results differ:\n  ${differences.join("\n  ")}`);
    }
    summary.parity = {
      compared: Object.keys(harnessRun.normalised),
      typed: harnessRun.normalised.typedAndClicked,
      screenshot: harnessRun.normalised.screenshot,
      snapshotNodes: harnessRun.normalised.snapshot.length,
    };

    // B4
    summary.contracts = await runHarnessContracts(vendor, ownerExports, env, base);
  } finally {
    server.close();
  }

  // B5
  summary.sidecar = await runSidecarSmoke();
  summary.elapsedMs = Date.now() - startTime;
  await rm(runDir, { recursive: true, force: true, maxRetries: 6, retryDelay: 250 }).catch(() => undefined);
  process.stdout.write(`PHASE5_BROWSER_OK ${JSON.stringify(summary)}\n`);
}

main().catch((error) => {
  const step = error instanceof GateError ? error.step : "UNEXPECTED";
  process.stdout.write(`PHASE5_BROWSER_FAIL ${step}\n${error instanceof Error ? error.message : String(error)}\n`);
  if (error instanceof GateError && error.command) process.stdout.write(`fix: ${error.command}\n`);
  process.exitCode = 1;
});
