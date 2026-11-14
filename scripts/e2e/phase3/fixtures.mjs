// Fixture corpus for the Phase 3a differential gate (spec A1/A2).
// Layout under Worse/.work/tmp/phase3-fixtures:
//   ws/    plain (non-git) workspace: unicode, spaces, hidden, ignored, binary, large, symlinks
//   repo/  git repository with staged / unstaged / untracked / renamed / deleted changes
// GIT_CEILING_DIRECTORIES=<root> keeps ws/ from being discovered inside the ZCode checkout.
import { spawnSync } from "node:child_process";
import { mkdir, rm, symlink, writeFile } from "node:fs/promises";
import path from "node:path";

const PNG_1X1 = Buffer.from(
  "89504e470d0a1a0a0000000d4948445200000001000000010806000000" +
    "1f15c4890000000d49444154789c6360000002000154a24f5d0000000049454e44ae426082",
  "hex",
);

async function put(file, content) {
  await mkdir(path.dirname(file), { recursive: true });
  await writeFile(file, content);
}

async function trySymlink(target, link, type) {
  try {
    await symlink(target, link, type);
    return true;
  } catch {
    return false;
  }
}

function git(cwd, args, env) {
  const result = spawnSync("git", args, { cwd, env, encoding: "utf8" });
  if (result.status !== 0) throw new Error(`git ${args.join(" ")} failed: ${result.stderr}`);
  return result.stdout;
}

function largeText() {
  const lines = [];
  for (let index = 0; index < 120_000; index += 1) lines.push(`line ${index} needle-${index % 97} lorem ipsum dolor sit amet`);
  return `${lines.join("\n")}\n`;
}

async function buildWorkspace(ws) {
  const files = {
    "a.txt": "alpha\nneedle here\n",
    "with space.txt": "space file needle\n",
    "中文 文件.md": "# 标题\nneedle 中文\n",
    "emoji 😀.txt": "smile 😀 needle\n",
    "Upper.TXT": "UPPER NEEDLE\n",
    "Apple.txt": "a",
    "apple.md": "b",
    "Banana.txt": "c",
    "_under.txt": "d",
    "b-dash.txt": "e",
    "10.txt": "ten",
    "9.txt": "nine",
    "crlf.txt": "one\r\ntwo needle\r\n",
    "src/index.ts": "export const needle = 1;\n",
    "src/util.js": "module.exports = {};\n",
    "src/components/Button.tsx": "export function Button() { return null; }\n",
    "nested/deep/x/y/z.ts": "// deep needle\n",
    ".hidden/secret.txt": "hidden needle\n",
    ".github/workflows/ci.yml": "name: ci\n",
    "node_modules/pkg/index.js": "needle in node_modules\n",
    "dist/out.js": "ignored by gitignore\n",
    "debug.log": "ignored log needle\n",
    ".env": "SECRET=1\n",
    ".env.local": "SECRET=2\n",
    "lib/native.dll": "MZ",
    "coverage.out": "x",
    "docs/readme.md": "docs needle\n",
    ".gitignore": "dist/\n*.log\n",
  };
  for (const [relative, content] of Object.entries(files)) await put(path.join(ws, relative), content);
  await put(path.join(ws, "big/large.txt"), largeText());
  await put(path.join(ws, "bin/blob.bin"), Buffer.from([0, 1, 2, 3, 0, 255, 254, 10, 13, 0]));
  await put(path.join(ws, "bin/invalid-utf8.txt"), Buffer.from([0x61, 0xff, 0xfe, 0x62, 0x0a, 0xc3, 0x28, 0x0a]));
  await put(path.join(ws, "img/pic.png"), PNG_1X1);
  await put(path.join(ws, "img/photo.jpg"), Buffer.from([0xff, 0xd8, 0xff, 0xd9]));
  await mkdir(path.join(ws, "empty-dir"), { recursive: true });
  const dirLink = await trySymlink(path.join(ws, "src"), path.join(ws, "link-to-src"), "junction");
  const fileLink = await trySymlink(path.join(ws, "a.txt"), path.join(ws, "a-link.txt"), "file");
  const brokenLink = await trySymlink(path.join(ws, "nope.txt"), path.join(ws, "broken-link.txt"), "file");
  return { dirLink, fileLink, brokenLink };
}

async function buildRepo(repo, env) {
  await mkdir(repo, { recursive: true });
  git(repo, ["init", "-q", "-b", "main"], env);
  git(repo, ["config", "user.name", "Phase Three"], env);
  git(repo, ["config", "user.email", "phase3@example.invalid"], env);
  git(repo, ["config", "core.autocrlf", "false"], env);
  const committed = {
    "README.md": "# repo\n",
    "src/keep.ts": "export const keep = 1;\n",
    "src/modify-staged.ts": "export const a = 1;\n",
    "src/modify-unstaged.ts": "export const b = 1;\n",
    "src/rename-me.ts": "export const renamed = 'content that is long enough to be detected as a rename';\n",
    "src/delete-me.ts": "export const gone = 1;\n",
    "packages/sub/index.ts": "export const sub = 1;\n",
    "packages/sub/change.ts": "export const subChange = 1;\n",
    "assets/logo.bin": Buffer.from([0, 1, 2, 3]),
    ".gitignore": "*.log\nbuild/\n",
  };
  for (const [relative, content] of Object.entries(committed)) await put(path.join(repo, relative), content);
  git(repo, ["add", "-A"], env);
  git(repo, ["commit", "-q", "-m", "initial commit"], env);
  git(repo, ["branch", "feature"], env);
  await put(path.join(repo, "src/keep.ts"), "export const keep = 2;\n");
  git(repo, ["commit", "-q", "-am", "second commit"], env);

  await put(path.join(repo, "src/modify-staged.ts"), "export const a = 2;\nexport const a2 = 3;\n");
  git(repo, ["add", "src/modify-staged.ts"], env);
  await put(path.join(repo, "src/modify-unstaged.ts"), "export const b = 2;\n");
  git(repo, ["mv", "src/rename-me.ts", "src/renamed.ts"], env);
  git(repo, ["rm", "-q", "src/delete-me.ts"], env);
  await put(path.join(repo, "src/added-staged.ts"), "export const added = 1;\n");
  git(repo, ["add", "src/added-staged.ts"], env);
  await put(path.join(repo, "src/untracked.ts"), "export const untracked = 1;\n");
  await put(path.join(repo, "untracked dir/note 中文.md"), "note\n");
  await put(path.join(repo, "packages/sub/change.ts"), "export const subChange = 2;\n");
  await put(path.join(repo, "packages/sub/new.ts"), "export const subNew = 1;\n");
  await put(path.join(repo, "debug.log"), "ignored\n");
  await put(path.join(repo, "build/out.js"), "ignored\n");
  await put(path.join(repo, "assets/logo.bin"), Buffer.from([0, 1, 2, 3, 4]));
}

/** Perf corpus: BULK_DIRS x BULK_FILES small source files, so index/scan costs dominate RPC overhead. */
const BULK_DIRS = 100;
const BULK_FILES = 100;

async function buildBulk(bulk) {
  for (let dir = 0; dir < BULK_DIRS; dir += 1) {
    const folder = path.join(bulk, `pkg-${dir}`, "src");
    await mkdir(folder, { recursive: true });
    const writes = [];
    for (let file = 0; file < BULK_FILES; file += 1) {
      const marker = file % 10 === 0 ? "// bulkneedle marker\n" : "";
      const body = `// module ${dir}/${file}\nexport const value${file} = ${file};\n${marker}`;
      writes.push(writeFile(path.join(folder, `module-${file}.ts`), body));
    }
    await Promise.all(writes);
  }
}

/** Recreates the corpus from scratch and returns its paths. */
export async function buildFixtures(root, env) {
  await rm(root, { recursive: true, force: true });
  const ws = path.join(root, "ws");
  const repo = path.join(root, "repo");
  const bulk = path.join(root, "bulk");
  const atomic = path.join(root, "atomic");
  await mkdir(atomic, { recursive: true });
  const links = await buildWorkspace(ws);
  await buildRepo(repo, env);
  await buildBulk(bulk);
  return { root, ws, repo, bulk, repoSub: path.join(repo, "packages", "sub"), atomic, links };
}
