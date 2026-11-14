// Fixtures for the Phase 7 performance gate:
// - the isolated-home seed both apps start from (settings, empty credentials, Personal Provider on
//   the local mock, one small git workspace restored as the last session);
// - large deterministic service-layer fixtures (100 MiB file, source tree, git repo), generated once
//   and cached under Worse/.work/cache/phase7/fixtures.
import { execFile } from "node:child_process";
import { createWriteStream, existsSync } from "node:fs";
import { mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const moduleDir = path.dirname(fileURLToPath(import.meta.url));
export const repoRoot = path.resolve(moduleDir, "../../../..");
export const worseRoot = path.join(repoRoot, "Worse");
export const cacheRoot = path.join(worseRoot, ".work/cache/phase7");
const requireFromRepo = createRequire(path.join(repoRoot, "package.json"));

export const PROVIDER_ID = "phase7-perf-local";
export const MODEL_ID = "phase7-model";
// 本地 mock 不校验 key；该值只为满足 ApiKey Access schema，不是凭据。
const FIXTURE_API_KEY = "phase7-synthetic-no-remote-access";

function run(command, args, cwd, input) {
  return new Promise((resolve, reject) => {
    const child = execFile(command, args, { cwd, windowsHide: true, encoding: "utf8", maxBuffer: 64 * 1024 * 1024 }, (error, stdout, stderr) => {
      if (error) reject(new Error(`${command} ${args.join(" ")} failed: ${stderr || error.message}`));
      else resolve(stdout.trim());
    });
    if (input !== undefined) child.stdin.end(input);
  });
}

export const git = (cwd, ...args) => run("git", args, cwd);

async function initRepo(dir) {
  await git(dir, "init", "-q", "-b", "main");
  await git(dir, "config", "user.name", "Phase7 Perf");
  await git(dir, "config", "user.email", "phase7-perf@example.invalid");
  await git(dir, "config", "commit.gpgsign", "false");
  await git(dir, "config", "core.autocrlf", "false");
}

let seedKit;
async function loadSeedKit(runDir) {
  if (seedKit) return seedKit;
  const { build } = await import(pathToFileURL(requireFromRepo.resolve("esbuild")).href);
  const outfile = path.join(runDir, "seedKit.mjs");
  await build({
    entryPoints: [path.join(moduleDir, "seedKit.entry.ts")],
    outfile,
    bundle: true,
    platform: "node",
    format: "esm",
    target: "node24",
    logLevel: "warning",
    banner: { js: 'import { createRequire as __gateCreateRequire } from "node:module";\nconst require = __gateCreateRequire(import.meta.url);' },
  });
  seedKit = await import(pathToFileURL(outfile).href);
  return seedKit;
}

/** Small workspace restored at startup (same for both apps). */
export async function createWorkspace(home) {
  const workspace = path.join(home, "Phase7PerfProject");
  if (existsSync(path.join(workspace, ".git"))) return workspace;
  await mkdir(path.join(workspace, "src"), { recursive: true });
  await writeFile(path.join(workspace, "README.md"), "# Phase7 perf fixture\n");
  for (let index = 0; index < 20; index += 1) {
    await writeFile(path.join(workspace, "src", `module${index}.ts`), `export const value${index} = ${index};\n`);
  }
  await initRepo(workspace);
  await git(workspace, "add", "-A");
  await git(workspace, "commit", "-q", "-m", "phase7 perf: initial fixture");
  return workspace;
}

/**
 * Seeds `<home>/.zcode/v2` the way ZCode's desktop e2e does: provider family chosen, the workspace
 * as last session, empty credentials (no account), a Personal Provider + default model on the mock.
 * closeToTrayOnWindows=false so a window close quits ZCode (graceful exit, no tray leftover).
 */
export async function seedHome({ home, runDir, providerBaseUrl, workspace }) {
  const v2 = path.join(home, ".zcode", "v2");
  await mkdir(v2, { recursive: true });
  const providerConfig = path.join(v2, "provider_config.json");
  const kit = await loadSeedKit(runDir);
  await kit.seedPersonalProviderConfig(providerConfig, {
    id: PROVIDER_ID,
    label: "Phase7 Perf Local",
    apiFormat: "anthropic-messages",
    baseURL: providerBaseUrl,
    apiKey: FIXTURE_API_KEY,
    enabled: true,
    models: [{ id: MODEL_ID, contextWindow: 200_000, maxOutputTokens: 4096 }],
  });
  await kit.seedPersonalDefaultModelSelection(providerConfig, { providerId: PROVIDER_ID, modelId: MODEL_ID });
  await writeFile(
    path.join(v2, "setting.json"),
    JSON.stringify({
      providerFamilyDomain: "zai",
      providerFamilyDomainMigrated: true,
      providerFamilyDomainUpdatedAt: Date.now(),
      providerFamilyConnectionSelections: {},
      locale: "en-US",
      localePreference: "en-US",
      closeToTrayOnWindows: false,
      recentProjects: [workspace],
      lastWorkspaceSession: [{ kind: "local", workspacePath: workspace, workspacePurpose: "project" }],
      lastActiveTabIndex: 0,
    }),
  );
  await writeFile(path.join(v2, "credentials.json"), "{}");
}

/** The provider base URL changes per run (random port); rewrite it in the seeded config. */
export async function repointProvider(home, providerBaseUrl, runDir) {
  const providerConfig = path.join(home, ".zcode", "v2", "provider_config.json");
  const kit = await loadSeedKit(runDir);
  await kit.seedPersonalProviderConfig(providerConfig, {
    id: PROVIDER_ID,
    label: "Phase7 Perf Local",
    apiFormat: "anthropic-messages",
    baseURL: providerBaseUrl,
    apiKey: FIXTURE_API_KEY,
    enabled: true,
    models: [{ id: MODEL_ID, contextWindow: 200_000, maxOutputTokens: 4096 }],
  });
}

// ---------------------------------------------------------------------------------------------
// Large service-layer fixtures (deterministic content).

const WORDS = ["alpha", "beta", "gamma", "delta", "render", "session", "workspace", "provider", "stream", "buffer", "socket", "harness", "window", "command", "search", "index"];

function lineFor(seed) {
  let text = `${String(seed).padStart(8, "0")}:`;
  let state = seed * 2654435761;
  for (let index = 0; index < 10; index += 1) {
    state = (state * 1103515245 + 12345) >>> 0;
    text += ` ${WORDS[state % WORDS.length]}`;
  }
  return `${text}\n`;
}

async function writeBigFile(file, bytes) {
  await mkdir(path.dirname(file), { recursive: true });
  const stream = createWriteStream(file);
  let written = 0;
  let seed = 0;
  while (written < bytes) {
    let chunk = "";
    while (chunk.length < 1 << 20 && written + chunk.length < bytes) chunk += lineFor(seed++);
    if (written + chunk.length > bytes) chunk = chunk.slice(0, bytes - written);
    written += chunk.length;
    if (!stream.write(chunk)) await new Promise((resolve) => stream.once("drain", resolve));
  }
  await new Promise((resolve, reject) => stream.end((error) => (error ? reject(error) : resolve())));
}

const EXTENSIONS = [".ts", ".tsx", ".js", ".json", ".md", ".css", ".rs", ".py"];

/** `count` files spread over nested directories (~40 files per leaf directory). */
async function writeTree(root, count, prefix) {
  const perDir = 40;
  for (let index = 0; index < count; index += perDir) {
    const dirIndex = Math.floor(index / perDir);
    const dir = path.join(root, `pkg${dirIndex % 50}`, `mod${Math.floor(dirIndex / 50) % 40}`, `d${dirIndex}`);
    await mkdir(dir, { recursive: true });
    const writes = [];
    for (let offset = 0; offset < perDir && index + offset < count; offset += 1) {
      const n = index + offset;
      const name = `${prefix}${WORDS[n % WORDS.length]}_${n}${EXTENSIONS[n % EXTENSIONS.length]}`;
      writes.push(writeFile(path.join(dir, name), `// ${name}\n${lineFor(n)}${lineFor(n + 1)}`));
    }
    await Promise.all(writes);
  }
}

/**
 * Generates (or reuses) the cached fixtures. Scale is part of the cache key so a different scale
 * never reuses a mismatched tree.
 */
export async function ensureServiceFixtures({ bigFileBytes, treeFiles, repoFiles, repoModified, repoUntracked }) {
  const root = path.join(cacheRoot, "fixtures");
  const manifestFile = path.join(root, "manifest.json");
  const scale = { bigFileBytes, treeFiles, repoFiles, repoModified, repoUntracked, version: 1 };
  const manifest = existsSync(manifestFile) ? JSON.parse(await readFile(manifestFile, "utf8")) : null;
  const paths = { root, bigFile: path.join(root, "big-100mb.txt"), tree: path.join(root, "tree"), repo: path.join(root, "repo") };
  if (manifest && JSON.stringify(manifest.scale) === JSON.stringify(scale)) {
    // 仓库里的“修改/未跟踪”状态由生成脚本设定；若被其它运行改动则重新生成。
    const status = await git(paths.repo, "status", "--porcelain", "-uall").catch(() => "");
    const lines = status ? status.split("\n").length : 0;
    if (lines === repoModified + repoUntracked) return { ...paths, scale, generated: false };
  }
  const started = Date.now();
  await rm(root, { recursive: true, force: true });
  await mkdir(root, { recursive: true });
  await writeBigFile(paths.bigFile, bigFileBytes);
  await writeTree(paths.tree, treeFiles, "t_");
  await writeTree(paths.repo, repoFiles, "r_");
  await initRepo(paths.repo);
  await git(paths.repo, "add", "-A");
  await git(paths.repo, "commit", "-q", "-m", "phase7 perf: repo fixture");
  const tracked = (await git(paths.repo, "ls-files")).split("\n").filter(Boolean);
  for (let index = 0; index < repoModified; index += 1) {
    const file = path.join(paths.repo, tracked[(index * 37) % tracked.length]);
    await writeFile(file, `// modified ${index}\n${lineFor(index + 7)}`);
  }
  await mkdir(path.join(paths.repo, "untracked"), { recursive: true });
  for (let index = 0; index < repoUntracked; index += 1) {
    await writeFile(path.join(paths.repo, "untracked", `new_${index}.ts`), lineFor(index + 99));
  }
  await writeFile(manifestFile, JSON.stringify({ scale, generatedAt: new Date().toISOString(), seconds: Math.round((Date.now() - started) / 1000) }, null, 2));
  return { ...paths, scale, generated: true, seconds: Math.round((Date.now() - started) / 1000) };
}
