import { invoke } from "@tauri-apps/api/core";
import type { PluginSandboxPlatformPort, PluginSandboxPortsEvent } from "@zcode/shared/mcp-apps";
// The shared UI logger (packages/ui/src/logger.ts); @zcode/ui does not re-export it.
import { logger } from "@/logger.js";

/**
 * Desktop plugin UI sandbox port (Phase 5, spec `Worse/docs/specs/harness/phase-5-plugins.md`, 4.6).
 *
 * ZCode's UI (`pluginUiPagePlane.ts`) mounts an Electron `<webview>` per sandbox. WebView2 has no
 * guest views, so this module adopts each such element and puts a cross-origin `<iframe>` (the
 * Harness relay shell served by `hsandbox://`) inside it, then re-creates the few guest events the
 * UI listens to and the Main → renderer port handshake:
 * - first shell load → `dom-ready`, then `{type:"init", sandboxId, initId}` + one MessagePort to the
 *   shell (target origin pinned to the shell origin) and the other port to `onPorts` handlers;
 * - plugin frame loaded / reloaded (reported by the shell) → `did-frame-finish-load` /
 *   `did-start-navigation` (the UI treats the latter as a crash, same as ZCode);
 * - shell reload → main-frame `did-start-navigation`.
 * The shell iframe only delegates `clipboard-write` (granted by declaration in ZCode too; the shell
 * forwards it to the plugin frame only when the record declares `clipboardWrite`). Camera /
 * microphone / geolocation are never delegated, so Permissions Policy blocks them in every plugin
 * frame (ZCode prompts for them; Harness has no prompt yet, spec section 2). Records are owned by the host-node sandbox owner; this
 * module keeps only per-element DOM wiring and the gesture timestamps.
 */

const SANDBOX_ELEMENT = "webview[data-zcode-retained-sandbox]";
/** Same window as ZCode's PLUGIN_SANDBOX_USER_GESTURE_TTL_MS. */
const USER_GESTURE_TTL_MS = 5000;
/** One Harness window owns one Host; host-node accepts registrations only for this id. */
const OWNER_WEB_CONTENTS_ID = 1;
const SHELL_IFRAME_SANDBOX = "allow-scripts allow-same-origin allow-forms";
/** Upper bound of what a plugin frame can receive; the shell narrows it to the record's declaration. */
const SHELL_IFRAME_ALLOW = "clipboard-write";

type SandboxCallMethod = "dispose" | "resetOwner";

interface AdoptedSandbox {
  sandboxId: string;
  initId: number;
  shellOrigin: string;
  iframe: HTMLIFrameElement;
}

function callSandbox(method: SandboxCallMethod, params: Record<string, unknown>): Promise<unknown> {
  return invoke("plugin_sandbox_call", { method, params });
}

/** Windows (WebView2 maps custom schemes to http) and macOS/Linux shell origins. */
const SHELL_ORIGIN = /^(?:http:\/\/hsandbox\.shell-[a-f0-9]{32}\.localhost|hsandbox:\/\/shell-[a-f0-9]{32}\.localhost)$/;

function parseShellUrl(raw: string): { url: URL; origin: string; sandboxId: string; initId: number } | null {
  let url: URL;
  try {
    url = new URL(raw);
  } catch {
    return null;
  }
  // 非特殊 scheme 的 URL.origin 为 "null"，这里按 scheme://host 自行拼出 shell 源。
  const origin = `${url.protocol}//${url.host}`;
  const sandboxId = url.pathname.split("/")[2];
  const initId = Number(/^#init=(\d+)$/.exec(url.hash)?.[1]);
  if (!SHELL_ORIGIN.test(origin) || !sandboxId || !Number.isSafeInteger(initId) || initId < 1) return null;
  return { url, origin, sandboxId, initId };
}

/**
 * Plugin UI is enabled only on Windows (WebView2), the configuration the phase5-plugins gate verifies.
 * 安全说明：wry 会把 Tauri 初始化脚本注入所有帧（WebView2 上忽略 for_main_frame_only）。Windows 上沙箱源是
 * `http://hsandbox.<kind>-<h32>.localhost`，Tauri `is_local_url` 判定为非本地，且子帧 postMessage 不进入 wry 的
 * WebMessageReceived，guest 的 invoke 无法到达命令。macOS/Linux 上 `hsandbox://` 的 scheme 名本身已注册，
 * 会被视为本地源，因此在核实前不启用（spec 第 2 节，needs sign-off）。
 */
export function harnessPluginSandboxSupported(): boolean {
  return /\bWindows\b/.test(globalThis.navigator?.userAgent ?? "");
}

function dispatchGuestEvent(element: Element, type: string, fields: Record<string, unknown> = {}): void {
  element.dispatchEvent(Object.assign(new Event(type), fields));
}

export function createHarnessPluginSandboxPlatform(): PluginSandboxPlatformPort {
  const handlers = new Set<(event: PluginSandboxPortsEvent) => void>();
  const adopted = new WeakMap<Element, AdoptedSandbox>();
  const byWindow = new Map<Window, { element: Element; sandbox: AdoptedSandbox }>();
  const focusedAt = new Map<string, number>();
  const consumedAt = new Map<string, number>();
  const supportsMove = typeof (Element.prototype as Element & { moveBefore?: unknown }).moveBefore === "function";

  const sandboxForActiveElement = (): AdoptedSandbox | undefined => {
    const active = document.activeElement;
    if (!(active instanceof HTMLIFrameElement)) return undefined;
    const host = active.parentElement;
    return host ? adopted.get(host) : undefined;
  };

  // 手势近似：WebView2 看不到跨源 iframe 内的输入；点击 iframe 会让宿主窗口失焦且 activeElement 指向该 iframe，
  // 并按 User Activation v2 把瞬时激活传播到祖先帧。两者同时成立才视为一次用户手势（spec 第 2 节 degraded）。
  window.addEventListener("blur", () => {
    const sandbox = sandboxForActiveElement();
    if (sandbox) focusedAt.set(sandbox.sandboxId, Date.now());
  });

  window.addEventListener("message", (event) => {
    const source = event.source as Window | null;
    const entry = source ? byWindow.get(source) : undefined;
    if (!entry || event.origin !== entry.sandbox.shellOrigin) return;
    const data = event.data as { type?: unknown; sandboxId?: unknown; event?: unknown } | null;
    if (typeof data !== "object" || data === null || data.type !== "harness-sandbox-frame") return;
    if (data.sandboxId !== entry.sandbox.sandboxId) return;
    if (data.event === "load") dispatchGuestEvent(entry.element, "did-frame-finish-load", { isMainFrame: false });
    else if (data.event === "navigate") {
      dispatchGuestEvent(entry.element, "did-start-navigation", { isMainFrame: false, isInPlace: false, url: "" });
    }
  });

  const adopt = (element: Element): void => {
    if (adopted.has(element)) return;
    const parsed = parseShellUrl(element.getAttribute("src") ?? "");
    if (!parsed) {
      logger.warn("[pluginSandbox] ignored a sandbox element with an unexpected shell URL");
      return;
    }
    const iframe = document.createElement("iframe");
    iframe.setAttribute("sandbox", SHELL_IFRAME_SANDBOX);
    iframe.setAttribute("referrerpolicy", "no-referrer");
    iframe.setAttribute("allow", SHELL_IFRAME_ALLOW);
    iframe.setAttribute("title", "plugin");
    iframe.style.cssText = "display:block;flex:1 1 auto;width:100%;height:100%;border:0;background:transparent";
    const sandbox: AdoptedSandbox = {
      sandboxId: parsed.sandboxId,
      initId: parsed.initId,
      shellOrigin: parsed.origin,
      iframe
    };
    adopted.set(element, sandbox);
    let loads = 0;
    let shellWindow: Window | null = null;
    iframe.addEventListener("load", () => {
      loads += 1;
      if (loads > 1) {
        // shell 文档自身重新加载 = Electron guest 主帧导航；UI 按崩溃处理并重新登记。
        dispatchGuestEvent(element, "did-start-navigation", { isMainFrame: true, isInPlace: false, url: "" });
        return;
      }
      shellWindow = iframe.contentWindow;
      if (!shellWindow) return;
      byWindow.set(shellWindow, { element, sandbox });
      dispatchGuestEvent(element, "dom-ready");
      const channel = new MessageChannel();
      shellWindow.postMessage(
        { type: "init", sandboxId: sandbox.sandboxId, initId: sandbox.initId },
        sandbox.shellOrigin,
        [channel.port2]
      );
      const event: PluginSandboxPortsEvent = {
        sandboxId: sandbox.sandboxId,
        initId: sandbox.initId,
        port: channel.port1 as unknown as PluginSandboxPortsEvent["port"]
      };
      for (const handler of handlers) handler(event);
    });
    iframe.src = parsed.url.href.replace(/#.*$/, "");
    if (supportsMove) {
      // 与 ZCode preload 相同：只有沙箱元素注册移动回调，pluginUiPagePlane 才用 moveBefore 原子迁移（iframe 不重载）。
      Object.defineProperty(element, "connectedMoveCallback", { value: () => undefined, configurable: true });
    }
    Object.defineProperty(element, "focus", { value: () => iframe.focus(), configurable: true });
    element.append(iframe);
    const cleanup = new MutationObserver(() => {
      if (element.isConnected) return;
      cleanup.disconnect();
      if (shellWindow) byWindow.delete(shellWindow);
      focusedAt.delete(sandbox.sandboxId);
      consumedAt.delete(sandbox.sandboxId);
    });
    cleanup.observe(document.body, { childList: true, subtree: true });
  };

  const scan = (node: Node): void => {
    if (!(node instanceof Element)) return;
    if (node.matches(SANDBOX_ELEMENT)) adopt(node);
    for (const element of node.querySelectorAll(SANDBOX_ELEMENT)) adopt(element);
  };
  new MutationObserver((records) => {
    for (const record of records) for (const node of record.addedNodes) scan(node);
  }).observe(document.documentElement, { childList: true, subtree: true });

  // 渲染层重新加载等同 Electron owner webContents 销毁：释放上一页面留下的全部登记。
  void callSandbox("resetOwner", {}).catch(() => undefined);

  return {
    supportsRetainedMove: supportsMove,
    getOwnerWebContentsId: () => Promise.resolve(OWNER_WEB_CONTENTS_ID),
    async disposeSandbox(sandboxId, initId) {
      await callSandbox("dispose", initId === undefined ? { sandboxId } : { sandboxId, initId });
    },
    consumeUserGesture(sandboxId) {
      const now = Date.now();
      const focused = focusedAt.get(sandboxId);
      const active = sandboxForActiveElement()?.sandboxId === sandboxId;
      const activation = (navigator as Navigator & { userActivation?: { isActive: boolean } }).userActivation;
      const consumed = consumedAt.get(sandboxId);
      const ok =
        active &&
        activation?.isActive === true &&
        focused !== undefined &&
        now - focused <= USER_GESTURE_TTL_MS &&
        (consumed === undefined || consumed < focused);
      if (ok) consumedAt.set(sandboxId, now);
      return Promise.resolve(ok);
    },
    onPorts(handler) {
      handlers.add(handler);
      return () => handlers.delete(handler);
    }
  };
}
