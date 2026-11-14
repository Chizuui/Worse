// Builds the "packaged-like" ZCode layout the Phase 7 performance gate runs (no ZCode release is
// installed on this machine, and its NSIS installer cannot be built offline):
//
//   Worse/.work/cache/phase7/zcode-app/
//     ZCode.exe                 hard link of electron.exe -> app.isPackaged === true (no repo .env*)
//     <electron runtime files>  hard links; resources/default_app.asar omitted
//     resources/app/package.json, out/main, out/preload   hard-linked copies (import.meta.dirname
//                                                         must stay inside the layout)
//     resources/app/out/{host,scheduler,plugin-sandbox}   junctions -> packages/desktop/out/*
//     resources/app/out/renderer                          junction -> zcode-renderer (vite build)
//     resources/app/node_modules                          junction -> packages/desktop/node_modules
//     resources/glm, resources/tools/*                    junctions -> bundled-agents / bundled-tools
//     resources/config/..., icons                         copies (electron-builder extraResources)
//
// Nothing under packages/** is written. The size estimate (staged tree + 7-Zip) is also here.
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { copyFile, link, lstat, mkdir, readdir, readFile, rm, stat, symlink, writeFile } from "node:fs/promises";
import path from "node:path";
import { cacheRoot, repoRoot } from "./fixtures.mjs";

const desktopDir = path.join(repoRoot, "packages/desktop");
export const zcodeAppDir = path.join(cacheRoot, "zcode-app");
export const zcodeExe = path.join(zcodeAppDir, "ZCode.exe");
const rendererDir = path.join(cacheRoot, "zcode-renderer");
const LAYOUT_VERSION = 1;

function electronDist() {
  const extracted = path.join(cacheRoot, "electron-44.7.0");
  if (existsSync(path.join(extracted, "electron.exe"))) return extracted;
  const installed = path.join(repoRoot, "node_modules/electron/dist");
  if (existsSync(path.join(installed, "electron.exe"))) return installed;
  throw new Error("Electron 44.7.0 runtime not found (node_modules/electron/dist or Worse/.work/cache/phase7/electron-44.7.0)");
}

async function linkOrCopy(source, target) {
  await mkdir(path.dirname(target), { recursive: true });
  try {
    await link(source, target);
  } catch {
    await copyFile(source, target);
  }
}

async function linkTree(sourceDir, targetDir, filter = () => true) {
  for (const entry of await readdir(sourceDir, { recursive: true, withFileTypes: true })) {
    if (!entry.isFile()) continue;
    const source = path.join(entry.parentPath, entry.name);
    const relative = path.relative(sourceDir, source);
    if (!filter(relative)) continue;
    await linkOrCopy(source, path.join(targetDir, relative));
  }
}

async function junction(target, at) {
  await mkdir(path.dirname(at), { recursive: true });
  await symlink(target, at, "junction");
}

async function fingerprint() {
  const files = [path.join(desktopDir, "out/main/index.js"), path.join(desktopDir, "out/host/index.js"), path.join(desktopDir, "out/preload/index.cjs"), path.join(rendererDir, "index.html"), path.join(desktopDir, "package.json")];
  const parts = [];
  for (const file of files) {
    const info = await stat(file).catch(() => null);
    parts.push(info ? `${path.basename(file)}:${info.size}:${Math.round(info.mtimeMs)}` : `${path.basename(file)}:missing`);
  }
  return { version: LAYOUT_VERSION, electron: electronDist(), parts };
}

/** Creates or refreshes the layout. Returns { dir, exe, rebuilt }. */
export async function ensureZcodeLayout() {
  for (const required of ["out/main/index.js", "out/host/index.js", "out/preload/index.cjs", "bundled-agents/win32-x64/glm"]) {
    if (!existsSync(path.join(desktopDir, required))) throw new Error(`ZCode build artifact missing: packages/desktop/${required}`);
  }
  if (!existsSync(path.join(rendererDir, "index.html"))) {
    throw new Error(
      `ZCode renderer build missing at ${rendererDir}; build it with: cd packages/desktop && node ../../node_modules/vite/bin/vite.js build --outDir ${rendererDir} --emptyOutDir`,
    );
  }
  const manifestFile = path.join(zcodeAppDir, "phase7-layout.json");
  const wanted = await fingerprint();
  if (existsSync(manifestFile)) {
    const current = JSON.parse(await readFile(manifestFile, "utf8"));
    if (JSON.stringify(current) === JSON.stringify(wanted) && existsSync(zcodeExe)) return { dir: zcodeAppDir, exe: zcodeExe, rebuilt: false };
  }
  await removeLayout();
  const dist = electronDist();
  await linkTree(dist, zcodeAppDir, (relative) => relative.toLowerCase() !== path.join("resources", "default_app.asar") && relative.toLowerCase() !== "electron.exe");
  await linkOrCopy(path.join(dist, "electron.exe"), zcodeExe);

  const resources = path.join(zcodeAppDir, "resources");
  const app = path.join(resources, "app");
  await linkOrCopy(path.join(desktopDir, "package.json"), path.join(app, "package.json"));
  await linkTree(path.join(desktopDir, "out/main"), path.join(app, "out/main"));
  await linkTree(path.join(desktopDir, "out/preload"), path.join(app, "out/preload"));
  for (const name of ["host", "scheduler", "plugin-sandbox", "metadata"]) {
    if (existsSync(path.join(desktopDir, "out", name))) await junction(path.join(desktopDir, "out", name), path.join(app, "out", name));
  }
  await junction(rendererDir, path.join(app, "out/renderer"));
  await junction(path.join(desktopDir, "node_modules"), path.join(app, "node_modules"));
  await junction(path.join(desktopDir, "bundled-agents/win32-x64/glm"), path.join(resources, "glm"));
  for (const tool of await readdir(path.join(desktopDir, "bundled-tools/win32-x64"))) {
    await junction(path.join(desktopDir, "bundled-tools/win32-x64", tool), path.join(resources, "tools", tool));
  }
  await linkOrCopy(path.join(repoRoot, "config/default.json"), path.join(resources, "config/default.json"));
  // 当前检出的 out/ 是 ZCODE_ENV=test 构建；electron-builder 按该环境打包 zcode-builtin.test.json。
  await linkOrCopy(path.join(repoRoot, "config/provider/zcode-builtin.test.json"), path.join(resources, "config/provider/zcode-builtin.json"));
  await linkOrCopy(path.join(desktopDir, "build/icon.png"), path.join(resources, "icon.png"));
  await linkOrCopy(path.join(desktopDir, "build/icon_windows.png"), path.join(resources, "icon_windows.png"));
  await linkOrCopy(path.join(desktopDir, "build/icon.ico"), path.join(resources, "tray_icon.ico"));
  await writeFile(manifestFile, JSON.stringify(wanted));
  return { dir: zcodeAppDir, exe: zcodeExe, rebuilt: true };
}

/** Removes junctions first (rm -r must never descend into packages/**), then the rest. */
async function removeLayout() {
  if (!existsSync(zcodeAppDir)) return;
  const stack = [zcodeAppDir];
  while (stack.length > 0) {
    const dir = stack.pop();
    for (const entry of await readdir(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      const info = await lstat(full);
      if (info.isSymbolicLink()) await rm(full, { force: true });
      else if (info.isDirectory()) stack.push(full);
    }
  }
  await rm(zcodeAppDir, { recursive: true, force: true });
}

// ---------------------------------------------------------------------------------------------
// ZCode size estimate (lower bound): what `electron-builder --dir` would install, minus the
// production node_modules inside app.asar (not reproducible offline), and without source maps.

async function dirFiles(root) {
  const out = [];
  for (const entry of await readdir(root, { recursive: true, withFileTypes: true })) {
    if (entry.isFile()) out.push(path.join(entry.parentPath, entry.name));
  }
  return out;
}

export async function stageZcodeInstallEstimate() {
  const staged = path.join(cacheRoot, "zcode-staged");
  await rm(staged, { recursive: true, force: true });
  const dist = electronDist();
  let bytes = 0;
  let files = 0;
  const add = async (source, relative) => {
    await linkOrCopy(source, path.join(staged, relative));
    bytes += (await stat(source)).size;
    files += 1;
  };
  for (const file of await dirFiles(dist)) {
    const relative = path.relative(dist, file);
    if (relative.toLowerCase() === path.join("resources", "default_app.asar")) continue;
    await add(file, relative.toLowerCase() === "electron.exe" ? "ZCode.exe" : relative);
  }
  const sources = [
    [path.join(desktopDir, "out"), path.join("resources", "app", "out"), (rel) => !rel.endsWith(".map") && !rel.startsWith("e2e")],
    [rendererDir, path.join("resources", "app", "out", "renderer"), (rel) => !rel.endsWith(".map")],
    [path.join(desktopDir, "bundled-agents/win32-x64/glm"), path.join("resources", "glm"), (rel) => !rel.endsWith(".map")],
    [path.join(desktopDir, "bundled-tools/win32-x64"), path.join("resources", "tools"), () => true],
  ];
  for (const [root, prefix, filter] of sources) {
    for (const file of await dirFiles(root)) {
      const relative = path.relative(root, file);
      // out/renderer 在 packages/desktop/out 中不存在（未构建），由 rendererDir 提供。
      if (!filter(relative.split(path.sep).join("/"))) continue;
      await add(file, path.join(prefix, relative));
    }
  }
  for (const [source, relative] of [
    [path.join(desktopDir, "package.json"), "resources/app/package.json"],
    [path.join(repoRoot, "config/default.json"), "resources/config/default.json"],
    [path.join(repoRoot, "config/provider/zcode-builtin.test.json"), "resources/config/provider/zcode-builtin.json"],
    [path.join(desktopDir, "build/icon.png"), "resources/icon.png"],
    [path.join(desktopDir, "build/icon_windows.png"), "resources/icon_windows.png"],
    [path.join(desktopDir, "build/icon.ico"), "resources/tray_icon.ico"],
  ]) {
    await add(source, relative);
  }
  return { staged, bytes, files };
}

/** Compresses `dir` with 7-Zip LZMA2 -mx=9 into `archive`; returns the archive size or null. */
export async function sevenZip(dir, archive, sevenZipExe) {
  await rm(archive, { force: true });
  const code = await new Promise((resolve) => {
    const child = spawn(sevenZipExe, ["a", "-t7z", "-m0=lzma2", "-mx=9", "-bd", "-bso0", "-bsp0", archive, "."], { cwd: dir, windowsHide: true, stdio: "ignore" });
    child.once("error", () => resolve(-1));
    child.once("close", (exitCode) => resolve(exitCode));
  });
  if (code !== 0 || !existsSync(archive)) return { bytes: null, note: `7-Zip exited with ${code}` };
  return { bytes: (await stat(archive)).size, note: "7z LZMA2 -mx=9" };
}
