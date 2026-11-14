#!/usr/bin/env node
// Builds Harness release installers (contract for the updater work and the Phase 6 gates).
//
//   node Worse/scripts/package/build-installer.mjs --version <semver> [--bundles nsis] [--config <file>]...
//
// Always a release build (HARNESS_RELEASE=1): the signing policy fails closed unless credentials
// are configured or HARNESS_ALLOW_UNSIGNED=1. The version is overridden through a generated
// --config file; tauri.conf.json is never rewritten. Extra --config files are merged after it in
// order (Tauri CLI semantics). Builds share Worse/target, so a lock serialises concurrent callers.
// Last stdout line on success: HARNESS_BUNDLE_OK {json}.
// Spec: Worse/docs/specs/harness/phase-6-packaging.md section 1.
import { spawn } from "node:child_process";
import { readdir, rm, stat, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import path from "node:path";
import process from "node:process";
import { BuildLockTimeoutError, acquireBuildLock } from "./buildLock.mjs";
import { appDir, fail, packageDir, run, tauriDir, worseRoot } from "./lib.mjs";
import { enforceSigningPolicy } from "./signing-policy.mjs";

const USAGE =
  "node Worse/scripts/package/build-installer.mjs --version <semver> [--bundles <list>] [--config <file>]...";
const DEFAULT_BUNDLES = { win32: ["nsis"], darwin: ["app", "dmg"], linux: ["appimage", "deb", "rpm"] };
const WORK_DIR = path.join(worseRoot, ".work/tmp/package");
// 锁文件（非目录）：原子创建 + token 校验释放，见 buildLock.mjs。
const LOCK_FILE = path.join(WORK_DIR, "build-lock.json");
const LOCK_WAIT_MS = 30 * 60 * 1000;
const SEMVER = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;

function parseArgs(argv) {
  const options = { version: null, bundles: DEFAULT_BUNDLES[process.platform] ?? [], configs: [] };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    const value = argv[index + 1];
    if (arg === "--version" && value) options.version = argv[(index += 1)];
    else if (arg === "--bundles" && value) options.bundles = argv[(index += 1)].split(/[\s,]+/).filter(Boolean);
    else if (arg === "--config" && value) options.configs.push(path.resolve(argv[(index += 1)]));
    else if (arg === "--help" || arg === "-h") options.help = true;
    else fail(`unknown argument '${arg}'`, USAGE);
  }
  return options;
}

async function acquireLock() {
  try {
    const lock = await acquireBuildLock({ lockFile: LOCK_FILE, waitMs: LOCK_WAIT_MS, log: (line) => process.stdout.write(`${line}\n`) });
    return lock.release;
  } catch (error) {
    if (error instanceof BuildLockTimeoutError) fail(`${error.message} after 30 min`);
    throw error;
  }
}

function runTauri(args, env) {
  const require = createRequire(path.join(appDir, "package.json"));
  const cli = require.resolve("@tauri-apps/cli/tauri.js");
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [cli, ...args], { cwd: appDir, env, stdio: "inherit", windowsHide: true });
    child.once("error", reject);
    child.once("close", (code) => resolve(code ?? 1));
  });
}

async function collectArtifacts(bundles, version, since) {
  const artifacts = [];
  const bundleRoot = path.join(worseRoot, "target/release/bundle");
  for (const bundle of bundles) {
    const dir = path.join(bundleRoot, bundle === "app" ? "macos" : bundle);
    for (const name of await readdir(dir).catch(() => [])) {
      const file = path.join(dir, name);
      const info = await stat(file);
      if (!info.isFile() && !name.endsWith(".app")) continue;
      if (!name.includes(version) && bundle !== "app") continue;
      if (info.mtimeMs < since) continue;
      if (name.endsWith(".asc")) continue;
      artifacts.push({ bundle, path: file, bytes: info.isFile() ? info.size : null });
    }
  }
  return artifacts;
}

/** Linux (untested): detached ASCII signatures + rpmsign when available. */
async function signLinuxArtifacts(artifacts, decision) {
  if (process.platform !== "linux" || decision.action !== "sign") return;
  const key = decision.credentials.key;
  const passphrase = process.env.HARNESS_LINUX_GPG_PASSPHRASE;
  const passArgs = passphrase ? ["--pinentry-mode", "loopback", "--passphrase-fd", "0"] : [];
  for (const artifact of artifacts) {
    if (artifact.path.endsWith(".rpm") && (await run("rpmsign", ["--version"]).catch(() => ({ code: 1 }))).code === 0) {
      const rpm = await run("rpmsign", ["--addsign", "--define", `_gpg_name ${key}`, artifact.path]);
      if (rpm.code !== 0) fail(`rpmsign ${artifact.path} failed: ${rpm.stderr.trim()}`);
    }
    const gpg = await run(
      "gpg",
      ["--batch", "--yes", ...passArgs, "--local-user", key, "--armor", "--detach-sign", artifact.path],
      passphrase ? { input: passphrase } : {},
    );
    if (gpg.code !== 0) fail(`gpg signing ${artifact.path} failed: ${gpg.stderr.trim()}`);
    artifact.signature = `${artifact.path}.asc`;
  }
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options.help) {
    process.stdout.write(`usage: ${USAGE}\n`);
    return;
  }
  if (!options.version || !SEMVER.test(options.version)) fail("--version <semver> is required", USAGE);
  if (options.bundles.length === 0) fail(`no default bundles for ${process.platform}; pass --bundles`, USAGE);

  // 发布构建：先按签名策略判定（缺凭据且未显式允许时在编译前就失败）。
  process.env.HARNESS_RELEASE = "1";
  const decision = enforceSigningPolicy(process.platform, "installer signing");

  const release = await acquireLock();
  try {
    const override = { version: options.version };
    if (process.platform === "win32" && decision.action === "sign") {
      // Tauri 对 harness-app.exe、卸载程序和安装包调用该命令；路径用绝对路径，避免依赖 makensis 的工作目录。
      override.bundle = {
        windows: { signCommand: { cmd: "node", args: [path.join(packageDir, "sign-file.mjs"), "%1"] } },
      };
    }
    const overrideFile = path.join(WORK_DIR, `tauri.override.${process.pid}.json`);
    await writeFile(overrideFile, `${JSON.stringify(override, null, 2)}\n`);
    const started = Date.now();
    const args = ["build", "--bundles", options.bundles.join(","), "--config", overrideFile];
    for (const extra of options.configs) args.push("--config", extra);
    process.stdout.write(`tauri ${args.join(" ")}\n`);
    const code = await runTauri(args, { ...process.env, HARNESS_RELEASE: "1" });
    await rm(overrideFile, { force: true });
    if (code !== 0) fail(`tauri build exited with ${code}`);

    const artifacts = await collectArtifacts(options.bundles, options.version, started - 1000);
    if (artifacts.length === 0) fail(`tauri build produced no ${options.bundles.join(",")} artifact for ${options.version}`);
    await signLinuxArtifacts(artifacts, decision);
    const summary = {
      version: options.version,
      signed: decision.action === "sign",
      tauriDir,
      artifacts,
    };
    process.stdout.write(`HARNESS_BUNDLE_OK ${JSON.stringify(summary)}\n`);
  } finally {
    await release();
  }
}

await main();
