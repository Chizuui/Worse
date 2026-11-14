import { existsSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { parseRustServiceFlags } from "./routing/flags.ts";
import type { RustService } from "./routing/routeTable.ts";

export interface BridgeConfig {
  services: ReadonlySet<RustService>;
  binaryPath: string;
  cwd: string;
  trace: boolean;
  unknownFlags: string[];
  /**
   * Windows: directory with node-pty's bundled `conpty.dll` + `OpenConsole.exe`, prepended to
   * harness-host's `PATH` so portable-pty sideloads it (null = nothing to add).
   */
  conptyDir: string | null;
}

export interface ConptyProbe {
  platform?: NodeJS.Platform;
  arch?: string;
  exists?: (candidate: string) => boolean;
  /** Resolves `node-pty/package.json`; throws when node-pty is not installed. */
  resolveNodePty?: () => string;
}

const CONPTY_FILES = ["conpty.dll", "OpenConsole.exe"] as const;

/**
 * 修复原因（Phase 3b 回显延迟，见 Worse/.work/phase3b/fix.md）：TS Host 用 node-pty 的
 * `useConptyDll: true`，即 node-pty 自带的 conpty.dll + OpenConsole.exe；harness-host 的 portable-pty
 * 只有在 `LoadLibrary("conpty.dll")` 找得到时才用它，否则退回 Windows 10 自带的 conhost。自带 conhost 的
 * VT 渲染线程每帧之后固定休眠一帧（实测落在 ~15.6ms 时钟节拍上），连续输入时按键回显平均晚约 9ms、
 * 命令输出常常晚一整帧。这里为 harness-host 找到与 TS 相同的 conpty 目录，由调用方加到子进程 PATH
 * 最前面，两条链路因此使用同一个 ConPTY 实现。
 *
 * Order: `HARNESS_CONPTY_DIR` (`off`/`none`/empty = inbox ConPTY); nothing when a `conpty.dll` already
 * sits next to the binary (portable-pty finds it first); else node-pty's `prebuilds/win32-<arch>/conpty`.
 * A directory missing either file is ignored, so a broken install keeps the inbox ConPTY.
 */
export function resolveConptyDir(env: NodeJS.ProcessEnv, binaryPath: string, probe: ConptyProbe = {}): string | null {
  const platform = probe.platform ?? process.platform;
  if (platform !== "win32") return null;
  const exists = probe.exists ?? existsSync;
  const complete = (dir: string): boolean => CONPTY_FILES.every((file) => exists(path.join(dir, file)));
  const override = env.HARNESS_CONPTY_DIR;
  if (override !== undefined) {
    const trimmed = override.trim();
    if (trimmed === "" || /^(off|none)$/i.test(trimmed)) return null;
    return complete(trimmed) ? path.resolve(trimmed) : null;
  }
  if (exists(path.join(path.dirname(binaryPath), "conpty.dll"))) return null;
  const resolveNodePty = probe.resolveNodePty ?? (() => createRequire(import.meta.url).resolve("node-pty/package.json"));
  let packageJson: string;
  try {
    packageJson = resolveNodePty();
  } catch {
    return null;
  }
  const dir = path.join(path.dirname(packageJson), "prebuilds", `win32-${probe.arch ?? process.arch}`, "conpty");
  return complete(dir) ? dir : null;
}

/** harness-host's environment: the bridge env with `conptyDir` first on `PATH` (key case kept). */
export function rustHostEnv(env: NodeJS.ProcessEnv, conptyDir: string | null): NodeJS.ProcessEnv {
  if (conptyDir === null) return env;
  const key = Object.keys(env).find((name) => name.toUpperCase() === "PATH") ?? "PATH";
  const current = env[key];
  return { ...env, [key]: current ? `${conptyDir};${current}` : conptyDir };
}

/**
 * `Worse/target/release/harness-host(.exe)` when it exists, else `Worse/target/debug/...`,
 * resolved from this package (src/ or dist/). A debug build is ~5x slower on the file index
 * than the TS Host, so a release build wins when both are present.
 */
export function defaultRustHostPath(
  platform: NodeJS.Platform = process.platform,
  exists: (candidate: string) => boolean = existsSync
): string {
  const packageDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
  const executable = platform === "win32" ? "harness-host.exe" : "harness-host";
  const release = path.resolve(packageDir, "../../target/release", executable);
  // 启动时一次性同步探测（只在读取配置时执行一次），不在请求路径上做同步 IO。
  return exists(release) ? release : path.resolve(packageDir, "../../target/debug", executable);
}

/**
 * Reads the bridge configuration:
 * - `HARNESS_RUST_SERVICES`: `fs,search,git` | `all` | `none` (default in flags.ts)
 * - `HARNESS_RUST_HOST`: path to the harness-host binary
 * - `HARNESS_RUST_BRIDGE_TRACE=1`: log each routing outcome
 * - `HARNESS_CONPTY_DIR`: Windows ConPTY sideload directory (`off` = inbox ConPTY), see resolveConptyDir
 */
export function readBridgeConfig(env: NodeJS.ProcessEnv, cwd: string): BridgeConfig {
  const flags = parseRustServiceFlags(env.HARNESS_RUST_SERVICES);
  const override = env.HARNESS_RUST_HOST?.trim();
  const binaryPath = override ? path.resolve(cwd, override) : defaultRustHostPath();
  return {
    services: flags.services,
    binaryPath,
    cwd,
    trace: env.HARNESS_RUST_BRIDGE_TRACE === "1",
    unknownFlags: flags.unknown,
    // 启动时一次性同步探测（同 defaultRustHostPath），不在请求路径上。
    conptyDir: resolveConptyDir(env, binaryPath)
  };
}
