// Phase 5: the Chromium sidecar behind Harness's browser owner.
//
// Chromium is launched with `--remote-debugging-pipe` (CDP over fds 3/4, NUL-delimited JSON), so no
// debugging port is ever opened on the machine and Chromium exits by itself when this process (and
// with it the pipe) goes away. The launcher is a child of the Host sidecar, which the Rust shell
// supervises (kill-tree on shutdown). See docs/specs/harness/phase-5-browser.md.
import { spawn, type ChildProcess } from "node:child_process";
import { constants, accessSync, existsSync, statSync } from "node:fs";
import { mkdir, mkdtemp, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import type { Readable, Writable } from "node:stream";
import { log } from "./log.ts";

export type CdpParams = Record<string, unknown>;
export type CdpEventListener = (method: string, params: CdpParams, sessionId: string | undefined) => void;

interface PendingCall {
  resolve(value: CdpParams): void;
  reject(error: Error): void;
  method: string;
}

export class CdpError extends Error {
  readonly code: number | undefined;
  constructor(message: string, code?: number) {
    super(message);
    this.name = "CdpError";
    this.code = code;
  }
}

/** One browser-level CDP connection; sessions are flattened (`sessionId` on every frame). */
export class CdpConnection {
  readonly #write: (frame: string) => void;
  readonly #pending = new Map<number, PendingCall>();
  readonly #listeners = new Set<CdpEventListener>();
  readonly #closeListeners = new Set<(reason: string) => void>();
  #nextId = 1;
  #closed: string | null = null;

  constructor(write: (frame: string) => void) {
    this.#write = write;
  }

  get closed(): boolean {
    return this.#closed !== null;
  }

  send(method: string, params: CdpParams = {}, sessionId?: string): Promise<CdpParams> {
    if (this.#closed !== null) return Promise.reject(new CdpError(`CDP connection closed: ${this.#closed}`));
    const id = this.#nextId++;
    const frame: Record<string, unknown> = { id, method, params };
    if (sessionId !== undefined && sessionId !== "") frame.sessionId = sessionId;
    return new Promise<CdpParams>((resolve, reject) => {
      this.#pending.set(id, { resolve, reject, method });
      try {
        this.#write(JSON.stringify(frame));
      } catch (error) {
        this.#pending.delete(id);
        reject(error instanceof Error ? error : new Error(String(error)));
      }
    });
  }

  onEvent(listener: CdpEventListener): () => void {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }

  onClose(listener: (reason: string) => void): () => void {
    this.#closeListeners.add(listener);
    return () => this.#closeListeners.delete(listener);
  }

  /** Feeds one complete JSON frame received from Chromium. */
  dispatch(raw: string): void {
    let message: { id?: number; method?: string; params?: CdpParams; result?: CdpParams; error?: { message?: string; code?: number }; sessionId?: string };
    try {
      message = JSON.parse(raw) as typeof message;
    } catch {
      log("warn", "[browser] dropped a malformed CDP frame");
      return;
    }
    if (typeof message.id === "number") {
      const call = this.#pending.get(message.id);
      if (call === undefined) return;
      this.#pending.delete(message.id);
      if (message.error) {
        call.reject(new CdpError(`${call.method}: ${message.error.message ?? "CDP error"}`, message.error.code));
      } else {
        call.resolve(message.result ?? {});
      }
      return;
    }
    if (typeof message.method !== "string") return;
    const params = message.params ?? {};
    for (const listener of Array.from(this.#listeners)) {
      try {
        listener(message.method, params, message.sessionId);
      } catch (error) {
        log("warn", `[browser] CDP event listener threw on ${message.method}`, error);
      }
    }
  }

  close(reason: string): void {
    if (this.#closed !== null) return;
    this.#closed = reason;
    const error = new CdpError(`CDP connection closed: ${reason}`);
    for (const call of this.#pending.values()) call.reject(error);
    this.#pending.clear();
    for (const listener of Array.from(this.#closeListeners)) {
      try {
        listener(reason);
      } catch {
        // close 通知失败不影响其余监听者收尾。
      }
    }
    this.#listeners.clear();
    this.#closeListeners.clear();
  }
}

// ---------------------------------------------------------------------------------------------
// Executable discovery: the same candidate list as ZCode's CLI adapter
// (apps/zcode-cli/packages/adapters/src/browser/executable.ts), minus Playwright's bundled browser.

const LINUX_BROWSER_PATHS = [
  "/usr/bin/google-chrome-stable",
  "/usr/bin/google-chrome",
  "/usr/bin/chromium",
  "/usr/bin/chromium-browser",
  "/snap/bin/chromium",
  "/usr/bin/microsoft-edge"
];

const MAC_BROWSER_PATHS = [
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  "/Applications/Chromium.app/Contents/MacOS/Chromium",
  "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"
];

/** Windows env names are case-insensitive only on `process.env` itself; a copied object keeps the
 * original casing (`ProgramFiles`), so look the name up case-insensitively. */
function readEnv(env: NodeJS.ProcessEnv, name: string): string | undefined {
  const direct = env[name];
  if (direct !== undefined) return direct;
  const key = Object.keys(env).find((candidate) => candidate.toUpperCase() === name);
  return key === undefined ? undefined : env[key];
}

function windowsBrowserPaths(env: NodeJS.ProcessEnv): string[] {
  // 修复说明：gate 以普通对象传入 env 时 `env.PROGRAMFILES` 读不到 `ProgramFiles`，导致找不到已安装的 Chrome。
  return [readEnv(env, "PROGRAMFILES"), readEnv(env, "PROGRAMFILES(X86)"), readEnv(env, "LOCALAPPDATA")]
    .filter((root): root is string => Boolean(root?.trim()))
    .flatMap((root) => [
      path.join(root, "Google", "Chrome", "Application", "chrome.exe"),
      path.join(root, "Chromium", "Application", "chrome.exe"),
      path.join(root, "Microsoft", "Edge", "Application", "msedge.exe")
    ]);
}

function isUsableExecutable(candidate: string, platform: NodeJS.Platform): boolean {
  try {
    if (!existsSync(candidate) || !statSync(candidate).isFile()) return false;
    if (platform !== "win32") accessSync(candidate, constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

export interface BrowserExecutableResolution {
  path: string | null;
  candidates: string[];
}

/** `HARNESS_BROWSER_EXECUTABLE` (absolute) wins; otherwise installed Chrome / Chromium / Edge. */
export function resolveBrowserExecutable(
  env: NodeJS.ProcessEnv = process.env,
  platform: NodeJS.Platform = process.platform
): BrowserExecutableResolution {
  const explicit = env.HARNESS_BROWSER_EXECUTABLE?.trim();
  const candidates =
    explicit !== undefined && explicit !== ""
      ? [explicit]
      : platform === "darwin"
        ? MAC_BROWSER_PATHS
        : platform === "win32"
          ? windowsBrowserPaths(env)
          : LINUX_BROWSER_PATHS;
  const found = candidates.find((candidate) => path.isAbsolute(candidate) && isUsableExecutable(candidate, platform));
  return { path: found ?? null, candidates };
}

// ---------------------------------------------------------------------------------------------

export interface ChromiumProcess {
  readonly connection: CdpConnection;
  readonly pid: number | undefined;
  readonly executable: string;
  readonly userDataDir: string;
  readonly headless: boolean;
  /** Closes the browser (Browser.close), then kills the process if it does not exit; idempotent. */
  close(): Promise<void>;
  /** Resolves once the Chromium process has exited. */
  readonly exited: Promise<void>;
}

export interface LaunchChromiumOptions {
  executable: string;
  userDataDir: string;
  headless: boolean;
  /** Removed after exit (temporary profile fallback). */
  disposableProfile?: boolean;
  closeTimeoutMs?: number;
  /** Owner env (`HARNESS_BROWSER_GPU`); defaults to process.env. */
  env?: NodeJS.ProcessEnv;
}

const CHROMIUM_ARGS = [
  "--remote-debugging-pipe",
  "--no-first-run",
  "--no-default-browser-check",
  "--disable-sync",
  "--disable-background-networking",
  "--disable-component-update",
  "--disable-default-apps",
  "--disable-features=Translate,MediaRouter,OptimizationHints",
  "--disable-popup-blocking",
  // agent 页面不在前台也要按时产帧：否则输入事件等待下一帧，type/click 每次多出几十毫秒。
  "--disable-renderer-backgrounding",
  "--disable-background-timer-throttling",
  "--disable-backgrounding-occluded-windows",
  "--password-store=basic",
  "--use-mock-keychain",
  "--no-service-autorun",
  "--metrics-recording-only"
];

function attachPipeReader(stream: Readable, onFrame: (frame: string) => void): void {
  let pending: Buffer[] = [];
  stream.on("data", (chunk: Buffer) => {
    let start = 0;
    for (let index = chunk.indexOf(0); index !== -1; index = chunk.indexOf(0, start)) {
      pending.push(chunk.subarray(start, index));
      const frame = Buffer.concat(pending).toString("utf8");
      pending = [];
      start = index + 1;
      onFrame(frame);
    }
    if (start < chunk.length) pending.push(chunk.subarray(start));
  });
}

/** Full Chromium argv (minus the executable) for one launch. */
export function chromiumLaunchArgs(input: {
  userDataDir: string;
  headless: boolean;
  platform?: NodeJS.Platform;
  env?: NodeJS.ProcessEnv;
}): string[] {
  const platform = input.platform ?? process.platform;
  const env = input.env ?? process.env;
  const args = [...CHROMIUM_ARGS, `--user-data-dir=${input.userDataDir}`];
  if (input.headless) {
    args.push("--headless=new");
    // 修复说明（首次输入延迟）：phase5-browser gate 中 Harness 首次 click 92ms、type 325ms，ZCode 只有 4/5ms。
    // CDP 计时显示耗时全部在第一条 Input.dispatchMouseEvent(mouseMoved) 等待 renderer ack 上（单条 50~210ms），
    // executor 自身的 Runtime.evaluate 只有 1ms。Page.lifecycleEvent 显示：新启动 Chromium 的首个
    // firstPaint 固定在启动后约 400ms 才到（比 navigate 完成晚约 125ms，与何时导航无关）；输入在首帧前到达就一直
    // 等到首帧。原因是 Windows 上 GPU 进程（ANGLE/D3D11）冷启动拖住首个合成帧：同一场景下创建 WebGL 上下文
    // 就要 99ms。加 --disable-gpu 后 firstPaint 在 navigate 后约 10ms 到达，首次 click 降到 4~6ms。已排除的
    // 假设：后台/遮挡节流 flag（已开启）、Emulation.setFocusEmulationEnabled、Page.bringToFront、PaintHolding。
    // ZCode 自己的 headless Chrome helper（chromeLocalStorageManager.ts）同样用 --headless=new --disable-gpu；
    // Windows 上 WebGL 仍经 WARP 可用。只在已测量过的 Windows headless 生效，HARNESS_BROWSER_GPU=1 可保留硬件 GPU。
    if (platform === "win32" && env.HARNESS_BROWSER_GPU !== "1") args.push("--disable-gpu");
  }
  // 不带启动 URL：headful 时也不会额外弹出空白窗口之外的页面；首个空白页由 owner 关闭。
  args.push("about:blank");
  return args;
}

export async function launchChromium(options: LaunchChromiumOptions): Promise<ChromiumProcess> {
  await mkdir(options.userDataDir, { recursive: true });
  const args = chromiumLaunchArgs({ userDataDir: options.userDataDir, headless: options.headless, ...(options.env ? { env: options.env } : {}) });

  const child: ChildProcess = spawn(options.executable, args, {
    stdio: ["ignore", "ignore", "pipe", "pipe", "pipe"],
    // Windows：CREATE_NO_WINDOW，避免 GUI 宿主下闪出控制台窗口。
    windowsHide: true,
    detached: false
  });

  const toChromium = child.stdio[3] as Writable | null;
  const fromChromium = child.stdio[4] as Readable | null;
  if (toChromium === null || fromChromium === null) {
    child.kill();
    throw new Error("Chromium did not expose the CDP pipe");
  }

  const connection = new CdpConnection((frame) => {
    toChromium.write(`${frame}\0`);
  });
  attachPipeReader(fromChromium, (frame) => connection.dispatch(frame));

  let stderrTail = "";
  child.stderr?.on("data", (chunk: Buffer) => {
    stderrTail = (stderrTail + chunk.toString("utf8")).slice(-4_000);
  });

  const exited = new Promise<void>((resolve) => {
    child.once("exit", (code, signal) => {
      connection.close(`chromium exited (code=${code ?? "null"} signal=${signal ?? "null"})`);
      resolve();
    });
    child.once("error", (error) => {
      connection.close(`chromium failed to start: ${error.message}`);
      resolve();
    });
  });
  // 管道写端报错（进程已退出）时只需收尾连接，避免未处理的 error 事件。
  toChromium.on("error", () => connection.close("cdp pipe write failed"));
  fromChromium.on("error", () => connection.close("cdp pipe read failed"));

  // 事件驱动的启动握手：Browser.getVersion 成功即可用；进程提前退出时连接关闭，send 立即失败。
  try {
    const version = await connection.send("Browser.getVersion");
    log("info", `[browser] chromium ready pid=${child.pid ?? "?"} product=${String(version.product ?? "?")} headless=${options.headless}`);
  } catch (error) {
    child.kill();
    await exited;
    const detail = stderrTail.trim().split(/\r?\n/u).slice(-3).join(" | ");
    throw new Error(`Chromium failed to start: ${error instanceof Error ? error.message : String(error)}${detail ? ` (${detail})` : ""}`);
  }

  let closing: Promise<void> | null = null;
  const close = (): Promise<void> => {
    closing ??= (async () => {
      if (!connection.closed) {
        await connection.send("Browser.close").catch(() => undefined);
      }
      const timeoutMs = options.closeTimeoutMs ?? 2_000;
      let timer: NodeJS.Timeout | undefined;
      await Promise.race([
        exited,
        new Promise<void>((resolve) => {
          timer = setTimeout(resolve, timeoutMs);
          timer.unref();
        })
      ]);
      if (timer) clearTimeout(timer);
      if (child.exitCode === null && child.signalCode === null) {
        child.kill();
        await exited;
      }
      if (options.disposableProfile) {
        await rm(options.userDataDir, { recursive: true, force: true }).catch(() => undefined);
      }
    })();
    return closing;
  };

  return {
    connection,
    pid: child.pid,
    executable: options.executable,
    userDataDir: options.userDataDir,
    headless: options.headless,
    close,
    exited
  };
}

/** Persistent profile first (logins survive restarts, like ZCode's persist: partition); a locked
 * profile (second Harness window) falls back to a throwaway one. */
export async function launchChromiumWithProfileFallback(input: {
  executable: string;
  profileDir: string;
  headless: boolean;
  env?: NodeJS.ProcessEnv;
}): Promise<ChromiumProcess> {
  try {
    return await launchChromium({ executable: input.executable, userDataDir: input.profileDir, headless: input.headless, ...(input.env ? { env: input.env } : {}) });
  } catch (error) {
    log("warn", `[browser] persistent profile launch failed, retrying with a temporary profile: ${error instanceof Error ? error.message : String(error)}`);
    const tempDir = await mkdtemp(path.join(os.tmpdir(), "harness-browser-"));
    return await launchChromium({ executable: input.executable, userDataDir: tempDir, headless: input.headless, disposableProfile: true, ...(input.env ? { env: input.env } : {}) });
  }
}
