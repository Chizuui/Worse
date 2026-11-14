// %TEMP% updater directories for the Phase 6 updater gate (spec phase-6-updater.md section 4.1, U9).
//
// tauri-plugin-updater extracts the N+1 installer into %TEMP%\<productName>-<version>-updater-<6 random>
// and never deletes it; the shell sweeps such directories (only this product, version <= running, files
// only). The gate plants decoys next to the real one: some must be removed, the rest must survive.
import { existsSync, readdirSync } from "node:fs";
import { mkdir, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";

/** Names in %TEMP% that look like this product's updater directories. */
export function updaterTempDirs(product) {
  const prefix = `${product}-`;
  try {
    return readdirSync(os.tmpdir()).filter((name) => name.startsWith(prefix) && name.includes("-updater-"));
  } catch {
    return [];
  }
}

/** Creates the decoy directories; `expect` is what the shell's sweep must do with each. */
export async function plantDecoys(product) {
  const decoys = [
    { name: `${product}-0.6.0-updater-p6Old1`, expect: "removed", why: "older version, files only" },
    { name: `${product}-9.9.9-updater-p6New1`, expect: "kept", why: "newer than the running version" },
    { name: `${product}X-0.6.0-updater-p6Oth1`, expect: "kept", why: "another product" },
    { name: `${product}-0.6.0-updater-p6Long12`, expect: "kept", why: "suffix is not 6 characters" },
    { name: `${product}-0.6.0-updater-p6Sub1`, expect: "kept", why: "holds a subdirectory" },
  ];
  for (const decoy of decoys) {
    const dir = path.join(os.tmpdir(), decoy.name);
    await rm(dir, { recursive: true, force: true });
    await mkdir(dir, { recursive: true });
    await writeFile(path.join(dir, `${product}-0.6.0-installer.exe`), "phase6 decoy");
    if (decoy.name.endsWith("p6Sub1")) await mkdir(path.join(dir, "nested"));
  }
  return decoys;
}

/** Decoys whose current state differs from what the sweep should have left. */
export function decoyMismatches(decoys) {
  return decoys
    .map((decoy) => ({ ...decoy, actual: existsSync(path.join(os.tmpdir(), decoy.name)) ? "kept" : "removed" }))
    .filter((decoy) => decoy.actual !== decoy.expect);
}

export async function removeDecoys(decoys) {
  for (const decoy of decoys) {
    await rm(path.join(os.tmpdir(), decoy.name), { recursive: true, force: true }).catch(() => {});
  }
}
