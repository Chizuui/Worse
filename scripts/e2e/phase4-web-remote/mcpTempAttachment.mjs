// Phase 4 web remote gate, scenarios W12-W15 (spec phase-4-web-remote-control.md, section 12):
// the platform requests Electron Main answered in ZCode, now served by host-node's
// mcpTempAttachmentHandlers.ts (ZCode's own Main functions, bundled unchanged).
//
// - W12: a phone `createTempTextAttachment` gets ZCode's own `app-error unsupported-action` (ZCode's
//        shared relay schema omits the method) and writes nothing; the owner, reached by the desktop
//        renderer path, writes `<data root>/.zcode/tmp/paste-attachments/<date>/` with the exact text.
//        Like ZCode, the file persists: ZCode's own storage clean refuses `toolOutputs`. The gate
//        then deletes its own test files.
// - W13: the phone loads the MCP servers seeded in the isolated home's `~/.zcode/cli/config.json`.
// - W14: the phone saves (upsert, set-enabled); the file keeps unrelated fields; a reload shows it.
// - W15: the desktop renderer path (`platformRequest` on the shell channel) reaches the same owner.
//
// All files live in the gate's isolated home / data dir; nothing touches the user's real ~/.zcode.
import { randomUUID } from "node:crypto";
import { existsSync } from "node:fs";
import { mkdir, readdir, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { fail } from "../phase2/gate.mjs";

const RESPONSE_TIMEOUT_MS = 30_000;
const CALL_TIMEOUT_MS = 45_000;
const SEEDED = "phase4-seeded-mcp";
const ADDED = "phase4-added-mcp";
const DESKTOP_ADDED = "phase4-desktop-mcp";
const KEEP_FIELD = "harnessGateUnrelatedField";

/** Phone `platform-request` -> `platform-response` over the relay (the web page's platform proxy). */
async function phoneRequest(phone, method, args, step) {
  const requestId = `phase4-fixb-${method}-${randomUUID()}`;
  const sent = phone.sendData({ zcode_type: "platform-request", requestId, method, ...(args === undefined ? {} : { args }) });
  if (!sent) fail(step, "phone could not send platform-request");
  return phone.waitForPayload(
    (payload) => payload.zcode_type === "platform-response" && payload.requestId === requestId,
    step,
    `platform-response ${method}`,
    RESPONSE_TIMEOUT_MS,
  );
}

async function phoneOk(phone, method, args, step) {
  const response = await phoneRequest(phone, method, args, step);
  if (response.success !== true) fail(step, `${method} failed on the desktop: ${String(response.error)}`);
  return response.result;
}

/** Shell -> sidecar `harness-web-remote-call platformRequest`, as remote_control.rs sends it; returns the raw result line. */
function createShellPlatformRequest(sidecar) {
  let next = 0;
  return async (method, args, step) => {
    next += 1;
    const id = `fixb-${next}`;
    sidecar.sendLine({
      t: "host",
      message: { type: "harness-web-remote-call", id, method: "platformRequest", params: { method, args } },
    });
    const line = await sidecar.waitForLine(
      (candidate) => candidate.t === "host" && candidate.message?.type === "harness-web-remote-result" && candidate.message.id === id,
      CALL_TIMEOUT_MS,
      step,
      `platformRequest ${method}`,
    );
    return line.message;
  };
}

const isInside = (child, parent) => {
  const relative = path.relative(path.resolve(parent), path.resolve(child));
  return relative !== "" && !relative.startsWith("..") && !path.isAbsolute(relative);
};

async function listFiles(dir) {
  const entries = await readdir(dir, { recursive: true, withFileTypes: true });
  return entries.filter((entry) => entry.isFile()).map((entry) => path.join(entry.parentPath, entry.name));
}

const serverNames = (result) => (Array.isArray(result?.servers) ? result.servers.map((server) => server.name).sort() : null);

/**
 * Runs W12-W15. `dataDir` is the sidecar's ZCODE_DATA_BASE_DIR, `homeDir` the isolated HOME.
 * Needs a paired phone (relay connected) and a started manager.
 */
export async function runPhoneTempAttachmentAndMcp({ phone, sidecar, kit, dataDir, homeDir, workspaceDir }) {
  const shell = createShellPlatformRequest(sidecar);
  const pasteRoot = path.join(dataDir, ".zcode", "tmp", "paste-attachments");

  // W12 (phone): ZCode parity. ZCode's desktop validates relay payloads with
  // `webRemoteControlPlatformMethodSchema` (packages/shared/src/validation.ts), whose enum omits
  // `createTempTextAttachment`, so ZCode's own manager answers `app-error unsupported-action` before
  // `platformHandlers` runs. Harness runs that manager unchanged and must answer the same way.
  const phoneRequestId = `phase4-fixb-createTempTextAttachment-${randomUUID()}`;
  phone.sendData({
    zcode_type: "platform-request",
    requestId: phoneRequestId,
    method: "createTempTextAttachment",
    args: { text: "phone paste", filename: "phase4-phone.txt" },
  });
  const phoneTemp = await phone.waitForPayload(
    (payload) => payload.requestId === phoneRequestId && (payload.zcode_type === "app-error" || payload.zcode_type === "platform-response"),
    "W12_TEMP_PHONE",
    "app-error or platform-response createTempTextAttachment",
    RESPONSE_TIMEOUT_MS,
  );
  const phonePasteFiles = existsSync(pasteRoot) ? await listFiles(pasteRoot) : [];
  const phoneTempOutcome =
    phoneTemp.zcode_type === "app-error"
      ? `app-error ${String(phoneTemp.reason)} (ZCode schema omits the method)`
      : `platform-response success=${String(phoneTemp.success)}`;
  if (phoneTemp.zcode_type === "app-error" && phonePasteFiles.length !== 0) {
    fail("W12_TEMP_PHONE", "a rejected phone request still wrote a temp file");
  }

  // W12 (owner) via the desktop renderer path: long-paste text -> temp file with the exact content.
  const text = `phase4 fix B paste ${randomUUID()}\nline 2 with unicode: 中文 ✓\n${"x".repeat(6000)}\n`;
  const createdLine = await shell("createTempTextAttachment", { text, filename: "phase4-paste.txt" }, "W12_TEMP_CREATE");
  if (!createdLine.ok) fail("W12_TEMP_CREATE", `createTempTextAttachment failed: ${String(createdLine.error)}`);
  const created = createdLine.value;
  if (created?.mimeType !== "text/plain" || typeof created.localPath !== "string") {
    fail("W12_TEMP_CREATE", `unexpected result ${JSON.stringify(created)}`);
  }
  if (!isInside(created.localPath, pasteRoot) || !/^\d{4}-\d{2}-\d{2}$/.test(path.basename(path.dirname(created.localPath)))) {
    fail("W12_TEMP_PATH", `temp file is not under <data root>/.zcode/tmp/paste-attachments/<date>/: ${created.localPath}`);
  }
  if (!/^phase4-paste-[0-9a-f]{8}\.txt$/.test(created.filename) || path.basename(created.localPath) !== created.filename) {
    fail("W12_TEMP_PATH", `unexpected file name ${created.filename}`);
  }
  const onDisk = await readFile(created.localPath, "utf8");
  if (onDisk !== text || created.sizeBytes !== Buffer.byteLength(text, "utf8")) {
    fail("W12_TEMP_CONTENT", "the desktop temp file does not hold exactly the phone's text");
  }
  // 与 ZCode 相同：空文本被拒绝，不落盘。
  const empty = await shell("createTempTextAttachment", { text: "" }, "W12_TEMP_EMPTY");
  if (empty.ok !== false || !/empty/i.test(String(empty.error))) {
    fail("W12_TEMP_EMPTY", `empty text was not rejected: ${JSON.stringify(empty)}`);
  }

  // A second file with the default name (no filename), as the composer sends it.
  const desktopCreated = await shell("createTempTextAttachment", { text: "desktop long paste" }, "W15_DESKTOP_TEMP");
  if (!desktopCreated.ok || !isInside(desktopCreated.value?.localPath ?? "", pasteRoot)) {
    fail("W15_DESKTOP_TEMP", `desktop createTempTextAttachment failed: ${JSON.stringify(desktopCreated)}`);
  }
  if ((await readFile(desktopCreated.value.localPath, "utf8")) !== "desktop long paste") {
    fail("W15_DESKTOP_TEMP", "desktop temp file content mismatch");
  }

  // W12 lifecycle, ZCode parity: ZCode never deletes paste attachments. The Agent keeps referencing
  // `localPath` from message history, and the Resource Manager marks `toolOutputs` (which owns
  // <data root>/tmp) as not cleanable (storageCatalog.ts `toolOutputs: "none"`). Run that same ZCode
  // storage service against the gate roots: it must refuse and leave both files in place.
  const storage = kit.createStorageService({
    roots: kit.createStorageRootsResolver({ getHomeDir: () => homeDir, getDataBaseDir: () => dataDir }),
    scanRunner: { run: () => Promise.reject(new Error("scan is not used by the gate")) },
    cleaner: kit.createFsStorageCleaner(),
  });
  let storageCleanRefusal = null;
  try {
    await storage.clean({ rootId: "dataBaseDir", categoryId: "toolOutputs" });
  } catch (error) {
    storageCleanRefusal = String(error?.message ?? error);
  } finally {
    storage.dispose();
  }
  if (!storageCleanRefusal || !/not cleanable/.test(storageCleanRefusal)) {
    fail("W12_TEMP_LIFECYCLE", `ZCode storage clean did not refuse toolOutputs: ${String(storageCleanRefusal)}`);
  }
  if (!existsSync(created.localPath) || !existsSync(desktopCreated.value.localPath)) {
    fail("W12_TEMP_LIFECYCLE", "a paste attachment disappeared although ZCode never deletes them");
  }
  // The gate removes its own test files (they live in the gate run dir, never in the real home).
  await rm(pasteRoot, { recursive: true, force: true });
  if (existsSync(created.localPath) || existsSync(desktopCreated.value.localPath)) {
    fail("W12_TEMP_CLEANUP", "gate cleanup left the temp files behind");
  }

  // W13: seed the isolated home's user MCP config and load it from the phone.
  const configFile = path.join(homeDir, ".zcode", "cli", "config.json");
  await mkdir(path.dirname(configFile), { recursive: true });
  await writeFile(
    configFile,
    JSON.stringify({ [KEEP_FIELD]: { keep: true }, mcp: { servers: { [SEEDED]: { command: "node", args: ["-v"] } } } }, null, 2),
    { mode: 0o600 },
  );
  const loaded = await phoneOk(phone, "loadMcpFromUserDirectory", { workspacePath: workspaceDir }, "W13_MCP_LOAD");
  const seeded = loaded?.servers?.find((server) => server.name === SEEDED);
  if (!seeded || seeded.scope !== "user" || seeded.enabled !== true || path.resolve(seeded.file?.filePath ?? "") !== path.resolve(configFile)) {
    fail("W13_MCP_LOAD", `phone did not load the seeded server from the isolated home: ${JSON.stringify(serverNames(loaded))}`);
  }

  // W14: phone upsert + set-enabled; the file keeps unrelated fields; a reload shows both changes.
  const saved = await phoneOk(
    phone,
    "saveMcpToUserDirectory",
    { action: "upsert", source: "zcodeagentmcp", name: ADDED, config: { command: "node", args: ["--version"] } },
    "W14_MCP_SAVE",
  );
  if (saved?.success !== true) fail("W14_MCP_SAVE", `save returned ${JSON.stringify(saved)}`);
  const disabled = await phoneOk(
    phone,
    "saveMcpToUserDirectory",
    { action: "set-enabled", source: "zcodeagentmcp", name: SEEDED, enabled: false },
    "W14_MCP_DISABLE",
  );
  if (disabled?.success !== true) fail("W14_MCP_DISABLE", `set-enabled returned ${JSON.stringify(disabled)}`);
  const file = JSON.parse(await readFile(configFile, "utf8"));
  if (file[KEEP_FIELD]?.keep !== true) fail("W14_MCP_FILE", "the save dropped an unrelated config field");
  if (file.mcp?.servers?.[ADDED]?.command !== "node" || file.mcp?.servers?.[SEEDED]?.enabled !== false) {
    fail("W14_MCP_FILE", `config file does not hold the phone's changes: ${JSON.stringify(Object.keys(file.mcp?.servers ?? {}))}`);
  }
  const reloaded = await phoneOk(phone, "loadMcpFromUserDirectory", undefined, "W14_MCP_RELOAD");
  const reloadedSeeded = reloaded?.servers?.find((server) => server.name === SEEDED);
  if (!reloaded?.servers?.some((server) => server.name === ADDED) || reloadedSeeded?.enabled !== false) {
    fail("W14_MCP_RELOAD", `reload does not show the saved changes: ${JSON.stringify(serverNames(reloaded))}`);
  }

  // W15: the desktop renderer path sees and changes the same file; the phone sees the desktop's change.
  const desktopLoad = await shell("loadMcpFromUserDirectory", {}, "W15_DESKTOP_MCP");
  if (!desktopLoad.ok || JSON.stringify(serverNames(desktopLoad.value)) !== JSON.stringify(serverNames(reloaded))) {
    fail("W15_DESKTOP_MCP", `desktop load differs from the phone's: ${JSON.stringify(desktopLoad)}`);
  }
  const desktopSave = await shell(
    "saveMcpToUserDirectory",
    { action: "upsert", source: "zcodeagentmcp", name: DESKTOP_ADDED, config: { command: "node" } },
    "W15_DESKTOP_MCP",
  );
  if (!desktopSave.ok || desktopSave.value?.success !== true) fail("W15_DESKTOP_MCP", `desktop save failed: ${JSON.stringify(desktopSave)}`);
  const phoneAfterDesktop = await phoneOk(phone, "loadMcpFromUserDirectory", undefined, "W15_DESKTOP_MCP");
  if (!phoneAfterDesktop?.servers?.some((server) => server.name === DESKTOP_ADDED)) {
    fail("W15_DESKTOP_MCP", "the phone does not see the desktop's MCP save");
  }
  const desktopMigrate = await shell("migrateLegacyCommonMcp", {}, "W15_DESKTOP_MCP");
  if (!desktopMigrate.ok || typeof desktopMigrate.value?.totalCount !== "number") {
    fail("W15_DESKTOP_MCP", `desktop migrateLegacyCommonMcp failed: ${JSON.stringify(desktopMigrate)}`);
  }
  // Fail closed: an unknown platformRequest method is rejected before any IO.
  const rejected = await shell("readArbitraryFile", { path: configFile }, "W15_REJECT");
  if (rejected.ok !== false || !/unsupported platform request/.test(String(rejected.error))) {
    fail("W15_REJECT", `unknown platformRequest method was not rejected: ${JSON.stringify(rejected)}`);
  }

  return {
    tempAttachment: {
      phoneRequest: phoneTempOutcome,
      via: "desktop platformRequest (same owner as the phone platformHandlers)",
      dir: path.relative(dataDir, path.dirname(created.localPath)).split(path.sep).join("/"),
      filename: created.filename,
      sizeBytes: created.sizeBytes,
      contentMatches: true,
      emptyRejected: true,
      persistsLikeZCode: true,
      zcodeStorageClean: storageCleanRefusal,
      removedByGateCleanup: true,
    },
    mcp: {
      configFile: path.relative(homeDir, configFile).split(path.sep).join("/"),
      loadedByPhone: serverNames(loaded),
      reloadedByPhone: serverNames(reloaded),
      seededDisabledByPhone: reloadedSeeded.enabled === false,
      unrelatedFieldKept: true,
      phoneSawDesktopSave: true,
    },
    desktopPath: { tempAttachment: true, load: true, save: true, migrate: desktopMigrate.value.totalCount, unknownRejected: true },
  };
}
