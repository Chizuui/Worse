// Service-layer measurements (100 MiB file open, search, git status, watcher) through a real Host
// and the renderer's RPC surface, using the shared Phase 3 harness (Node sidecar launcher).
// ZCode side: HARNESS_RUST_SERVICES=none (ZCode's TypeScript services). Harness side: the shipped
// default routing (search,git,pty -> the installed harness-host.exe).
import { mkdir, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { loadModules, paths, verifyArtifacts } from "../phase2/environment.mjs";
import { openHostSession } from "../phase3/hostSession.mjs";
import { killTrees, processTree, stillAlive } from "./procs.mjs";

const now = () => performance.now();

export function stats(samples) {
  const values = samples.filter((value) => typeof value === "number" && Number.isFinite(value)).sort((a, b) => a - b);
  if (values.length === 0) return { n: 0, p50: null, p95: null, median: null, min: null, max: null };
  const rank = (p) => values[Math.min(values.length - 1, Math.max(0, Math.ceil((p / 100) * values.length) - 1))];
  const round = (value) => Math.round(value * 10) / 10;
  return { n: values.length, p50: round(rank(50)), p95: round(rank(95)), median: round(rank(50)), min: round(values[0]), max: round(values.at(-1)) };
}

async function timed(session, channel, method, params) {
  const started = now();
  const result = await session.call(channel, method, params);
  const ms = now() - started;
  if (!result.ok) throw new Error(`${channel}.${method} failed: ${result.error?.message ?? JSON.stringify(result.error)}`);
  return { ms, value: result.value };
}

const SEARCH_QUERIES = ["render", "session_1", "pkg7 mod3", "workspace.ts", "harness", "index_99", "provider json", "zzzz-no-match", "alpha", "d120"];

// UI 打开文件的真实路径（packages/ui/src/PreviewPane.tsx）：一次 readTextFile(offset 0, FILE_VIEWER_MAX_TEXT_BYTES)，
// 100 MiB 文件返回 truncated 后显示“文件过大”，不会整读。
const FILE_VIEWER_MAX_TEXT_BYTES = 256 * 1024;
// 两侧 readFileRange 单次窗口上限都是 1 MiB（TS fileService MAX_BINARY_READ_BYTES / Rust MAX_BINARY_READ_BYTES）。
const RANGE_WINDOW_BYTES = 1024 * 1024;
const UI_OPEN_REPS = 20;

async function measureFileOpen(session, fx, reps) {
  const uiOpen = [];
  const preview = [];
  const full = [];
  let bytes = 0;
  let truncated = null;
  // 第 0 次为预热，不计入统计（两侧同样处理）。
  for (let rep = 0; rep <= UI_OPEN_REPS; rep += 1) {
    const { ms, value } = await timed(session, "file", "readTextFile", { path: fx.bigFile, offset: 0, length: FILE_VIEWER_MAX_TEXT_BYTES });
    if (rep > 0) uiOpen.push(ms);
    truncated = value?.truncated ?? null;
  }
  for (let rep = 0; rep <= reps; rep += 1) {
    let started = now();
    await timed(session, "file", "stat", { path: fx.bigFile });
    await timed(session, "file", "readTextFile", { path: fx.bigFile, offset: 0, length: FILE_VIEWER_MAX_TEXT_BYTES });
    const previewMs = now() - started;
    started = now();
    const size = fx.scale.bigFileBytes;
    let read = 0;
    // 修复原因：之前按 4 MiB 请求并按 4 MiB 前进，但服务把窗口截到 1 MiB，实际只读了 25 MiB；
    // 现在按服务上限请求，并按实际返回的字节数前进，最后核对读满整个文件。
    while (read < size) {
      const { value } = await timed(session, "file", "readFileRange", { path: fx.bigFile, offset: read, length: Math.min(RANGE_WINDOW_BYTES, size - read) });
      const got = value?.data?.byteLength ?? value?.byteLength ?? value?.length ?? 0;
      if (got <= 0) throw new Error(`file.readFileRange returned no bytes at offset ${read}`);
      read += got;
    }
    const fullMs = now() - started;
    if (rep > 0) {
      preview.push(previewMs);
      full.push(fullMs);
    }
    bytes = read;
  }
  if (bytes !== fx.scale.bigFileBytes) throw new Error(`full read returned ${bytes} bytes, expected ${fx.scale.bigFileBytes}`);
  // 分层证据：同一调用在 64 KiB 与 1 MiB 两种窗口下的耗时，斜率=每 MiB 字节路径成本，截距≈每次调用的跳转成本。
  const rangeByWindow = {};
  for (const windowBytes of [64 * 1024, RANGE_WINDOW_BYTES]) {
    const samples = [];
    for (let rep = 0; rep <= UI_OPEN_REPS; rep += 1) {
      const { ms } = await timed(session, "file", "readFileRange", { path: fx.bigFile, offset: rep * RANGE_WINDOW_BYTES, length: windowBytes });
      if (rep > 0) samples.push(ms);
    }
    rangeByWindow[windowBytes] = samples;
  }
  return { rangeByWindow, uiOpenMs: uiOpen, uiOpenTruncated: truncated, previewMs: preview, fullReadMs: full, bytesReadLastRep: bytes, rangeWindowBytes: RANGE_WINDOW_BYTES };
}

async function measureSearch(session, fx, reps) {
  const index = await timed(session, "file", "searchWorkspaceFiles", { rootPath: fx.tree, query: "warmup", refresh: true });
  const length = await timed(session, "file", "listWorkspaceFilesLength", { rootPath: fx.tree });
  const samples = [];
  const perQuery = {};
  for (let rep = 0; rep <= reps; rep += 1) {
    for (const query of SEARCH_QUERIES) {
      const { ms, value } = await timed(session, "file", "searchWorkspaceFiles", { rootPath: fx.tree, query });
      if (rep === 0) continue;
      samples.push(ms);
      perQuery[query] ??= { ms: [], results: Array.isArray(value) ? value.length : (value?.items?.length ?? value?.results?.length ?? null) };
      perQuery[query].ms.push(Math.round(ms * 10) / 10);
    }
  }
  return { indexRefreshMs: index.ms, indexedFiles: length.value, queryMs: samples, perQuery };
}

async function measureGit(session, fx, reps) {
  const status = [];
  const refresh = [];
  let changes = null;
  for (let rep = 0; rep <= reps; rep += 1) {
    const r = await timed(session, "git", "refresh", { workspacePath: fx.repo });
    const c = await timed(session, "git", "getChanges", { workspacePath: fx.repo, sourceId: "unstaged" });
    if (rep === 0) continue;
    refresh.push(r.ms);
    status.push(c.ms);
    const value = c.value;
    changes = Array.isArray(value) ? value.length : (value?.changes?.length ?? value?.files?.length ?? value?.items?.length ?? null);
  }
  return { refreshMs: refresh, getChangesMs: status, statusMs: refresh.map((ms, index) => ms + status[index]), changes };
}

async function measureWatcher(session, fx, reps, scratch) {
  const channel = session.client.getChannel("file-watcher");
  const setupStarted = now();
  const { id } = await channel.call("watch", [{ path: fx.tree, recursive: true }]);
  const setupMs = now() - setupStarted;
  const events = [];
  let notify = null;
  const subscription = channel.listen("onDynamicChange", id)((event) => {
    events.push({ at: now(), event });
    notify?.();
  });
  const samples = [];
  const missed = [];
  const created = [];
  try {
    // 先写一个文件并等它的事件，确认 watcher 已就绪（事件驱动，有上限）。
    const settle = async (file) => {
      const before = events.length;
      const started = now();
      const fired = new Promise((resolve) => {
        const timer = setTimeout(() => resolve(false), 10_000);
        notify = () => {
          if (events.length > before) {
            clearTimeout(timer);
            resolve(true);
          }
        };
      });
      await writeFile(file, `phase7 ${Date.now()}\n`);
      created.push(file);
      const ok = await fired;
      notify = null;
      return ok ? events[before].at - started : null;
    };
    await mkdir(path.join(fx.tree, "pkg3", "mod0", "p7watch"), { recursive: true });
    const warm = await settle(path.join(fx.tree, "pkg3", "mod0", "p7watch", `warm-${scratch}.txt`));
    for (let rep = 0; rep < reps; rep += 1) {
      const ms = await settle(path.join(fx.tree, "pkg3", "mod0", "p7watch", `w-${scratch}-${rep}.txt`));
      if (ms === null) missed.push(rep);
      else samples.push(ms);
      // 等防抖窗口内的尾随事件结束，避免下一次样本被上一次的事件触发。
      await new Promise((resolve) => setTimeout(resolve, 400));
    }
    return { setupMs, warmMs: warm, eventMs: samples, missed };
  } finally {
    subscription.dispose();
    await channel.call("unwatch", [{ id }]).catch(() => {});
    await rm(path.join(fx.tree, "pkg3", "mod0", "p7watch"), { recursive: true, force: true });
  }
}

/**
 * Runs every service measurement for one side. `env` selects the routing. `parts` toggles
 * subsets. Returns raw samples per metric.
 */
export async function runServiceSide({ label, env, fx, reps, watchReps, log }) {
  verifyArtifacts();
  const modules = await loadModules();
  const mode = `p7perf-${label}-${process.pid}`;
  const session = await openHostSession({ modules, mode, env: { HARNESS_RUST_BRIDGE_TRACE: "1", ...env }, step: `P7_${label.toUpperCase()}` });
  const out = { label, env: Object.fromEntries(Object.entries(env).map(([key, value]) => [key, key === "HARNESS_RUST_HOST" ? path.basename(String(value)) : value])) };
  try {
    log(`[service:${label}] file open`);
    out.fileOpen = await measureFileOpen(session, fx, reps);
    log(`[service:${label}] search`);
    out.search = await measureSearch(session, fx, reps);
    log(`[service:${label}] git status`);
    out.git = await measureGit(session, fx, reps);
    log(`[service:${label}] watcher`);
    out.watcher = await measureWatcher(session, fx, watchReps, label);
    out.routes = [...new Set(session.trace.map((entry) => `${entry.method}:${entry.outcome}`))].sort();
  } finally {
    // 关闭前记录 sidecar 进程树（Host / harness-host / Agent），关闭后核对无残留；只处理本次启动的树。
    const tree = session.sidecar.child.pid ? await processTree([session.sidecar.child.pid]).catch(() => []) : [];
    out.exit = await session.close().catch((error) => ({ error: String(error?.message ?? error) }));
    let alive = await stillAlive(tree).catch(() => []);
    const deadline = Date.now() + 15_000;
    while (alive.length > 0 && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 500));
      alive = await stillAlive(tree).catch(() => []);
    }
    if (alive.length > 0) {
      out.leftoversKilled = alive.map((entry) => `${entry.name ?? ""}#${entry.pid}`);
      await killTrees(alive.map((entry) => entry.pid));
    }
    out.treeSize = tree.length;
    await rm(path.join(paths.runDir, `data-${mode}`), { recursive: true, force: true, maxRetries: 6, retryDelay: 250 }).catch(() => {});
  }
  return out;
}
