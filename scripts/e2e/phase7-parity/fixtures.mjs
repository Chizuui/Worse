// Isolated-home fixtures for the Phase 7 parity gate: ZCode data seed (settings, empty credentials,
// Personal Provider pointing at the local mock) and a small git workspace.
import { execFile } from "node:child_process";
import { createRequire } from "node:module";
import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { isolatedHomeDir } from "../lib/isolatedHome.mjs";
import { repoRoot } from "../phase2/environment.mjs";

export const PROVIDER_ID = "phase7-local";
export const PROVIDER_LABEL = "Phase7 Local Fixture";
export const MODEL_ID = "phase7-model";
// 本地 mock 不校验 key；该值只为满足 ApiKey Access schema，不是凭据。
const FIXTURE_API_KEY = "phase7-synthetic-no-remote-access";

const moduleDir = path.dirname(fileURLToPath(import.meta.url));
const requireFromRepo = createRequire(path.join(repoRoot, "package.json"));

function run(command, args, cwd) {
  return new Promise((resolve, reject) => {
    execFile(command, args, { cwd, windowsHide: true, encoding: "utf8" }, (error, stdout, stderr) => {
      if (error) reject(new Error(`${command} ${args.join(" ")} failed: ${stderr || error.message}`));
      else resolve(stdout.trim());
    });
  });
}

export function git(cwd, ...args) {
  return run("git", args, cwd);
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

export function dataPaths(home = isolatedHomeDir()) {
  const v2 = path.join(home, ".zcode", "v2");
  return {
    home,
    v2,
    settings: path.join(v2, "setting.json"),
    credentials: path.join(v2, "credentials.json"),
    providerConfig: path.join(v2, "provider_config.json"),
    tasksIndex: path.join(v2, "tasks-index.sqlite"),
    cliDb: path.join(home, ".zcode", "cli", "db", "db.sqlite"),
  };
}

/** Workspace with two commits, a nested file for the file tree, and an untracked change for git status. */
export async function createWorkspace(home) {
  const workspace = path.join(home, "Phase7Project");
  await mkdir(path.join(workspace, "src"), { recursive: true });
  await writeFile(path.join(workspace, "README.md"), "# Phase7 parity fixture\n\nPHASE7_README_MARKER\n");
  await writeFile(path.join(workspace, "src", "hello.txt"), "PHASE7_HELLO_FILE_CONTENT\n");
  await git(workspace, "init", "-q", "-b", "main");
  await git(workspace, "config", "user.name", "Phase7 Gate");
  await git(workspace, "config", "user.email", "phase7-gate@example.invalid");
  await git(workspace, "config", "commit.gpgsign", "false");
  await git(workspace, "add", "-A");
  await git(workspace, "commit", "-q", "-m", "phase7: initial fixture");
  return workspace;
}

/**
 * Seeds the isolated home the way ZCode's desktop e2e seeds its sandbox before launch
 * (session-pane-retention-fixture.ts): provider family chosen, the workspace as the last session,
 * empty credentials (no account), and a Personal Provider + default model on the local mock.
 */
export async function seedZcodeData({ runDir, providerBaseUrl, workspace, home = isolatedHomeDir() }) {
  const paths = dataPaths(home);
  await mkdir(paths.v2, { recursive: true });
  const kit = await loadSeedKit(runDir);
  await kit.seedPersonalProviderConfig(paths.providerConfig, {
    id: PROVIDER_ID,
    label: PROVIDER_LABEL,
    apiFormat: "anthropic-messages",
    baseURL: providerBaseUrl,
    apiKey: FIXTURE_API_KEY,
    enabled: true,
    models: [{ id: MODEL_ID, contextWindow: 200_000, maxOutputTokens: 4096 }],
  });
  await kit.seedPersonalDefaultModelSelection(paths.providerConfig, { providerId: PROVIDER_ID, modelId: MODEL_ID });
  await writeFile(
    paths.settings,
    JSON.stringify({
      providerFamilyDomain: "zai",
      providerFamilyDomainMigrated: true,
      providerFamilyDomainUpdatedAt: Date.now(),
      providerFamilyConnectionSelections: {},
      locale: "en-US",
      localePreference: "en-US",
      recentProjects: [workspace],
      lastWorkspaceSession: [{ kind: "local", workspacePath: workspace, workspacePurpose: "project" }],
      lastActiveTabIndex: 0,
      // ZCode 在 Windows 默认关闭即隐藏到托盘；gate 需要关窗即退出（Harness 无托盘），两边退出语义才一致。
      closeToTrayOnWindows: false,
    }),
  );
  await writeFile(paths.credentials, "{}");
  return paths;
}
