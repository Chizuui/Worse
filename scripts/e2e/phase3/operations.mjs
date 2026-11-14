// The routed RPC operations exercised by the differential gate. Each entry is one
// `channel.method` call with its exact TS service params (contract section 6).
import path from "node:path";

/** `op` names group results in the perf table; `id` must be unique. */
export function buildOperations(fx) {
  const ws = fx.ws;
  const w = (relative) => path.join(ws, ...relative.split("/"));
  const ops = [];
  const add = (service, channel, method, label, params) =>
    ops.push({ id: `${channel}.${method}:${label}`, op: `${channel}.${method}`, service, channel, method, params });

  // fs
  add("fs", "file", "readdir", "root-hidden", { path: ws, includeHidden: true });
  add("fs", "file", "readdir", "root-visible", { path: ws });
  add("fs", "file", "readdir", "nested", { path: w("nested/deep/x/y"), includeHidden: false });
  add("fs", "file", "readdir", "empty", { path: w("empty-dir") });
  add("fs", "file", "readdir", "missing", { path: w("missing-dir") });
  add("fs", "file", "readdir", "not-a-dir", { path: w("a.txt") });
  add("fs", "file", "stat", "file", { path: w("a.txt") });
  add("fs", "file", "stat", "dir", { path: w("src") });
  add("fs", "file", "stat", "unicode", { path: w("中文 文件.md") });
  add("fs", "file", "stat", "missing", { path: w("missing.txt") });
  if (fx.links.dirLink) add("fs", "file", "stat", "dir-link", { path: w("link-to-src") });
  if (fx.links.fileLink) add("fs", "file", "stat", "file-link", { path: w("a-link.txt") });
  if (fx.links.brokenLink) add("fs", "file", "stat", "broken-link", { path: w("broken-link.txt") });
  add("fs", "file", "checkFilesExist", "mixed", {
    paths: [w("a.txt"), w("missing.txt"), w("src"), w("emoji 😀.txt"), w("with space.txt")],
  });
  add("fs", "file", "checkFilesExist", "too-many", { paths: Array.from({ length: 16 }, (_, i) => w(`${i}.txt`)) });
  add("fs", "file", "resolvePath", "root", { path: ws });
  add("fs", "file", "resolvePath", "nested", { path: w("nested/deep") });
  if (fx.links.dirLink) add("fs", "file", "resolvePath", "dir-link", { path: w("link-to-src") });
  add("fs", "file", "resolvePath", "missing", { path: w("missing") });
  add("fs", "file", "readTextFile", "small", { path: w("a.txt") });
  add("fs", "file", "readTextFile", "unicode", { path: w("中文 文件.md") });
  add("fs", "file", "readTextFile", "large-default", { path: w("big/large.txt") });
  add("fs", "file", "readTextFile", "large-offset", { path: w("big/large.txt"), offset: 1_000_003, length: 9_999 });
  add("fs", "file", "readTextFile", "large-clamped", { path: w("big/large.txt"), length: 10_000_000 });
  add("fs", "file", "readTextFile", "beyond-eof", { path: w("a.txt"), offset: 1_000 });
  add("fs", "file", "readTextFile", "binary", { path: w("bin/blob.bin") });
  add("fs", "file", "readTextFile", "invalid-utf8", { path: w("bin/invalid-utf8.txt") });
  add("fs", "file", "readTextFile", "crlf", { path: w("crlf.txt") });
  add("fs", "file", "readTextFile", "directory", { path: w("src") });
  add("fs", "file", "readTextFile", "missing", { path: w("missing.txt") });
  add("fs", "file", "readFileRange", "middle", { path: w("big/large.txt"), offset: 4_096, length: 65_536 });
  add("fs", "file", "readFileRange", "beyond-eof", { path: w("a.txt"), offset: 10_000, length: 10 });
  add("fs", "file", "readFileRange", "binary", { path: w("bin/blob.bin"), offset: 0, length: 100 });
  add("fs", "file", "readMediaPreview", "png", { path: w("img/pic.png") });
  add("fs", "file", "readMediaPreview", "jpg", { path: w("img/photo.jpg") });
  add("fs", "file", "readMediaPreview", "too-large", { path: w("img/pic.png"), maxBytes: 10 });
  add("fs", "file", "readBinaryPreview", "blob", { path: w("bin/blob.bin") });
  add("fs", "file", "readBinaryPreview", "directory", { path: w("src") });

  // search (file index)
  for (const root of [ws, fx.repo]) {
    const tag = root === ws ? "ws" : "repo";
    add("search", "file", "listWorkspaceFilesLength", tag, { rootPath: root });
    add("search", "file", "listWorkspaceFilesRange", `${tag}-all`, { rootPath: root, offset: 0, length: 4_000_000 });
    add("search", "file", "listWorkspaceFilesRange", `${tag}-slice`, { rootPath: root, offset: 7, length: 40 });
    for (const query of ["", "a", "index", "TXT", "中文", "deep z", "srcbtn", "zzzz-no-match"]) {
      add("search", "file", "searchWorkspaceFiles", `${tag}-q:${query}`, { rootPath: root, query });
    }
    add("search", "file", "searchWorkspaceFiles", `${tag}-limit3`, { rootPath: root, query: "t", limit: 3 });
    add("search", "file", "searchWorkspaceFiles", `${tag}-refresh`, { rootPath: root, query: "ts", refresh: true });
    add("search", "file", "searchWorkspaceFiles", `${tag}-identity`, {
      rootPath: root,
      workspaceIdentity: ` identity-${tag} `,
      query: "md",
    });
  }
  // perf corpus (10k files): cold scans via refresh, then cached queries
  add("search", "file", "searchWorkspaceFiles", "bulk-refresh", { rootPath: fx.bulk, query: "module-5", refresh: true });
  add("search", "file", "searchWorkspaceFiles", "bulk-cached", { rootPath: fx.bulk, query: "pkg 9 mod 42" });
  add("search", "file", "listWorkspaceFilesLength", "bulk", { rootPath: fx.bulk });
  add("search", "file", "listWorkspaceFilesRange", "bulk-all", { rootPath: fx.bulk, offset: 0, length: 4_000_000 });
  add("search", "file", "searchWorkspaceFiles", "bad-limit", { rootPath: ws, query: "a", limit: null });

  // git (read-only)
  for (const [tag, workspacePath] of [
    ["repo", fx.repo],
    ["sub", fx.repoSub],
    ["nonrepo", ws],
  ]) {
    add("git", "git", "getRepositorySummary", tag, { workspacePath });
    add("git", "git", "getWorkspaceRepositoryInfo", tag, { workspacePath });
    add("git", "git", "getLocalBranches", tag, { workspacePath });
    add("git", "git", "getCommitGraph", tag, { workspacePath, maxCount: 10 });
    add("git", "git", "getChanges", `${tag}-unstaged`, { workspacePath, sourceId: "unstaged" });
    add("git", "git", "getChanges", `${tag}-staged`, { workspacePath, sourceId: "staged" });
    add("git", "git", "getBranchComparison", tag, { workspacePath });
    add("git", "git", "getIdentity", tag, { workspacePath });
    add("git", "git", "refresh", tag, { workspacePath, includeIdentity: true, includeBranchComparison: true });
  }
  add("git", "git", "getCommitGraph", "skip", { workspacePath: fx.repo, maxCount: 1, skip: 1 });
  add("git", "git", "getIgnoredPaths", "repo", {
    workspacePath: fx.repo,
    paths: ["debug.log", "build/out.js", "src/keep.ts", "missing.txt"],
  });
  for (const [label, query] of [
    ["unstaged", { path: "src/modify-unstaged.ts", sourceId: "unstaged" }],
    ["staged", { path: "src/modify-staged.ts", sourceId: "staged" }],
    ["staged-flag", { path: "src/modify-staged.ts", staged: true }],
    ["renamed", { path: "src/renamed.ts", sourceId: "staged" }],
    ["deleted", { path: "src/delete-me.ts", sourceId: "staged" }],
    ["added", { path: "src/added-staged.ts", sourceId: "staged" }],
    ["untracked", { path: "src/untracked.ts", sourceId: "unstaged" }],
    ["binary", { path: "assets/logo.bin", sourceId: "unstaged" }],
    ["branch", { path: "src/keep.ts", sourceId: "branch" }],
  ]) {
    add("git", "git", "getDiff", label, { workspacePath: fx.repo, ...query });
  }
  return ops;
}
