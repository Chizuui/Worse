// Isolated home for every e2e gate + a guard that the user's real ZCode data was not touched.
//
// 修复说明：各 gate 启动真实的 ZCode Host（以及 Agent CLI），而 ZCode 从用户家目录解析数据目录：
//   - settingService:   ZCODE_DESKTOP_HOME_DIR > HOME > USERPROFILE > os.homedir()  -> ~/.zcode/v2/setting.json
//   - services/paths:   ZCODE_DATA_BASE_DIR > HOME > os.homedir()                    -> ~/.zcode/v2/*
//   - commands/hooks/memory/skills/subagents/*-sync: HOME > USERPROFILE > os.homedir()
//   - sshConfigAlias、credentialCipherProvider、Agent CLI(ZCODE_STORAGE_DIR 缺省): os.homedir()
//     （Windows 上 os.homedir() 读 USERPROFILE，不读 HOME）
//   - mcpUserDirectory/legacy、sea-playwright/tui loader: APPDATA / LOCALAPPDATA / XDG_*
//   - harness-store: ZCODE_DATA_BASE_DIR > HOME > USERPROFILE；harness-pty: USERPROFILE/HOME
// 之前只有 ZCODE_DATA_BASE_DIR 被隔离，setting.json 仍按 HOME 写进真实 ~/.zcode，导致 gate 把
// WSL 连接档案和指向 127.0.0.1 测试 relay 的 webRemoteControlRelayV2Device 写入用户真实设置。
// 现在 gate 进程一启动就把上述全部变量改写到 Worse/.work/tmp/e2e-home-<gate>-<pid>，
// 之后 spawn 的所有子进程（sidecar、Host、Agent、harness-host、git、pty、wsl）都继承隔离环境；
// 退出时比对真实 ~/.zcode/v2 关键文件的 size/mtime/sha256，变化即打印 HOME_LEAK 并以非零码退出。
import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, rmSync, statSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../..");
const tmpRoot = path.join(repoRoot, "Worse/.work/tmp");

/** Real ZCode files whose change means a gate leaked into the user's home. Never read for content. */
const GUARDED_FILES = [
  ".zcode/v2/setting.json",
  ".zcode/v2/tasks-index.sqlite",
  ".zcode/v2/credentials.json",
  // Fix B：MCP 用户目录写入走 HOME/.zcode/cli/config.json，gate 写错 home 时这里会报 HOME_LEAK。
  ".zcode/cli/config.json",
];

/**
 * Inherited variables that would point ZCode/agents at real user directories regardless of HOME.
 * They are removed so the defaults resolve inside the isolated home.
 */
const INHERITED_PATH_OVERRIDES = [
  "ZCODE_HOME",
  "ZCODE_STORAGE_DIR",
  "ZCODE_LOG_DIR",
  "ZCODE_SESSION_DB_PATH",
  "ZCODE_SESSION_DB",
  "ZCODE_DESKTOP_USER_DATA_DIR",
  "ZCODE_DESKTOP_SESSION_DATA_DIR",
  "ZCODE_REMOTE_ASSET_CACHE_DIR",
  "HARNESS_APP_DATA_DIR",
];

let active = null;

/** Candidate real home directories, captured before any override (HOME and USERPROFILE may differ). */
function realHomeCandidates() {
  const seen = new Set();
  const homes = [];
  for (const candidate of [process.env.ZCODE_DESKTOP_HOME_DIR, process.env.HOME, process.env.USERPROFILE, os.homedir()]) {
    const value = candidate?.trim();
    if (!value) continue;
    const key = process.platform === "win32" ? path.resolve(value).toLowerCase() : path.resolve(value);
    if (seen.has(key)) continue;
    seen.add(key);
    homes.push(path.resolve(value));
  }
  return homes;
}

/** size + mtime + sha256 of a file, or null when it does not exist. Content is hashed, never printed. */
function fingerprint(file) {
  let stat;
  try {
    stat = statSync(file);
  } catch {
    return null;
  }
  if (!stat.isFile()) return { kind: "non-file" };
  let sha256;
  try {
    sha256 = createHash("sha256").update(readFileSync(file)).digest("hex");
  } catch (error) {
    sha256 = `unreadable:${error?.code ?? "error"}`;
  }
  return { size: stat.size, mtimeMs: stat.mtimeMs, sha256 };
}

function snapshotRealFiles(homes) {
  const entries = [];
  for (const home of homes) {
    for (const relative of GUARDED_FILES) {
      const file = path.join(home, relative);
      entries.push({ file, print: file, before: fingerprint(file) });
    }
  }
  return entries;
}

function describeChange(before, after) {
  if (!before && after) return "created";
  if (before && !after) return "deleted";
  const changed = [];
  if (before.size !== after.size) changed.push("size");
  if (before.mtimeMs !== after.mtimeMs) changed.push("mtime");
  if (before.sha256 !== after.sha256) changed.push("sha256");
  return changed.join("+");
}

/** Returns the leaks (file + change kind) since the snapshot. */
export function checkHomeLeaks() {
  if (!active) return [];
  const leaks = [];
  for (const entry of active.snapshot) {
    const after = fingerprint(entry.file);
    if (JSON.stringify(entry.before) === JSON.stringify(after)) continue;
    leaks.push({ file: entry.file, print: entry.print, change: describeChange(entry.before, after) });
  }
  return leaks;
}

function buildEnv(home) {
  const appData = path.join(home, "AppData", "Roaming");
  const localAppData = path.join(home, "AppData", "Local");
  const env = {
    HOME: home,
    USERPROFILE: home,
    APPDATA: appData,
    LOCALAPPDATA: localAppData,
    XDG_CONFIG_HOME: path.join(home, ".config"),
    XDG_DATA_HOME: path.join(home, ".local", "share"),
    XDG_CACHE_HOME: path.join(home, ".cache"),
    XDG_STATE_HOME: path.join(home, ".local", "state"),
    // settingService 优先读 ZCODE_DESKTOP_HOME_DIR；gate 自己的 ZCODE_DATA_BASE_DIR 仍可覆盖数据根。
    ZCODE_DESKTOP_HOME_DIR: home,
    ZCODE_DATA_BASE_DIR: home,
    CODEX_HOME: path.join(home, ".codex"),
    CLAUDE_CONFIG_DIR: path.join(home, ".claude"),
  };
  if (process.platform === "win32") {
    const parsed = path.parse(home);
    env.HOMEDRIVE = parsed.root.replace(/[\\/]+$/, "");
    env.HOMEPATH = home.slice(env.HOMEDRIVE.length);
    Object.assign(env, storePwshFreePath());
  }
  return env;
}

/**
 * 修复原因：2026-11-01 起本机 gate 内由 TS Host（未修改的 ZCode 终端代码）启动 Microsoft Store 版
 * pwsh.exe（MSIX，位于 WindowsApps）稳定报 "Cannot create process, error code: 5"；同一 node-pty 调用
 * 在独立探针中（真实/隔离 env、隐藏窗口、相同 cwd）均能启动，根因未确认。gate 比较的是 TS 与 Rust
 * 的行为一致性，而非 Store 版 pwsh 能否被激活，因此默认从 PATH 去掉 WindowsApps 目录，让 ZCode 的
 * resolveTerminalShell 回落到 powershell.exe，TS 与 Rust 两侧使用同一 shell。
 * 设置 HARNESS_E2E_KEEP_STORE_PWSH=1 保留原 PATH 以复现该问题。
 */
function storePwshFreePath() {
  if (process.env.HARNESS_E2E_KEEP_STORE_PWSH === "1") return {};
  const key = Object.keys(process.env).find((name) => name.toUpperCase() === "PATH") ?? "Path";
  const entries = (process.env[key] ?? "").split(path.delimiter);
  const kept = entries.filter((entry) => !/\\WindowsApps(\\|$)/i.test(entry));
  if (kept.length === entries.length) return {};
  return { [key]: kept.join(path.delimiter) };
}

function cleanupHome() {
  if (!active || active.cleaned) return;
  active.cleaned = true;
  if (process.env.HARNESS_E2E_KEEP_HOME === "1") {
    process.stderr.write(`HOME_KEPT ${active.home}\n`);
    return;
  }
  try {
    // Windows 上刚退出的子进程可能短暂持有 SQLite 句柄，用有限重试。
    rmSync(active.home, { recursive: true, force: true, maxRetries: 6, retryDelay: 250 });
  } catch (error) {
    process.stderr.write(`note: could not remove isolated home ${active.home} (${error?.code ?? "error"})\n`);
  }
}

function onExit() {
  if (!active) return;
  const leaks = checkHomeLeaks();
  if (leaks.length > 0) {
    for (const leak of leaks) {
      process.stdout.write(
        `HOME_LEAK ${leak.print} (${leak.change}) — the real file changed during gate '${active.gate}'. ` +
          "Cause may be the gate, or a running ZCode app writing its own data; close ZCode and rerun to tell them apart.\n",
      );
    }
    process.exitCode = 1;
  } else {
    process.stderr.write(`HOME_GUARD_OK ${active.gate}: ${active.snapshot.length} real file slot(s) unchanged\n`);
  }
  cleanupHome();
}

/**
 * Creates the throwaway home for `gate`, rewrites this process's environment so every child process
 * inherits it, snapshots the real guarded files, and installs the exit guard + cleanup.
 * Idempotent per process. Returns `{ home, env }` (`env` = the overrides only).
 */
export function installIsolatedHome(gate) {
  if (active) return { home: active.home, env: { ...active.env } };
  const homes = realHomeCandidates();
  const snapshot = snapshotRealFiles(homes);
  const home = path.join(tmpRoot, `e2e-home-${gate}-${process.pid}`);
  rmSync(home, { recursive: true, force: true });
  const env = buildEnv(home);
  for (const dir of [home, env.APPDATA, env.LOCALAPPDATA, env.XDG_CONFIG_HOME, env.XDG_DATA_HOME, env.XDG_CACHE_HOME, env.XDG_STATE_HOME]) {
    mkdirSync(dir, { recursive: true });
  }
  for (const key of INHERITED_PATH_OVERRIDES) delete process.env[key];
  // TEMP/TMP 保持真实值：临时目录不属于 ZCode 用户数据，且 mkdtemp/ConPTY 依赖可用的系统临时目录。
  Object.assign(process.env, env);
  active = { gate, home, env, snapshot, realHomes: homes, cleaned: false };
  process.on("exit", onExit);
  for (const signal of ["SIGINT", "SIGTERM"]) {
    process.once(signal, () => process.exit(130));
  }
  return { home, env: { ...env } };
}

/** The isolation overrides for spawning a child; throws when the gate never installed isolation. */
export function isolatedHomeEnv() {
  if (!active) {
    throw new Error("isolated home is not installed: call installIsolatedHome(<gate>) before spawning ZCode processes");
  }
  return { ...active.env };
}

/** The isolated home directory of the running gate. */
export function isolatedHomeDir() {
  return isolatedHomeEnv().HOME;
}
