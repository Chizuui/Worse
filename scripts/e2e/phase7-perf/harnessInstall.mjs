// Harness side of the Phase 7 performance gate: the payload of the release NSIS installer is
// extracted with 7-Zip into Worse/.work/cache/phase7/harness-install-<sha8>/ (exactly the files
// the installer copies to $INSTDIR), and the app runs from there.
//
// The installer itself is NOT executed: Tauri's NSIS template (tauri-bundler 2.9.4, utils.nsh
// CheckIfAppIsRunning) force-kills every harness-app.exe of the current user by name in silent
// mode, which would also kill Harness instances other gates or the user started. Extraction also
// leaves no HKCU entries. A real silent install was measured by the Phase 6 gate (189 MiB).
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createReadStream, existsSync } from "node:fs";
import { readdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import path from "node:path";
import process from "node:process";
import { cacheRoot, worseRoot } from "./fixtures.mjs";

export const SEVEN_ZIP = process.env.PHASE7_PERF_7Z?.trim() || "7z";
// NSIS 安装器内部文件，不会出现在 $INSTDIR。
const NSIS_INTERNAL = new Set(["$pluginsdir", "[nsis].nsi"]);

async function sha256(file) {
  const hash = createHash("sha256");
  await new Promise((resolve, reject) => createReadStream(file).on("data", (chunk) => hash.update(chunk)).on("end", resolve).on("error", reject));
  return hash.digest("hex");
}

export async function dirSize(dir, skip = () => false) {
  let bytes = 0;
  let files = 0;
  for (const entry of await readdir(dir, { recursive: true, withFileTypes: true })) {
    if (!entry.isFile()) continue;
    const full = path.join(entry.parentPath, entry.name);
    if (skip(path.relative(dir, full))) continue;
    bytes += (await stat(full)).size;
    files += 1;
  }
  return { bytes, files };
}

export function defaultInstaller() {
  return process.env.PHASE7_PERF_INSTALLER?.trim() || path.join(worseRoot, "target/release/bundle/nsis/Harness_0.6.0_x64-setup.exe");
}

/** Returns { dir, exe, installer, installerBytes, installedBytes, installedFiles, sha256, reused }. */
export async function ensureHarnessInstall({ installer = defaultInstaller(), log = () => {} } = {}) {
  if (!existsSync(installer)) throw new Error(`Harness installer not found: ${installer}`);
  const info = await stat(installer);
  const digest = await sha256(installer);
  const dir = path.join(cacheRoot, `harness-install-${digest.slice(0, 8)}`);
  const metaFile = path.join(cacheRoot, `harness-install-${digest.slice(0, 8)}.json`);
  if (existsSync(metaFile) && existsSync(path.join(dir, "harness-app.exe"))) {
    const meta = JSON.parse(await readFile(metaFile, "utf8"));
    if (meta.sha256 === digest) return { ...meta, dir, exe: path.join(dir, "harness-app.exe"), reused: true };
  }
  await rm(dir, { recursive: true, force: true });
  const started = performance.now();
  const code = await new Promise((resolve) => {
    const child = spawn(SEVEN_ZIP, ["x", "-y", "-bd", "-bso0", "-bsp0", `-o${dir}`, installer], { windowsHide: true, stdio: "ignore" });
    child.once("error", () => resolve(-1));
    child.once("close", (exitCode) => resolve(exitCode));
  });
  // 7z.exe 可能是应用执行别名（WindowsApps 重解析点，existsSync 为 false），所以直接启动并按结果判断。
  if (code !== 0 || !existsSync(path.join(dir, "harness-app.exe"))) throw new Error(`7-Zip (${SEVEN_ZIP}) could not extract ${installer} (exit ${code}); set PHASE7_PERF_7Z`);
  for (const name of await readdir(dir)) {
    if (NSIS_INTERNAL.has(name.toLowerCase()) || name.toLowerCase() === "uninstall.exe") await rm(path.join(dir, name), { recursive: true, force: true });
  }
  const size = await dirSize(dir);
  const meta = {
    installer,
    installerBytes: info.size,
    installerMtime: info.mtime.toISOString(),
    sha256: digest,
    installedBytes: size.bytes,
    installedFiles: size.files,
    installedNote: "NSIS payload extracted with 7-Zip ($INSTDIR files, without uninstall.exe)",
    extractSeconds: Math.round((performance.now() - started) / 100) / 10,
  };
  await writeFile(metaFile, JSON.stringify(meta, null, 2));
  log(`extracted Harness installer payload: ${size.files} files, ${(size.bytes / 1048576).toFixed(1)} MiB`);
  return { ...meta, dir, exe: path.join(dir, "harness-app.exe"), reused: false };
}
