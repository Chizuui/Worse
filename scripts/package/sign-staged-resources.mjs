#!/usr/bin/env node
// Signs the native files inside the staged host/ tree, which Tauri does not sign itself.
// Runs in beforeBuildCommand after stage-sidecar-resources.mjs (staging never signs).
//   Windows: every PE file (.exe/.dll/.node) without a valid signature is Authenticode-signed;
//            vendor-signed files (official node.exe, Microsoft OpenConsole.exe/conpty.dll) keep theirs.
//   macOS (untested): every Mach-O file is codesigned with the hardened runtime.
// Spec: Worse/docs/specs/harness/phase-6-packaging.md sections 4 and 5.
import { open, readdir } from "node:fs/promises";
import path from "node:path";
import process from "node:process";
import { fail, run, tauriDir } from "./lib.mjs";
import { acceptable, authenticodeStatus, signWindowsFile } from "./sign-file.mjs";
import { enforceSigningPolicy } from "./signing-policy.mjs";

const DEFAULT_ROOT = path.join(tauriDir, "resources/host");
const PE_EXTENSIONS = new Set([".exe", ".dll", ".node"]);
const MACHO_MAGIC = new Set(["feedface", "feedfacf", "cefaedfe", "cffaedfe", "cafebabe", "bebafeca"]);
/** Mach-O files that run JavaScript or load third-party addons need the JIT entitlements. */
const NODE_ENTITLED = new Set(["node", "harness-host"]);

async function listFiles(dir) {
  const entries = await readdir(dir, { withFileTypes: true, recursive: true });
  return entries.filter((entry) => entry.isFile()).map((entry) => path.join(entry.parentPath, entry.name));
}

async function isMachO(file) {
  const handle = await open(file, "r");
  try {
    const { buffer, bytesRead } = await handle.read(Buffer.alloc(4), 0, 4, 0);
    return bytesRead === 4 && MACHO_MAGIC.has(buffer.toString("hex"));
  } finally {
    await handle.close();
  }
}

async function signWindows(root, credentials) {
  const files = (await listFiles(root)).filter((file) => PE_EXTENSIONS.has(path.extname(file).toLowerCase()));
  const before = await authenticodeStatus(files);
  let kept = 0;
  let signed = 0;
  for (const file of files) {
    if (before.get(file)?.status === "Valid") {
      kept += 1;
      continue;
    }
    await signWindowsFile(file, credentials);
    signed += 1;
  }
  const after = await authenticodeStatus(files);
  const bad = files.filter((file) => !acceptable(after.get(file)));
  if (bad.length > 0) fail(`unsigned files remain in host/: ${bad.map((file) => path.relative(root, file)).join(", ")}`);
  process.stdout.write(`host/ PE files: ${signed} signed, ${kept} kept vendor signature\n`);
}

async function signMac(root, credentials) {
  const entitlements = path.join(tauriDir, "macos/node-entitlements.plist");
  let signed = 0;
  for (const file of await listFiles(root)) {
    if (!(await isMachO(file))) continue;
    const args = ["--force", "--timestamp", "--options", "runtime", "--sign", credentials.identity];
    if (NODE_ENTITLED.has(path.basename(file))) args.push("--entitlements", entitlements);
    const result = await run("codesign", [...args, file]);
    if (result.code !== 0) fail(`codesign ${file} failed: ${result.stderr.trim()}`);
    signed += 1;
  }
  process.stdout.write(`host/ Mach-O files: ${signed} signed\n`);
}

async function main() {
  const rootArg = process.argv.indexOf("--root");
  const root = rootArg > 0 && process.argv[rootArg + 1] ? path.resolve(process.argv[rootArg + 1]) : DEFAULT_ROOT;
  const decision = enforceSigningPolicy(process.platform, "sign host/ resources");
  if (decision.action !== "sign") return;
  if (process.platform === "win32") await signWindows(root, decision.credentials);
  else if (process.platform === "darwin") await signMac(root, decision.credentials);
  // Linux：包内文件不做逐个签名，签名针对最终产物（build-installer.mjs 写分离式 .asc）。
}

await main();
