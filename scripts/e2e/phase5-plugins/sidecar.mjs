// P4 + P5 of the Phase 5 plugins gate: the real host-node sidecar + ZCode Host (Gen UI sandbox over RPC,
// plugin lifecycle over `plugin-management` against a local fixture marketplace).
import { existsSync } from "node:fs";
import { mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { isolatedHomeDir } from "../lib/isolatedHome.mjs";
import { paths as phase2Paths, repoRoot } from "../phase2/environment.mjs";
import { GateError, fail, withTimeout } from "../phase2/gate.mjs";
import { ALLOWED_ORIGIN, SHUTDOWN_TIMEOUT_MS, attachAndCallSettings, awaitReady, buildInitLocalMessage } from "../phase2/scenarios.mjs";
import { startSidecar } from "../phase2/sidecarProcess.mjs";
import { CALL_TIMEOUT_MS, EMBEDDER, RPC_TIMEOUT_MS, assertGuestDocument, decode, h32Of, hsandboxUrl, runDir } from "./common.mjs";

async function writeFixtureMarketplace(root) {
  const plugin = path.join(root, "plugins", "phase5-hello");
  await mkdir(path.join(root, ".claude-plugin"), { recursive: true });
  await mkdir(path.join(plugin, ".claude-plugin"), { recursive: true });
  await mkdir(path.join(plugin, "skills", "phase5-greeting"), { recursive: true });
  await writeFile(
    path.join(root, ".claude-plugin", "marketplace.json"),
    JSON.stringify({ name: "phase5-fixtures", owner: { name: "Harness gate" }, plugins: [{ name: "phase5-hello", version: "1.0.0", source: "./plugins/phase5-hello", description: "Phase 5 fixture plugin" }] }),
  );
  await writeFile(path.join(plugin, ".claude-plugin", "plugin.json"), JSON.stringify({ name: "phase5-hello", version: "1.0.0", description: "Phase 5 fixture plugin", skills: "skills" }));
  await writeFile(
    path.join(plugin, "skills", "phase5-greeting", "SKILL.md"),
    ["---", "name: phase5-greeting", "description: Says hello from the Harness phase 5 fixture.", "---", "", "# Greeting", ""].join("\n"),
  );
}

async function listFilesRelative(dir) {
  const out = [];
  const walk = async (current) => {
    for (const entry of await readdir(current, { withFileTypes: true })) {
      const full = path.join(current, entry.name);
      if (entry.isDirectory()) await walk(full);
      else out.push(path.relative(dir, full).replaceAll("\\", "/"));
    }
  };
  if (existsSync(dir)) await walk(dir);
  return out.sort();
}

async function readJsonIfExists(file) {
  try {
    return JSON.parse(await readFile(file, "utf8"));
  } catch {
    return null;
  }
}

function findPlugin(list, pluginId) {
  const plugins = list?.plugins ?? list?.installedPlugins ?? [];
  return plugins.find((plugin) => plugin.id === pluginId) ?? null;
}

export async function runSidecar(modules) {
  const summary = {};
  const dataDir = path.join(runDir, "sidecar-data");
  const workspace = path.join(runDir, "workspace");
  const marketplaceDir = path.join(runDir, "fixture-marketplace");
  await mkdir(dataDir, { recursive: true });
  await mkdir(workspace, { recursive: true });
  await writeFixtureMarketplace(marketplaceDir);
  const sidecar = startSidecar({
    launcher: phase2Paths.launcher,
    cwd: repoRoot,
    env: {
      HARNESS_HOST_ENTRY: phase2Paths.hostEntry,
      HARNESS_ALLOWED_ORIGINS: ALLOWED_ORIGIN,
      HARNESS_LOG_LEVEL: "info",
      HARNESS_BROWSER: "off",
      ZCODE_ENV: "production",
      ZCODE_DATA_BASE_DIR: dataDir,
      ZCODE_BUILTIN_PROVIDER_CONFIG_FILE: phase2Paths.providerConfig,
    },
  });
  let nextCall = 1;
  const sandboxCall = async (method, params) => {
    const id = `gate-${nextCall++}`;
    sidecar.sendLine({ t: "host", message: { type: "harness-web-remote-sandbox-call", id, method, params } });
    const line = await sidecar.waitForLine(
      (candidate) => candidate.t === "host" && candidate.message?.type === "harness-web-remote-sandbox-result" && candidate.message.id === id,
      CALL_TIMEOUT_MS,
      "P4_SANDBOX_CALL",
      `sandbox ${method}`,
    );
    return line.message;
  };
  try {
    const ready = await awaitReady(sidecar);
    const ctx = { sidecar, ...modules, url: `ws://127.0.0.1:${ready.wsPort}/v1/attach` };
    const attached = await attachAndCallSettings(ctx, buildInitLocalMessage({ repoRoot, providerConfig: phase2Paths.providerConfig }), "init-local", "P4");
    const client = attached.connection.client;
    const rpc = (channel, method, params, timeout = RPC_TIMEOUT_MS) =>
      withTimeout(client.getChannel(channel).call(method, [params]), timeout, `RPC_${channel}_${method}`, `${channel}.${method}`);

    // --- P4: Gen UI prepareSandbox through the real Host ---
    const stats0 = await sandboxCall("stats", {});
    if (!stats0.ok) fail("P4_STATS", stats0.error);
    const target = { workspacePath: workspace, sessionId: "phase5-session", path: path.join(workspace, "phase5-widget.html") };
    const handle = await rpc("gen-ui", "prepareSandbox", { ...target, html: '<div id="phase5">gen ui fixture</div>', ownerWebContentsId: 1, instanceKey: "phase5:1" }, 30_000);
    if (!handle?.shellUrl?.startsWith("http://hsandbox.shell-") && !handle?.shellUrl?.startsWith("hsandbox://shell-")) {
      fail("P4_GENUI_HANDLE", `prepareSandbox returned ${JSON.stringify(handle).slice(0, 300)}`);
    }
    const leakedRegister = sidecar.lines.some((line) => line.t === "host" && line.message?.type === "plugin-sandbox-register-request");
    if (leakedRegister) fail("P4_LEAK", "plugin-sandbox-register-request reached the Rust shell");
    const appIdentity = handle.instance.appIdentity;
    const shellFetch = await sandboxCall("fetch", { url: hsandboxUrl("shell", appIdentity, `/instance/${handle.sandboxId}/`), method: "GET", embedderOrigin: EMBEDDER });
    const pluginFetch = await sandboxCall("fetch", { url: hsandboxUrl("plugin", appIdentity, `/instance/${handle.sandboxId}/index.html`), method: "GET", embedderOrigin: EMBEDDER });
    if (!shellFetch.ok || shellFetch.value.status !== 200) fail("P4_SHELL_FETCH", JSON.stringify(shellFetch).slice(0, 300));
    if (!pluginFetch.ok || pluginFetch.value.status !== 200) fail("P4_PLUGIN_FETCH", JSON.stringify(pluginFetch).slice(0, 300));
    if (!decode(pluginFetch.value).toString("utf8").includes("gen ui fixture")) fail("P4_PLUGIN_FETCH", "Gen UI document lacks the fixture fragment");
    assertGuestDocument("P4_GUEST", pluginFetch.value, [String(ready.wsPort)]);
    assertGuestDocument("P4_GUEST_SHELL", shellFetch.value, [String(ready.wsPort)]);
    let foreignError = null;
    try {
      await rpc("gen-ui", "prepareSandbox", { ...target, html: "<div>x</div>", ownerWebContentsId: 7, instanceKey: "phase5:foreign" }, 30_000);
    } catch (error) {
      foreignError = error instanceof Error ? error.message : String(error);
    }
    if (!foreignError?.includes("Plugin UI sandbox is unavailable.")) fail("P4_FOREIGN_OWNER", `foreign owner got ${foreignError}`);
    const disposed = await sandboxCall("dispose", { sandboxId: handle.sandboxId, initId: handle.initId });
    const afterDispose = await sandboxCall("fetch", { url: hsandboxUrl("plugin", appIdentity, `/instance/${handle.sandboxId}/index.html`), method: "GET", embedderOrigin: EMBEDDER });
    if (!disposed.ok || afterDispose.value?.status !== 404) fail("P4_DISPOSE", "dispose over the sandbox channel did not remove the record");
    const unknown = await sandboxCall("bogus", {});
    if (unknown.ok !== false) fail("P4_UNKNOWN_METHOD", "an unknown sandbox method was accepted");
    summary.sidecarSandbox = {
      shellUrl: handle.shellUrl.replace(handle.sandboxId, "<id>").replace(h32Of(appIdentity), "<h32>"),
      foreignOwner: "rejected",
      registerRequestReachedShell: false,
    };

    // --- P5: plugin lifecycle over plugin-management ---
    const pluginId = "phase5-hello@phase5-fixtures";
    const scope = { workspacePath: workspace };
    const pluginsRoot = path.join(isolatedHomeDir(), ".zcode", "cli", "plugins");
    const configFile = path.join(isolatedHomeDir(), ".zcode", "cli", "config.json");
    const steps = {};
    const added = await rpc("plugin-management", "addPluginMarketplace", { ...scope, source: marketplaceDir });
    steps.addMarketplace = (added.marketplaces ?? []).map((entry) => entry.id ?? entry.name).filter(Boolean);
    if (!JSON.stringify(added).includes("phase5-fixtures")) fail("P5_ADD_MARKETPLACE", JSON.stringify(added).slice(0, 400));
    const installed = await rpc("plugin-management", "installPlugin", { ...scope, pluginName: "phase5-hello", marketplace: "phase5-fixtures", scope: "user" });
    if (!(installed.installedPlugins ?? []).some((plugin) => plugin.id === pluginId)) fail("P5_INSTALL", JSON.stringify(installed).slice(0, 400));
    const installedRecord = await readJsonIfExists(path.join(pluginsRoot, "installed_plugins.json"));
    if (!JSON.stringify(installedRecord ?? {}).includes("phase5-hello")) fail("P5_INSTALL_DISK", "installed_plugins.json does not record the plugin");
    const cacheFiles = (await listFilesRelative(pluginsRoot)).filter((file) => file.includes("phase5-hello"));
    if (!cacheFiles.some((file) => file.endsWith("SKILL.md"))) fail("P5_INSTALL_DISK", `plugin files not installed: ${cacheFiles.join(", ")}`);
    steps.install = { id: pluginId, files: cacheFiles.length };

    const listed = await rpc("plugin-management", "listPlugins", scope);
    const listedPlugin = findPlugin(listed, pluginId);
    if (!listedPlugin) fail("P5_LIST", `listPlugins lacks ${pluginId}: ${JSON.stringify(listed).slice(0, 400)}`);
    steps.listEnabled = listedPlugin.enabled;

    const disabled = await rpc("plugin-management", "setPluginEnabled", { ...scope, pluginId, enabled: false, scope: "user" });
    const configAfterDisable = await readJsonIfExists(configFile);
    const listedDisabled = findPlugin(await rpc("plugin-management", "listPlugins", scope), pluginId);
    if (listedDisabled?.enabled !== false || configAfterDisable?.plugins?.enabledPlugins?.[pluginId] !== false) {
      fail("P5_DISABLE", `list=${JSON.stringify(listedDisabled)} config=${JSON.stringify(configAfterDisable?.plugins)} result=${JSON.stringify(disabled).slice(0, 200)}`);
    }
    await rpc("plugin-management", "setPluginEnabled", { ...scope, pluginId, enabled: true, scope: "user" });
    const configAfterEnable = await readJsonIfExists(configFile);
    const listedEnabled = findPlugin(await rpc("plugin-management", "listPlugins", scope), pluginId);
    if (listedEnabled?.enabled !== true || configAfterEnable?.plugins?.enabledPlugins?.[pluginId] === false) {
      fail("P5_ENABLE", `list=${JSON.stringify(listedEnabled)} config=${JSON.stringify(configAfterEnable?.plugins)}`);
    }
    steps.disableEnable = { configKey: `plugins.enabledPlugins["${pluginId}"]`, disabled: false, enabledAgain: true };

    const uninstalled = await rpc("plugin-management", "uninstallPlugin", { ...scope, pluginId });
    if (uninstalled.removedPlugin?.id !== pluginId) fail("P5_UNINSTALL", JSON.stringify(uninstalled).slice(0, 400));
    if (findPlugin(await rpc("plugin-management", "listPlugins", scope), pluginId)) fail("P5_UNINSTALL", "the plugin is still listed");
    const afterUninstall = await readJsonIfExists(path.join(pluginsRoot, "installed_plugins.json"));
    if (JSON.stringify(afterUninstall ?? {}).includes(`"${pluginId}"`)) fail("P5_UNINSTALL_DISK", "installed_plugins.json still records the plugin");
    steps.uninstall = { removed: uninstalled.removedPlugin.id };

    const removed = await rpc("plugin-management", "removePluginMarketplace", { ...scope, marketplace: "phase5-fixtures" });
    if (JSON.stringify(removed.marketplaces ?? []).includes("phase5-fixtures")) fail("P5_REMOVE_MARKETPLACE", JSON.stringify(removed).slice(0, 300));
    steps.removeMarketplace = true;
    summary.lifecycle = steps;

    attached.connection.dispose();
    attached.port.close();
    const shutdownAt = Date.now();
    sidecar.child.stdin.end();
    const exit = await sidecar.waitForExit(SHUTDOWN_TIMEOUT_MS, "P4_EXIT");
    if (exit.code !== 0) fail("P4_EXIT_CODE", `sidecar exited with code ${String(exit.code)}\n${sidecar.stderrTail(20)}`);
    summary.sidecarExit = { code: exit.code, ms: exit.at - shutdownAt };
    return summary;
  } catch (error) {
    if (error instanceof GateError) error.message += `\n--- sidecar stderr ---\n${sidecar.stderrTail(25)}`;
    throw error;
  } finally {
    sidecar.kill();
  }
}
