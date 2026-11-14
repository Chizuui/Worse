#!/usr/bin/env node
// Phase 2 machine gate (spec section 11, S1-S4): the real ZCode Host as a Node sidecar.
// Prints PHASE2_E2E_OK plus a JSON summary, or PHASE2_E2E_FAIL <step> on the first failure.
import { mkdir } from "node:fs/promises";
import process from "node:process";
import { paths, loadModules, removeRunDir, repoRoot, verifyArtifacts } from "./phase2/environment.mjs";
import { GateError, delay, fail } from "./phase2/gate.mjs";
import {
  ALLOWED_ORIGIN,
  SHUTDOWN_TIMEOUT_MS,
  assertNoAgentSurvivors,
  assertRejections,
  attachAndCallSettings,
  awaitReady,
  buildAttachServicePortMessage,
  buildInitLocalMessage,
} from "./phase2/scenarios.mjs";
import { startSidecar } from "./phase2/sidecarProcess.mjs";
import { installIsolatedHome } from "./lib/isolatedHome.mjs";

// 修复说明：Host/Agent 按 HOME/USERPROFILE 解析 ~/.zcode，曾把测试数据写进用户真实设置；
// 在任何子进程启动前切到一次性 home，并在退出时检查真实 ~/.zcode/v2 文件未被改动（HOME_LEAK）。
installIsolatedHome("phase2-smoke");

const startTime = Date.now();

async function runScenarios(sidecar, modules) {
  const ready = await awaitReady(sidecar);
  const ctx = { sidecar, ...modules, url: `ws://127.0.0.1:${ready.wsPort}/v1/attach` };

  // S1: first attachment carries init-local.
  const initLocal = buildInitLocalMessage({ repoRoot, providerConfig: paths.providerConfig });
  if ("hostId" in initLocal || "deliveryKind" in initLocal) {
    fail("S1_INIT_LOCAL", "init-local must not carry hostId or deliveryKind");
  }
  const first = await attachAndCallSettings(ctx, initLocal, "init-local", "S1");

  // S2: a reload attaches with attach-service-port on the same Host process.
  const second = await attachAndCallSettings(ctx, buildAttachServicePortMessage(), "attach-service-port", "S2");
  const readyLines = sidecar.lines.filter((line) => line.t === "ready").length;
  const hostPidUnchanged = !sidecar.exitInfo && readyLines === 1 && sidecar.child.pid === ready.hostPid;
  if (!hostPidUnchanged) fail("S2_HOST_PID", "the Host process changed or exited between attachments");

  // Closing the first socket must not stop the Host (invariant 3).
  first.connection.dispose();
  first.port.close();
  await delay(250);
  if (sidecar.exitInfo) fail("S2_WS_CLOSE", "closing an attachment socket stopped the Host");

  // S3: token and origin rejection.
  const rejections = await assertRejections(ctx);

  // S4: shutdown barrier.
  second.connection.dispose();
  second.port.close();
  await delay(250);
  const shutdownStartedAt = Date.now();
  sidecar.child.stdin.end();
  const exit = await sidecar.waitForExit(SHUTDOWN_TIMEOUT_MS, "S4_EXIT");
  if (exit.code !== 0) {
    // Still a failure; the hint only names a known intermittent Host-side exit race on Windows.
    const libuvExitRace = sidecar.stderrTail(200).includes("UV_HANDLE_CLOSING");
    const hint = libuvExitRace ? " (libuv UV_HANDLE_CLOSING assertion during Host process.exit; see p2f.md)" : "";
    fail("S4_EXIT_CODE", `sidecar exited with code ${String(exit.code)}, signal ${String(exit.signal)}${hint}`);
  }
  const agents = assertNoAgentSurvivors(sidecar);

  return {
    status: "ok",
    ready: { wsPort: ready.wsPort, hostPid: ready.hostPid, protocol: ready.protocol },
    firstAttachment: first.summary,
    secondAttachment: second.summary,
    hostPidUnchanged,
    rejections,
    shutdown: { exitCode: exit.code, exitSignal: exit.signal, exitMs: exit.at - shutdownStartedAt, ...agents },
  };
}

async function main() {
  verifyArtifacts();
  const modules = await loadModules();
  await removeRunDir();
  await mkdir(paths.dataDir, { recursive: true });

  const sidecar = startSidecar({
    launcher: paths.launcher,
    cwd: repoRoot,
    env: {
      HARNESS_HOST_ENTRY: paths.hostEntry,
      HARNESS_ALLOWED_ORIGINS: ALLOWED_ORIGIN,
      ZCODE_ENV: "production",
      ZCODE_DATA_BASE_DIR: paths.dataDir,
      ZCODE_BUILTIN_PROVIDER_CONFIG_FILE: paths.providerConfig,
    },
  });

  try {
    const summary = await runScenarios(sidecar, modules);
    const tmpCleaned = await removeRunDir();
    summary.shutdown = { ...summary.shutdown, tmpDir: paths.runDir, tmpCleaned };
    summary.elapsedMs = Date.now() - startTime;
    process.stdout.write(`PHASE2_E2E_OK\n${JSON.stringify(summary, null, 2)}\n`);
  } catch (error) {
    const tail = sidecar.stderrTail();
    if (tail) process.stderr.write(`sidecar stderr (tail):\n${tail}\n`);
    throw error;
  } finally {
    sidecar.kill();
    if (!(await removeRunDir())) process.stderr.write(`note: could not remove ${paths.runDir}; inspect it manually\n`);
  }
}

main().then(
  () => process.exit(0),
  (error) => {
    const step = error instanceof GateError ? error.step : "UNEXPECTED";
    process.stderr.write(`PHASE2_E2E_FAIL ${step}\nreason: ${error instanceof Error ? error.message : String(error)}\n`);
    if (error instanceof GateError && error.command) process.stderr.write(`run: ${error.command}\n`);
    process.exit(1);
  },
);
