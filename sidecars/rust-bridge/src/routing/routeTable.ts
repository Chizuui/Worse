/**
 * A Rust service group; each one has its own feature flag. `watch` and `pty` are
 * session services (Phase 3b): they are served by `sessions/*`, not by this stateless table.
 * `store` (Phase 3c) is the read-only task-index path (`project.readTree`, ...), opt-in.
 */
export type RustService = "fs" | "search" | "git" | "watch" | "pty" | "store";

export const RUST_SERVICES: readonly RustService[] = ["fs", "search", "git", "watch", "pty", "store"];

/**
 * How a Rust JSON result becomes the RPC result the TS service would return.
 * `jsonText`: Rust returns `{json: "<text>"}` whose key order is the TS object order.
 */
export type ResultShape = "json" | "base64Bytes" | "jsonText";

export interface Route {
  service: Exclude<RustService, "watch" | "pty">;
  /** JSON-RPC method name on `harness-host` (contract section 6). */
  rustMethod: string;
  timeoutMs: number;
  result: ResultShape;
  /** The TS method may be called with no argument (treated as `{}`), e.g. `project.readTree()`. */
  allowNoArgs?: boolean;
}

const FS_TIMEOUT_MS = 10_000;
const SEARCH_TIMEOUT_MS = 60_000;
const GIT_TIMEOUT_MS = 30_000;
/** Covers the 30 s primary-root stat budget of `project.readTree`. */
const STORE_TIMEOUT_MS = 35_000;

function fs(method: string, result: ResultShape = "json"): Route {
  return { service: "fs", rustMethod: `file.${method}`, timeoutMs: FS_TIMEOUT_MS, result };
}

function search(method: string): Route {
  return { service: "search", rustMethod: `file.${method}`, timeoutMs: SEARCH_TIMEOUT_MS, result: "json" };
}

function git(method: string): Route {
  return { service: "git", rustMethod: `git.${method}`, timeoutMs: GIT_TIMEOUT_MS, result: "json" };
}

function store(method: string): Route {
  return { service: "store", rustMethod: `store.${method}`, timeoutMs: STORE_TIMEOUT_MS, result: "jsonText", allowNoArgs: true };
}

/**
 * RPC `channel -> method -> route`. Only read operations are listed: every
 * mutation stays on the TS Host so there is never a second writer (spec R4).
 */
const ROUTES: ReadonlyMap<string, ReadonlyMap<string, Route>> = new Map([
  [
    "file",
    new Map([
      ["readdir", fs("readdir")],
      ["stat", fs("stat")],
      ["checkFilesExist", fs("checkFilesExist")],
      ["resolvePath", fs("resolvePath")],
      ["readTextFile", fs("readTextFile")],
      ["readFileRange", fs("readFileRange", "base64Bytes")],
      ["readMediaPreview", fs("readMediaPreview")],
      ["readBinaryPreview", fs("readBinaryPreview")],
      ["searchWorkspaceFiles", search("searchWorkspaceFiles")],
      ["listWorkspaceFilesLength", search("listWorkspaceFilesLength")],
      ["listWorkspaceFilesRange", search("listWorkspaceFilesRange")]
    ])
  ],
  [
    "git",
    new Map([
      ["getRepositorySummary", git("getRepositorySummary")],
      ["getWorkspaceRepositoryInfo", git("getWorkspaceRepositoryInfo")],
      ["getLocalBranches", git("getLocalBranches")],
      ["getCommitGraph", git("getCommitGraph")],
      ["getChanges", git("getChanges")],
      ["getIgnoredPaths", git("getIgnoredPaths")],
      ["getDiff", git("getDiff")],
      ["getBranchComparison", git("getBranchComparison")],
      ["getIdentity", git("getIdentity")],
      ["refresh", git("refresh")]
    ])
  ],
  [
    "project",
    new Map([
      ["readTree", store("readTree")],
      ["readProjects", store("readProjects")]
    ])
  ],
  ["task-groups", new Map([["read", store("readGroups")]])]
]);

export function findRoute(channel: string, method: string): Route | undefined {
  return ROUTES.get(channel)?.get(method);
}

export function listRoutes(): Array<{ channel: string; method: string; route: Route }> {
  const out: Array<{ channel: string; method: string; route: Route }> = [];
  for (const [channel, methods] of ROUTES) {
    for (const [method, route] of methods) out.push({ channel, method, route });
  }
  return out;
}
