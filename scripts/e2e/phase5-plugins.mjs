#!/usr/bin/env node
// Phase 5 plugins gate (spec docs/specs/harness/phase-5-plugins.md, section 7).
//
// P1  Sandbox parity, in process: the Harness owner (host-node dist/pluginSandbox.js) and ZCode Main's
//     registry + protocol handler (bundled unchanged in dist-vendor/pluginSandboxRuntime.js) get the same
//     registration; every response is compared. Allowed differences are only the documented origin
//     tokens (CSP frame-src / frame-ancestors) and the Harness shell document itself.
// P2  Sandbox protocol contract: register reply shape, owner check, quota, idempotency, 404/405/h32
//     isolation, dispose with stale/current initId, Gen UI vendor rewrite, response-size cap.
// P3  Guest privilege checks on every served document: no Tauri IPC / Host token / ws port, shell
//     connect-src 'none', plugin CSP never lists ipc: / localhost, init accepted only from the embedder.
// P4  Real host-node sidecar + ZCode Host: Gen UI prepareSandbox over RPC is answered by the sidecar owner
//     (no register request leaks to the shell), served over the sandbox channel, foreign owner rejected.
// P5  Plugin lifecycle over the real `plugin-management` RPC (ZCode Host → ZCode CLI), local fixture
//     marketplace only: add marketplace, install, list, disable, enable, uninstall, remove marketplace;
//     on-disk facts under the isolated home are compared with each RPC result.
// P6  Real GUI (Windows, WebView2 over CDP): the debug harness-app + vite dev server. A node client attaches
//     to the app's own Host, Gen UI prepareSandbox registers a record, the renderer adopts a
//     `webview[data-zcode-retained-sandbox]` element, the shell + plugin frames load over hsandbox://, and
//     inside both frames: no Tauri IPC globals, no parent DOM access, ipc.localhost fetch blocked,
//     camera/microphone/geolocation blocked by Permissions Policy; moveBefore keeps the frame; dispose
//     makes the URL 404. Skipped only with HARNESS_PHASE5_GUI=0 (reported in the summary).
//
// No network, no real marketplace, no account. Prints PHASE5_PLUGINS_OK + JSON summary, or
// PHASE5_PLUGINS_FAIL <step>.
import { installIsolatedHome } from "./lib/isolatedHome.mjs";

// 修复说明：Host/CLI 按 HOME/USERPROFILE/APPDATA 解析 ~/.zcode；任何子进程启动前切到一次性 home，
// 插件安装、市场与 config.json 只落在隔离目录，退出时校验真实 ~/.zcode 未被改动。
installIsolatedHome("phase5-plugins");

import { existsSync } from "node:fs";
import { mkdir, rm } from "node:fs/promises";
import path from "node:path";
import process from "node:process";
import { pathToFileURL } from "node:url";
import { loadModules, verifyArtifacts } from "./phase2/environment.mjs";
import { GateError, fail } from "./phase2/gate.mjs";
import { BUILD_COMMAND, assetsDir, ownerModule, runDir, runtimeBundle, shellBundle } from "./phase5-plugins/common.mjs";
import { runGui } from "./phase5-plugins/gui.mjs";
import { runSandboxContract } from "./phase5-plugins/sandboxContract.mjs";
import { runSidecar } from "./phase5-plugins/sidecar.mjs";

const startTime = Date.now();

async function main() {
  for (const [file, label] of [
    [ownerModule, "host-node dist/pluginSandbox.js"],
    [runtimeBundle, "host-node dist-vendor/pluginSandboxRuntime.js"],
    [shellBundle, "host-node dist-vendor/pluginSandboxShell.js"],
  ]) {
    if (!existsSync(file)) fail("BUILD_PLUGIN_SANDBOX", `${label} is missing`, BUILD_COMMAND);
  }
  if (!existsSync(path.join(assetsDir, "plugin-sandbox-alias.js"))) {
    fail("BUILD_ZCODE_ASSETS", "packages/desktop/out/plugin-sandbox is missing", "pnpm --filter @zcode/desktop exec tsup --config tsup.config.ts");
  }
  verifyArtifacts();
  await rm(runDir, { recursive: true, force: true });
  await mkdir(path.join(runDir, "workspace"), { recursive: true });
  const vendor = await import(pathToFileURL(runtimeBundle).href);
  const ownerExports = await import(pathToFileURL(ownerModule).href);
  const summary = { sandbox: await runSandboxContract(vendor, ownerExports) };
  const modules = await loadModules();
  Object.assign(summary, await runSidecar(modules));
  Object.assign(summary, await runGui(modules));
  summary.elapsedMs = Date.now() - startTime;
  await rm(runDir, { recursive: true, force: true, maxRetries: 6, retryDelay: 250 }).catch(() => undefined);
  process.stdout.write(`PHASE5_PLUGINS_OK ${JSON.stringify(summary)}\n`);
}

main().catch((error) => {
  const step = error instanceof GateError ? error.step : "UNEXPECTED";
  process.stdout.write(`PHASE5_PLUGINS_FAIL ${step}\n${error instanceof Error ? (error.stack ?? error.message) : String(error)}\n`);
  if (error instanceof GateError && error.command) process.stdout.write(`fix: ${error.command}\n`);
  process.exitCode = 1;
});
