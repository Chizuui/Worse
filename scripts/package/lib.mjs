// Shared helpers for Worse/scripts/package/*. See docs/specs/harness/phase-6-packaging.md.
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { readFile, stat } from "node:fs/promises";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

export const packageDir = path.dirname(fileURLToPath(import.meta.url));
export const worseRoot = path.resolve(packageDir, "../..");
export const repoRoot = path.resolve(worseRoot, "..");
export const appDir = path.join(worseRoot, "apps/harness-app");
export const tauriDir = path.join(appDir, "src-tauri");
export const NODE_LOCK_FILE = path.join(packageDir, "node-runtime.lock.json");
/** Layout-relative directory of the bundled Node runtime inside `host/`. */
export const NODE_RUNTIME_DIR = "runtime/node";

const FAILED = Symbol("harness-package-failure");

// 修复原因：fetch() 的 undici 句柄未关闭时调用 process.exit()，Windows 上 libuv 会断言崩溃
// （src\win\async.c "!(handle->flags & UV_HANDLE_CLOSING)"），退出码变成 0xC0000409。
// 改为设置 exitCode 并抛出带标记的错误，由下面的处理器吞掉，进程自然退出。
for (const event of ["uncaughtException", "unhandledRejection"]) {
  process.on(event, (error) => {
    if (error?.[FAILED]) return;
    process.stderr.write(`${error?.stack ?? error}\n`);
    process.exitCode = 1;
  });
}

export function fail(message, command) {
  process.stderr.write(`error: ${message}\n${command ? `run: ${command}\n` : ""}`);
  process.exitCode = 1;
  throw Object.assign(new Error(message), { [FAILED]: true });
}

export async function isFile(target) {
  return stat(target).then((s) => s.isFile(), () => false);
}

export async function readJson(file) {
  return JSON.parse(await readFile(file, "utf8"));
}

export function sha256File(file) {
  return new Promise((resolve, reject) => {
    const hash = createHash("sha256");
    createReadStream(file)
      .on("error", reject)
      .on("data", (chunk) => hash.update(chunk))
      .on("end", () => resolve(hash.digest("hex")));
  });
}

/** Node's `process.platform-process.arch` key used by the lock file and the cache. */
export function platformKey(platform = process.platform, arch = process.arch) {
  return `${platform}-${arch}`;
}

export function nodeBinaryName(platform = process.platform) {
  return platform === "win32" ? "node.exe" : "node";
}

/** Cache root for verified Node runtimes (`HARNESS_NODE_CACHE_DIR` overrides). */
export function nodeCacheRoot() {
  const explicit = process.env.HARNESS_NODE_CACHE_DIR?.trim();
  return explicit ? path.resolve(explicit) : path.join(worseRoot, ".work/cache/node-runtime");
}

/** Directory holding the extracted, verified runtime for one platform. */
export function nodeCacheDir(version, key) {
  return path.join(nodeCacheRoot(), `v${version}`, key);
}

/** The `[tools] node` version pinned in the repository's mise.toml. */
export async function miseNodeVersion() {
  const text = await readFile(path.join(repoRoot, "mise.toml"), "utf8");
  const tools = text.split(/^\[/m).find((section) => section.startsWith("tools]"));
  const match = tools?.match(/^\s*node\s*=\s*"([^"]+)"/m);
  return match?.[1] ?? null;
}

/**
 * Runs a command without a shell; resolves with { code, stdout, stderr }. `inherit: true` streams
 * output to this process instead of capturing it.
 */
export function run(command, args, options = {}) {
  const { inherit = false, input, ...spawnOptions } = options;
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, {
      windowsHide: true,
      stdio: inherit ? ["ignore", "inherit", "inherit"] : ["pipe", "pipe", "pipe"],
      ...spawnOptions,
    });
    let stdout = "";
    let stderr = "";
    child.stdout?.setEncoding("utf8").on("data", (chunk) => (stdout += chunk));
    child.stderr?.setEncoding("utf8").on("data", (chunk) => (stderr += chunk));
    child.once("error", reject);
    child.once("close", (code) => resolve({ code: code ?? 1, stdout, stderr }));
    if (child.stdin) {
      if (input !== undefined) child.stdin.end(input);
      else child.stdin.end();
    }
  });
}
