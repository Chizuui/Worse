#!/usr/bin/env node
// Downloads, verifies and caches the official Node.js runtime bundled into host/runtime/node.
//
// Verification (fail closed): archive SHA-256 == value pinned in node-runtime.lock.json AND == the
// line in the release's SHASUMS256.txt; after extraction (host platform only) `node --version`
// must print the pinned version. Only node(.exe) and LICENSE are extracted. The script never
// writes into host/; stage-sidecar-resources.mjs copies from the cache.
// Spec: Worse/docs/specs/harness/phase-6-packaging.md section 2.
import { createWriteStream } from "node:fs";
import { chmod, cp, mkdir, rename, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import process from "node:process";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { pathToFileURL } from "node:url";
import {
  fail,
  isFile,
  miseNodeVersion,
  nodeBinaryName,
  nodeCacheDir,
  nodeCacheRoot,
  NODE_LOCK_FILE,
  platformKey,
  readJson,
  run,
  sha256File,
} from "./lib.mjs";

export const FETCH_COMMAND = "node Worse/scripts/package/fetch-node-runtime.mjs";

function parseArgs(argv) {
  const options = { platform: process.platform, arch: process.arch, quiet: false };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--platform" && argv[index + 1]) options.platform = argv[(index += 1)];
    else if (arg === "--arch" && argv[index + 1]) options.arch = argv[(index += 1)];
    else if (arg === "--quiet") options.quiet = true;
    else if (arg === "--help" || arg === "-h") options.help = true;
    else fail(`unknown argument '${arg}'`, `${FETCH_COMMAND} [--platform <p>] [--arch <a>]`);
  }
  return options;
}

async function download(url, target) {
  const response = await fetch(url, { redirect: "follow" });
  if (!response.ok || !response.body) throw new Error(`GET ${url} -> HTTP ${response.status}`);
  const partial = `${target}.part-${process.pid}`;
  await pipeline(Readable.fromWeb(response.body), createWriteStream(partial));
  await rename(partial, target);
}

async function fetchText(url) {
  const response = await fetch(url, { redirect: "follow" });
  if (!response.ok) throw new Error(`GET ${url} -> HTTP ${response.status}`);
  return response.text();
}

/** SHA-256 for `file` from a SHASUMS256.txt body, or null. */
function shasumFor(text, file) {
  for (const line of text.split("\n")) {
    const [digest, name] = line.trim().split(/\s+/);
    if (name === file) return digest.toLowerCase();
  }
  return null;
}

/** `tar` that can read zip archives on Windows (bsdtar in System32; Git's GNU tar cannot). */
function tarCommand() {
  if (process.platform !== "win32") return "tar";
  return path.join(process.env.SystemRoot ?? "C:\\Windows", "System32", "tar.exe");
}

/**
 * Verified cached runtime for (version, key), or null when absent, tampered, or recorded for a
 * different archive than the one pinned in the lock (`archiveSha256`).
 */
export async function readVerifiedRuntime(version, key, platform, archiveSha256) {
  const dir = nodeCacheDir(version, key);
  const record = path.join(dir, "verified.json");
  if (!(await isFile(record))) return null;
  const verified = await readJson(record);
  const binary = path.join(dir, nodeBinaryName(platform));
  if (!(await isFile(binary)) || !(await isFile(path.join(dir, "LICENSE")))) return null;
  if (verified.version !== version || verified.archiveSha256 !== archiveSha256?.toLowerCase()) return null;
  if ((await sha256File(binary)) !== verified.binarySha256) return null;
  return { dir, binary, license: path.join(dir, "LICENSE"), ...verified };
}

export async function fetchNodeRuntime({ platform = process.platform, arch = process.arch, quiet = false } = {}) {
  const log = (line) => (quiet ? undefined : process.stdout.write(`${line}\n`));
  const lock = await readJson(NODE_LOCK_FILE);
  const pinned = await miseNodeVersion();
  if (pinned !== lock.version) {
    fail(`node-runtime.lock.json pins ${lock.version} but mise.toml pins ${pinned}; update the lock from SHASUMS256.txt`);
  }
  const key = platformKey(platform, arch);
  const artifact = lock.artifacts[key];
  if (!artifact) fail(`no pinned Node runtime for ${key} in ${NODE_LOCK_FILE}`);

  const cached = await readVerifiedRuntime(lock.version, key, platform, artifact.sha256);
  if (cached) {
    log(`node runtime v${lock.version} ${key}: cached and verified (${cached.dir})`);
    return cached;
  }

  const versionDir = path.join(nodeCacheRoot(), `v${lock.version}`);
  await mkdir(versionDir, { recursive: true });
  const archive = path.join(versionDir, artifact.file);

  // 1) 官方 SHASUMS256.txt 与仓库锁定值必须一致（两处独立校验，任何一处不符即失败）。
  const shasums = await fetchText(`${lock.baseUrl}SHASUMS256.txt`);
  const published = shasumFor(shasums, artifact.file);
  if (published !== artifact.sha256.toLowerCase()) {
    fail(`SHASUMS256.txt lists ${published ?? "nothing"} for ${artifact.file}, lock pins ${artifact.sha256}`);
  }
  // 2) Archive: reuse a cached download only when its digest matches.
  if (!(await isFile(archive)) || (await sha256File(archive)) !== published) {
    log(`downloading ${lock.baseUrl}${artifact.file}`);
    await rm(archive, { force: true });
    await download(`${lock.baseUrl}${artifact.file}`, archive);
  }
  const archiveSha256 = await sha256File(archive);
  if (archiveSha256 !== published) {
    await rm(archive, { force: true });
    fail(`checksum mismatch for ${artifact.file}: got ${archiveSha256}, expected ${published}`);
  }

  // 3) Extract only node(.exe) + LICENSE into a scratch dir, then move into place.
  const root = artifact.file.replace(/\.(zip|tar\.gz|tar\.xz)$/, "");
  const binaryMember = platform === "win32" ? `${root}/node.exe` : `${root}/bin/node`;
  const scratch = path.join(versionDir, `${key}.extract-${process.pid}`);
  await rm(scratch, { recursive: true, force: true });
  await mkdir(scratch, { recursive: true });
  const extracted = await run(tarCommand(), ["-xf", archive, "-C", scratch, binaryMember, `${root}/LICENSE`]);
  if (extracted.code !== 0) fail(`extracting ${artifact.file} failed: ${extracted.stderr.trim()}`);
  const target = nodeCacheDir(lock.version, key);
  await rm(target, { recursive: true, force: true });
  await mkdir(target, { recursive: true });
  const binary = path.join(target, nodeBinaryName(platform));
  await cp(path.join(scratch, ...binaryMember.split("/")), binary);
  await cp(path.join(scratch, root, "LICENSE"), path.join(target, "LICENSE"));
  await rm(scratch, { recursive: true, force: true });
  if (platform !== "win32") await chmod(binary, 0o755);

  // 4) Runtime self-check when the binary can run here.
  if (platform === process.platform && arch === process.arch) {
    const probe = await run(binary, ["--version"]);
    if (probe.code !== 0 || probe.stdout.trim() !== `v${lock.version}`) {
      await rm(target, { recursive: true, force: true });
      fail(`extracted runtime reports '${probe.stdout.trim()}' (exit ${probe.code}), expected v${lock.version}`);
    }
  }
  const verified = {
    version: lock.version,
    platform: key,
    archive: artifact.file,
    archiveSha256,
    binarySha256: await sha256File(binary),
    source: `${lock.baseUrl}${artifact.file}`,
    verifiedAt: new Date().toISOString(),
  };
  await writeFile(path.join(target, "verified.json"), `${JSON.stringify(verified, null, 2)}\n`);
  log(`node runtime v${lock.version} ${key}: verified sha256 ${archiveSha256} -> ${target}`);
  return { dir: target, binary, license: path.join(target, "LICENSE"), ...verified };
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  const options = parseArgs(process.argv.slice(2));
  if (options.help) {
    process.stdout.write(`usage: ${FETCH_COMMAND} [--platform <p>] [--arch <a>] [--quiet]\n`);
  } else {
    await fetchNodeRuntime(options);
  }
}
