// Harness relay shell for the plugin UI sandbox (spec docs/specs/harness/phase-5-plugins.md, 4.5).
//
// ZCode's shell (renderer/src/plugin-sandbox/main.ts) only accepts `init` from its own preload
// (`event.source === window`). In Harness the shell is an <iframe> inside the renderer, so this
// file replaces only that DOM wiring and reuses ZCode's relay logic (`relay.ts`) unchanged:
// - `init` + exactly one MessagePort is accepted only from `window.parent` with the embedder
//   origin that host-node wrote into the <meta name="harness-sandbox"> tag;
// - the iframe `allow` comes from that meta tag (derived from the registry record), never from
//   the message;
// - plugin frame messages are accepted only from the plugin frame with the plugin origin.
// Bundled for the browser by `scripts/build-pluginSandbox-vendor.mjs` into
// `dist-vendor/pluginSandboxShell.js` and served at `/instance/<id>/__harness__/shell.js`.
import { createPluginSandboxRelay } from "../../../../packages/desktop/src/renderer/src/plugin-sandbox/relay.ts";

interface HarnessShellConfig {
  sandboxId: string;
  embedderOrigin: string;
  pluginOrigin: string;
  pluginUrl: string;
  allow: string;
}

function readConfig(): HarnessShellConfig | null {
  const raw = document.querySelector('meta[name="harness-sandbox"]')?.getAttribute("content");
  if (!raw) return null;
  try {
    const value = JSON.parse(raw) as Partial<HarnessShellConfig>;
    if (
      typeof value.sandboxId !== "string" ||
      typeof value.embedderOrigin !== "string" ||
      typeof value.pluginOrigin !== "string" ||
      typeof value.pluginUrl !== "string" ||
      typeof value.allow !== "string"
    ) {
      return null;
    }
    // 只信任与自身路径一致的配置：shell 文档路径 /instance/<id>/ 与 meta 的 sandboxId 必须相同。
    if (window.location.pathname.split("/")[2] !== value.sandboxId) return null;
    return value as HarnessShellConfig;
  } catch {
    return null;
  }
}

const root = document.getElementById("root") ?? document.body;
const errorNode = document.getElementById("error");
const config = readConfig();
let frameElement: HTMLIFrameElement | null = null;

function showError(message: string): void {
  if (errorNode) {
    errorNode.textContent = message;
    errorNode.hidden = false;
  }
}

function notifyParent(event: "load" | "navigate"): void {
  if (!config) return;
  window.parent.postMessage(
    { type: "harness-sandbox-frame", sandboxId: config.sandboxId, event },
    config.embedderOrigin
  );
}

if (!config) {
  showError(`Plugin sandbox: unexpected shell document ${window.location.origin}`);
} else {
  const relay = createPluginSandboxRelay({
    sandboxId: config.sandboxId,
    pluginOrigin: config.pluginOrigin,
    pluginUrl: config.pluginUrl,
    createPluginFrame(src, allow) {
      const iframe = document.createElement("iframe");
      // 与 ZCode shell 相同：allow-scripts + allow-same-origin + allow-forms，无顶层导航、无弹窗。
      iframe.setAttribute("sandbox", "allow-scripts allow-same-origin allow-forms");
      if (allow) iframe.setAttribute("allow", allow);
      iframe.setAttribute("referrerpolicy", "no-referrer");
      iframe.title = "plugin";
      let loads = 0;
      iframe.addEventListener("load", () => {
        loads += 1;
        // 首次加载 = 插件文档就绪；之后的 load 是插件 frame 自身重载，宿主按崩溃处理（与 ZCode 一致）。
        notifyParent(loads === 1 ? "load" : "navigate");
      });
      iframe.addEventListener("error", () => console.warn(`[plugin-sandbox-shell] frame error src=${src}`));
      iframe.src = src;
      root.appendChild(iframe);
      frameElement = iframe;
      return {
        postMessage(message, targetOrigin) {
          iframe.contentWindow?.postMessage(message, targetOrigin);
        },
        remove() {
          iframe.remove();
          if (frameElement === iframe) frameElement = null;
        }
      };
    },
    showError,
    log: (message) => console.warn(message)
  });

  window.addEventListener("message", (event) => {
    if (event.source === window.parent && event.origin === config.embedderOrigin) {
      const data = event.data as { type?: unknown } | null;
      if (typeof data !== "object" || data === null || data.type !== "init") return;
      relay.handleInit(
        { ...data, allow: config.allow },
        event.ports as unknown as Parameters<typeof relay.handleInit>[1]
      );
      return;
    }
    if (frameElement && event.source === frameElement.contentWindow && event.origin === config.pluginOrigin) {
      relay.handleFrameMessage(event.data);
      return;
    }
    console.warn(
      `[plugin-sandbox-shell] dropped message origin=${event.origin} expected=${config.pluginOrigin} fromFrame=${String(
        Boolean(frameElement && event.source === frameElement.contentWindow)
      )}`
    );
  });

  window.addEventListener("pagehide", () => relay.dispose());
}
