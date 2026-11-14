// HKCU guard for the ZCode side of the Phase 7 performance gate.
//
// ZCode main writes per-user registry entries on every start, unconditionally:
//   - app.setAsDefaultProtocolClient("zcode", execPath, ["--"])  -> HKCU\Software\Classes\zcode
//   - installWindowsOpenFolderContextMenu (reg.exe add)          -> HKCU\Software\Classes\{Directory,Drive}\shell\ZCode.OpenInZCode
// The registry is not covered by the isolated home, so a gate launch would repoint the user's own
// zcode:// handler and Explorer menu at the gate's cache layout. Before the first ZCode launch the
// keys are exported; after every ZCode run (and at gate exit) a key that now points into the gate
// layout is deleted and the export re-imported. Keys that point anywhere else (another ZCode wrote
// them in the meantime) are left alone.
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import process from "node:process";

const REG = path.join(process.env.SystemRoot ?? "C:\\Windows", "System32", "reg.exe");
export const GUARDED_KEYS = [
  "HKCU\\Software\\Classes\\zcode",
  "HKCU\\Software\\Classes\\Directory\\shell\\ZCode.OpenInZCode",
  "HKCU\\Software\\Classes\\Drive\\shell\\ZCode.OpenInZCode",
];

function reg(args) {
  return new Promise((resolve) => {
    const child = spawn(REG, args, { windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    child.stdout.setEncoding("utf8").on("data", (chunk) => (stdout += chunk));
    child.once("error", () => resolve({ code: -1, stdout }));
    child.once("close", (code) => resolve({ code: code ?? 1, stdout }));
  });
}

async function keyText(key) {
  const result = await reg(["query", key, "/s"]);
  return result.code === 0 ? result.stdout : null;
}

const state = { snapshot: null, marker: null, restored: 0, notes: [] };

/** Exports the guarded keys once per gate run. `marker` is the gate layout dir (case-insensitive). */
export async function snapshotRegistry({ dir, marker }) {
  if (state.snapshot) return;
  await mkdir(dir, { recursive: true });
  state.marker = marker.toLowerCase();
  state.snapshot = [];
  for (const [index, key] of GUARDED_KEYS.entries()) {
    const text = await keyText(key);
    const file = path.join(dir, `reg-guard-${index}.reg`);
    let exported = false;
    if (text !== null) exported = (await reg(["export", key, file, "/y"])).code === 0 && existsSync(file);
    // 快照本身已指向 gate 布局（上次运行异常中断），原值无法恢复，只记录并在结束时删除该键。
    const stale = text !== null && text.toLowerCase().includes(state.marker);
    if (stale) state.notes.push(`${key} already pointed into the gate layout before this run; it is deleted, not restored`);
    state.snapshot.push({ key, existed: text !== null, file: exported && !stale ? file : null, stale });
  }
}

/** Restores every guarded key that now points into the gate layout. */
export async function restoreRegistry() {
  if (!state.snapshot) return;
  for (const entry of state.snapshot) {
    const text = await keyText(entry.key);
    if (text === null || !text.toLowerCase().includes(state.marker)) continue;
    await reg(["delete", entry.key, "/f"]);
    if (entry.file) await reg(["import", entry.file]);
    state.restored += 1;
  }
}

export function registryGuardReport() {
  return {
    keys: GUARDED_KEYS,
    snapshotTaken: Boolean(state.snapshot),
    existedBefore: state.snapshot?.map((entry) => ({ key: entry.key, existed: entry.existed, stale: entry.stale })) ?? [],
    restores: state.restored,
    notes: state.notes,
  };
}

/** True when no guarded key points into the gate layout. */
export async function registryClean() {
  if (!state.snapshot) return true;
  for (const key of GUARDED_KEYS) {
    const text = await keyText(key);
    if (text !== null && text.toLowerCase().includes(state.marker)) return false;
  }
  return true;
}
