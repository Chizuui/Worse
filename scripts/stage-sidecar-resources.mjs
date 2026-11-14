#!/usr/bin/env node
// Stages the packaged Host layout for `tauri build`.
//
// Output: Worse/apps/harness-app/src-tauri/resources/host/ (bundled as the `host/` resource by
// tauri.{windows,macos,linux}.conf.json). The directory mirrors the checkout paths, so the shell
// resolver (src-tauri/src/host/layout.rs), the Host's findUpward("apps/zcode-cli/...") from its
// cwd, and rust-bridge's `Worse/target/release/harness-host` lookup all work unchanged.
// Bare-module dependencies of the staged JavaScript are copied (pnpm symlinks dereferenced) into
// `node_modules/` at the layout root; a conflicting version is nested under its requester.
// Inputs are build outputs only; this script never builds and fails naming the build command.
import { spawn } from "node:child_process";
import { cp, mkdir, readdir, readFile, realpath, rm, stat, writeFile } from "node:fs/promises";
import { isBuiltin } from "node:module";
import path from "node:path";
import process from "node:process";
import { fileURLToPath, pathToFileURL } from "node:url";
import { FETCH_COMMAND, readVerifiedRuntime } from "./package/fetch-node-runtime.mjs";
import { NODE_LOCK_FILE, NODE_RUNTIME_DIR, nodeBinaryName, platformKey } from "./package/lib.mjs";

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, "../..");
const defaultOut = path.join(repoRoot, "Worse/apps/harness-app/src-tauri/resources/host");
const exe = process.platform === "win32" ? ".exe" : "";
const MANIFEST = "harness-host-manifest.json";

// Each source: repo-relative path, required entry, build command, package dir to resolve deps from.
// `scan: false` marks a self-contained bundle: ZCode's Electron package ships zcode.cjs alone.
const SOURCES = [
  {
    dir: "Worse/sidecars/host-node/dist",
    entry: "main.js",
    build: "pnpm --dir Worse/sidecars/host-node build",
    pkg: "Worse/sidecars/host-node",
  },
  {
    // Phase 4: ZCode's web remote-control manager bundled by host-node's build
    // (scripts/build-webRemote-vendor.mjs); src/webRemoteHost.ts loads it from `../dist-vendor/`.
    dir: "Worse/sidecars/host-node/dist-vendor",
    entry: "webRemoteRuntime.js",
    build: "pnpm --dir Worse/sidecars/host-node build",
    pkg: "Worse/sidecars/host-node",
  },
  {
    dir: "Worse/sidecars/rust-bridge/dist",
    entry: "index.js",
    build: "pnpm --dir Worse/sidecars/rust-bridge build",
    pkg: "Worse/sidecars/rust-bridge",
  },
  {
    // ZCode terminal policy bundled by rust-bridge's build (scripts/build-vendor.mjs); the Rust PTY
    // route loads it from `../../dist-vendor/` relative to the bridge dist/.
    dir: "Worse/sidecars/rust-bridge/dist-vendor",
    entry: "terminalPolicy.js",
    build: "pnpm --dir Worse/sidecars/rust-bridge build",
    pkg: "Worse/sidecars/rust-bridge",
  },
  {
    dir: "packages/desktop/out/host",
    entry: "index.js",
    build: "pnpm --filter @zcode/desktop exec tsup --config tsup.config.ts",
    pkg: "packages/desktop",
  },
  {
    // Phase 5 (plugins): plugin UI alias script, Gen UI runtime and vendor files served by
    // host-node's plugin sandbox owner from `<host entry>/../plugin-sandbox` (browser bundles only).
    dir: "packages/desktop/out/plugin-sandbox",
    entry: "plugin-sandbox-alias.js",
    build: "pnpm --filter @zcode/desktop exec tsup --config tsup.config.ts",
    pkg: "packages/desktop",
    scan: false,
  },
  {
    dir: "apps/zcode-cli/packages/cli/dist",
    entry: "zcode.cjs",
    build: "pnpm build:desktop-agent-cli",
    pkg: "apps/zcode-cli/packages/cli",
    scan: false,
  },
];
const FILES = [
  { file: "config/provider/zcode-builtin.json", build: "restore config/provider from the checkout" },
  { file: "config/provider/zcode-builtin.test.json", build: "restore config/provider from the checkout" },
  { file: `Worse/target/release/harness-host${exe}`, build: "cargo build --release -p harness-host (in Worse/)" },
];
// Bundled native search tools, resolved by the Host's findRuntimeToolBinary from
// `<cwd>/packages/desktop/bundled-tools/<platform>-<arch>/<tool>` (cwd = layout root). ripgrep is
// required (ZCode's electron-builder config always ships it); ugrep and bfs ship when prepared.
const TOOLS_DIR = `packages/desktop/bundled-tools/${process.platform}-${process.arch}`;
const TOOLS_BUILD = "pnpm --filter @zcode/desktop prepare:native-search";
const TOOLS = [
  { id: "ripgrep", binary: `rg${exe}`, required: true },
  { id: "ugrep", binary: `ugrep${exe}`, required: false },
  { id: "bfs", binary: `bfs${exe}`, required: false },
];
// Windows: node-pty's ConPTY pair, staged next to harness-host (see stageConpty).
const CONPTY_FILES = ["conpty.dll", "OpenConsole.exe"];
const RUST_HOST_DIR = "Worse/target/release";
// Session methods the bridge calls besides its route table (contract r2 section 9).
const SESSION_METHODS = ["pty.spawn", "pty.write", "pty.resize", "pty.kill", "watch.subscribe", "watch.unsubscribe"];
// Source maps, typings, and native debug symbols are never needed at runtime.
const SKIPPED = [/\.map$/, /\.d\.ts$/, /\.tsbuildinfo$/, /\.pdb$/];
// Build-time or type-only packages, excluded like packages/desktop/electron-builder.config.js does.
const EXCLUDED_PACKAGES = new Set(["nan", "node-addon-api", "node-gyp", "cpu-features", "buildcheck"]);
const isExcludedPackage = (name) => EXCLUDED_PACKAGES.has(name) || name.startsWith("@types/");
// node-pty ships prebuilds for every platform; keep only the build host's (ZCode asarUnpack rule).
const PREBUILD_KEY = `${process.platform}-${process.arch}`;
const SPECIFIER = /\bfrom\s*["']([^"']+)["']|\bimport\(\s*["']([^"']+)["']\s*\)|\brequire\d*\(\s*["']([^"']+)["']\s*\)/g;
const PACKAGE_NAME = /^(@[a-z0-9][\w.~-]*\/)?[a-z0-9][\w.~-]*$/;

function fail(message, command) {
  process.stderr.write(`error: ${message}\n${command ? `run: ${command}\n` : ""}`);
  process.exit(1);
}

async function isFile(target) {
  return stat(target).then((s) => s.isFile(), () => false);
}

function parseArgs(argv) {
  const options = { out: defaultOut, skipNode: false };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--out" && argv[index + 1]) options.out = path.resolve(argv[(index += 1)]);
    else if (arg === "--skip-node") options.skipNode = true;
    else if (arg === "--help" || arg === "-h") options.help = true;
    else fail(`unknown argument '${arg}'`, "node Worse/scripts/stage-sidecar-resources.mjs [--out <dir>] [--skip-node]");
  }
  return options;
}

async function listFiles(dir) {
  const entries = await readdir(dir, { recursive: true, withFileTypes: true });
  return entries
    .filter((entry) => entry.isFile())
    .map((entry) => path.join(entry.parentPath, entry.name))
    .filter((file) => !SKIPPED.some((pattern) => pattern.test(file)));
}

/**
 * Bare package names referenced by an ESM file. Static `from` imports are required; dynamic
 * import()/require() are optional: staged when resolvable, otherwise only reported.
 */
function collectSpecifiers(source, into) {
  for (const match of source.matchAll(SPECIFIER)) {
    const raw = match[1] ?? match[2] ?? match[3];
    if (!raw || raw.startsWith(".") || raw.startsWith("/") || isBuiltin(raw)) continue;
    const parts = raw.split("/");
    const name = raw.startsWith("@") ? parts.slice(0, 2).join("/") : parts[0];
    if (!PACKAGE_NAME.test(name)) continue;
    const optional = match[1] === undefined;
    into.set(name, (into.get(name) ?? true) && optional);
  }
}

/** Node's lookup: `<dir>/node_modules/<name>` walking up from `fromDir`. */
async function findPackage(name, fromDir) {
  for (let dir = fromDir; ; dir = path.dirname(dir)) {
    const candidate = path.join(dir, "node_modules", name);
    if (await isFile(path.join(candidate, "package.json"))) return realpath(candidate);
    if (path.dirname(dir) === dir) return null;
  }
}

async function readJson(file) {
  return JSON.parse(await readFile(file, "utf8"));
}

function keepPackageFile(segments) {
  if (segments.includes("node_modules")) return false;
  if (segments[0] === "prebuilds" && segments[1] && segments[1] !== PREBUILD_KEY) return false;
  return !SKIPPED.some((pattern) => pattern.test(segments.at(-1) ?? ""));
}

class DependencyStager {
  constructor(outRoot) {
    this.outRoot = outRoot;
    this.topLevel = new Map(); // name -> real path placed at <out>/node_modules
    this.placed = new Map(); // `${real}|${stagedDir}` -> true
    this.packages = [];
    this.unresolved = [];
  }

  async stage(name, fromRealDir, requesterStagedDir, optional) {
    if (isExcludedPackage(name)) return;
    const real = await findPackage(name, fromRealDir);
    if (!real) {
      this.unresolved.push({ name, from: path.relative(repoRoot, fromRealDir), optional });
      if (!optional) fail(`cannot resolve '${name}' from ${fromRealDir}`, "pnpm install");
      return;
    }
    // 修复原因（Phase 6 门禁发现）：仓库根的 `pnpm typecheck`（tsc -b desktop/tsconfig.host.json）会把
    // 逐文件编译结果写进 packages/desktop/out/host，覆盖 tsup 打包出的 index.js；新 index.js 直接
    // import "@zcode/shared" 等工作区包，暂存后这些包落在 host/node_modules 下且入口是 .ts 源码，
    // Node 拒绝对 node_modules 内文件做类型擦除（ERR_UNSUPPORTED_NODE_MODULES_TYPE_STRIPPING），
    // 打包版 Host 启动即失败。工作区包本应被 tsup 打进入口，出现即说明入口不是打包产物。
    if (!real.split(path.sep).includes("node_modules")) {
      fail(
        `'${name}' resolves to the workspace package ${real}; the staged JavaScript must not import ` +
          "workspace sources (the Host entry is probably tsc output, not the tsup bundle)",
        "pnpm --filter @zcode/desktop exec tsup --config tsup.config.ts",
      );
    }
    const top = this.topLevel.get(name);
    const target =
      top === undefined || top === real
        ? path.join(this.outRoot, "node_modules", name)
        : path.join(requesterStagedDir, "node_modules", name);
    if (top === undefined) this.topLevel.set(name, real);
    const key = `${real}|${target}`;
    if (this.placed.has(key)) return;
    this.placed.set(key, true);
    await cp(real, target, {
      recursive: true,
      dereference: true,
      filter: (src) => keepPackageFile(path.relative(real, src).split(path.sep)),
    });
    const manifest = await readJson(path.join(real, "package.json"));
    this.packages.push({ name, version: manifest.version, path: path.relative(this.outRoot, target) });
    const deps = [
      ...Object.keys(manifest.dependencies ?? {}).map((dep) => [dep, false]),
      ...Object.keys(manifest.optionalDependencies ?? {}).map((dep) => [dep, true]),
    ];
    for (const [dep, depOptional] of deps) await this.stage(dep, real, target, depOptional);
  }
}

/**
 * 修复原因（Phase 3c 门禁发现）：Worse/target/release/harness-host 可能是旧版本（早于 Phase 3b 的
 * pty/watch 方法），暂存后 rust-bridge 握手看到方法未声明，终端静默回落到 TS。这里在暂存前做一次
 * `initialize` 握手，要求二进制声明 rust-bridge 会路由的全部方法，否则失败并提示重新编译。
 */
async function probeRustHost(binary) {
  const { listRoutes } = await import(
    pathToFileURL(path.join(repoRoot, "Worse/sidecars/rust-bridge/dist/routing/routeTable.js")).href
  );
  const required = [...listRoutes().map((entry) => entry.route.rustMethod), ...SESSION_METHODS];
  const child = spawn(binary, [], { cwd: path.dirname(binary), stdio: ["pipe", "pipe", "ignore"], windowsHide: true });
  const result = await new Promise((resolve, reject) => {
    let buffer = "";
    const timer = setTimeout(() => reject(new Error("initialize timed out")), 15_000);
    child.once("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
    child.stdout.setEncoding("utf8").on("data", (chunk) => {
      buffer += chunk;
      for (const line of buffer.split("\n").slice(0, -1)) {
        const message = JSON.parse(line);
        if (message.id === 0) {
          clearTimeout(timer);
          resolve(message.result);
        }
      }
      buffer = buffer.slice(buffer.lastIndexOf("\n") + 1);
    });
    child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id: 0, method: "initialize", params: { protocolVersion: 1, client: "stage-sidecar-resources" } })}\n`);
  }).finally(() => child.stdin.end());
  await new Promise((resolve) => (child.exitCode === null ? child.once("exit", resolve) : resolve()));
  const advertised = new Set(Array.isArray(result?.methods) ? result.methods : []);
  const missing = required.filter((method) => !advertised.has(method));
  if (missing.length > 0) {
    fail(`${binary} is stale: it does not advertise ${missing.join(", ")}`, "cargo build --release -p harness-host (in Worse/)");
  }
  return { serverVersion: result.serverVersion, methods: advertised.size };
}

/** Copies ripgrep/ugrep/bfs from packages/desktop/bundled-tools; fails only when ripgrep is missing. */
async function stageTools(outRoot) {
  const staged = [];
  for (const tool of TOOLS) {
    const dir = `${TOOLS_DIR}/${tool.id}`;
    const binary = path.join(repoRoot, dir, tool.binary);
    if (!(await isFile(binary))) {
      if (tool.required) fail(`missing ${binary}`, TOOLS_BUILD);
      continue;
    }
    await cp(path.join(repoRoot, dir), path.join(outRoot, dir), { recursive: true, dereference: true });
    staged.push(`${dir}/${tool.binary}`);
  }
  return staged;
}

/**
 * 修复原因（Phase 3c 打包）：Phase 3b 让 harness-host 通过 node-pty/package.json 找到 conpty.dll，
 * 打包后这条路径只是碰巧可用（取决于 node-pty 是否被暂存以及 prebuilds 过滤规则），一旦缺失，Rust PTY
 * 会静默退回 Win10 自带 ConPTY（回显慢一帧）。这里把 conpty.dll + OpenConsole.exe 放到 harness-host
 * 同目录：portable-pty 的 LoadLibrary 先查应用目录，rust-bridge 的 resolveConptyDir 看到同目录已有
 * conpty.dll 就不再改 PATH，打包布局因此不依赖 node-pty 的包结构。
 */
async function stageConpty(outRoot) {
  if (process.platform !== "win32") return null;
  const nodePty = await findPackage("node-pty", await realpath(path.join(repoRoot, "packages/desktop")));
  const from = nodePty === null ? null : path.join(nodePty, "prebuilds", `win32-${process.arch}`, "conpty");
  for (const file of CONPTY_FILES) {
    if (from === null || !(await isFile(path.join(from, file)))) {
      fail(`missing node-pty ConPTY file ${file}${from === null ? "" : ` in ${from}`}`, "pnpm install");
    }
  }
  for (const file of CONPTY_FILES) {
    await cp(path.join(from, file), path.join(outRoot, RUST_HOST_DIR, file));
  }
  return CONPTY_FILES.map((file) => `${RUST_HOST_DIR}/${file}`);
}

/**
 * Phase 6: the verified official Node runtime (fetch-node-runtime.mjs cache) goes to
 * host/runtime/node so packaged builds do not need Node on PATH. Returns the manifest entry.
 */
async function resolveNodeRuntime() {
  const lock = JSON.parse(await readFile(NODE_LOCK_FILE, "utf8"));
  const key = platformKey();
  const verified = await readVerifiedRuntime(lock.version, key, process.platform, lock.artifacts[key]?.sha256);
  if (verified === null) fail(`no verified Node.js v${lock.version} runtime cached for ${key}`, FETCH_COMMAND);
  return verified;
}

async function stageNodeRuntime(outRoot, verified) {
  const binary = `${NODE_RUNTIME_DIR}/${nodeBinaryName()}`;
  const target = path.join(outRoot, binary);
  await mkdir(path.join(outRoot, NODE_RUNTIME_DIR), { recursive: true });
  await cp(verified.binary, target);
  await cp(verified.license, path.join(outRoot, NODE_RUNTIME_DIR, "LICENSE"));
  const version = await new Promise((resolve) => {
    const child = spawn(target, ["--version"], { stdio: ["ignore", "pipe", "ignore"], windowsHide: true });
    let out = "";
    child.stdout.setEncoding("utf8").on("data", (chunk) => (out += chunk));
    child.once("error", () => resolve(""));
    child.once("close", () => resolve(out.trim()));
  });
  if (version !== `v${verified.version}`) {
    fail(`staged ${binary} reports '${version}' instead of v${verified.version}`, FETCH_COMMAND);
  }
  return {
    version: verified.version,
    path: binary,
    sha256: verified.binarySha256,
    archive: verified.archive,
    archiveSha256: verified.archiveSha256,
    source: verified.source,
    license: `${NODE_RUNTIME_DIR}/LICENSE`,
  };
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options.help) {
    process.stdout.write("usage: node Worse/scripts/stage-sidecar-resources.mjs [--out <dir>] [--skip-node]\n");
    return;
  }
  for (const source of SOURCES) {
    const entry = path.join(repoRoot, source.dir, source.entry);
    if (!(await isFile(entry))) fail(`missing ${entry}`, source.build);
  }
  for (const { file, build } of FILES) {
    if (!(await isFile(path.join(repoRoot, file)))) fail(`missing ${path.join(repoRoot, file)}`, build);
  }

  const rustHost = await probeRustHost(path.join(repoRoot, `${RUST_HOST_DIR}/harness-host${exe}`));
  const verifiedNode = options.skipNode ? null : await resolveNodeRuntime();

  await rm(options.out, { recursive: true, force: true });
  await mkdir(options.out, { recursive: true });
  const stager = new DependencyStager(options.out);
  for (const source of SOURCES) {
    const from = path.join(repoRoot, source.dir);
    const specifiers = new Map();
    for (const file of await listFiles(from)) {
      const target = path.join(options.out, path.relative(repoRoot, file));
      await mkdir(path.dirname(target), { recursive: true });
      await cp(file, target);
      if (source.scan !== false && /\.m?js$/.test(file)) {
        collectSpecifiers(await readFile(file, "utf8"), specifiers);
      }
    }
    // ESM .js needs the nearest package.json to declare "type": "module".
    const pkg = await readJson(path.join(repoRoot, source.pkg, "package.json"));
    await writeFile(
      path.join(options.out, source.pkg, "package.json"),
      `${JSON.stringify({ name: pkg.name, private: true, type: pkg.type ?? "commonjs" }, null, 2)}\n`,
    );
    const packageReal = await realpath(path.join(repoRoot, source.pkg));
    const stagedPackage = path.join(options.out, source.pkg);
    for (const [name, optional] of specifiers) {
      await stager.stage(name, packageReal, stagedPackage, optional);
    }
  }
  for (const { file } of FILES) {
    const target = path.join(options.out, file);
    await mkdir(path.dirname(target), { recursive: true });
    await cp(path.join(repoRoot, file), target);
  }
  const tools = await stageTools(options.out);
  const conpty = await stageConpty(options.out);
  const nodeRuntime = verifiedNode === null ? null : await stageNodeRuntime(options.out, verifiedNode);

  const manifest = {
    schemaVersion: 1,
    nodeEngine: ">=24",
    platform: `${process.platform}-${process.arch}`,
    sources: [...SOURCES.map((source) => source.dir), ...FILES.map((entry) => entry.file)],
    rustHost,
    tools,
    conpty,
    nodeRuntime,
    packages: stager.packages.sort((a, b) => a.path.localeCompare(b.path)),
    unresolvedOptional: stager.unresolved,
  };
  await writeFile(path.join(options.out, MANIFEST), `${JSON.stringify(manifest, null, 2)}\n`);
  const files = await listFiles(options.out);
  let bytes = 0;
  for (const file of files) bytes += (await stat(file)).size;
  process.stdout.write(
    `staged ${files.length} files (${(bytes / 1048576).toFixed(1)} MiB), ` +
      `${stager.packages.length} packages, ${stager.unresolved.length} optional unresolved -> ${options.out}\n`,
  );
  for (const missing of stager.unresolved) {
    process.stdout.write(`  optional, not staged: ${missing.name} (from ${missing.from})\n`);
  }
}

await main();
