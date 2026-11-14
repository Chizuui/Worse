// Harness owner of the plugin UI sandbox (spec docs/specs/harness/phase-5-plugins.md).
//
// ZCode keeps sandbox records in Electron Main (`main/pluginSandbox/`). Harness has no Main, so
// the same registry, CSP builders and protocol handler (bundled unchanged into
// dist-vendor/pluginSandboxRuntime.js) run here, and this module is their single owner:
// - Host `plugin-sandbox-register-request` messages are intercepted on the parent port before
//   they reach the Rust shell; the reply carries the Harness shell URL (spec 4.1, 4.3);
// - the Rust `hsandbox` URI scheme forwards every sandbox request here as
//   `harness-web-remote-sandbox-call {method:"fetch"}`; the ZCode handler answers it and only the
//   origin tokens of the CSP are rewritten (spec 4.4);
// - the renderer disposes records through the same channel.
// Rust keeps no sandbox state; it only forwards bytes.
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { log } from "./log.ts";

type Json = Record<string, unknown>;

export const SANDBOX_CALL = "harness-web-remote-sandbox-call";
export const SANDBOX_RESULT = "harness-web-remote-sandbox-result";
const REGISTER_REQUEST = "plugin-sandbox-register-request";
const REGISTER_RESULT = "plugin-sandbox-register-result";

/** One Harness window owns one Host; `getOwnerWebContentsId()` in the renderer returns this. */
export const HARNESS_OWNER_WEB_CONTENTS_ID = 1;
export const HARNESS_SANDBOX_SCHEME = "hsandbox";
const ZCODE_SANDBOX_SCHEME = "zcode-sandbox";
/** Virtual renderer dir: ZCode's handler serves the shell HTML and `/assets/*` from here. */
const SHELL_HTML_FILE = "plugin-sandbox.html";
const SHELL_SCRIPT_ASSET = "assets/harness-shell.js";
const SHELL_CONFIG_PLACEHOLDER = "__HARNESS_SANDBOX_CONFIG__";
const EMBEDDER_ORIGIN = /^(?:https?|tauri):\/\/[a-z0-9.-]+(?::\d{1,5})?$/;
const H32 = /^[a-f0-9]{32}$/;
/** Keeps one reply line well under the 16 MiB sidecar line limit (MCP UI resources are ≤ 8 MiB). */
const MAX_BODY_BASE64_CHARS = 14 * 1024 * 1024;

/** Same document and styles as ZCode's renderer/plugin-sandbox.html; the script is the Harness shell. */
const SHELL_HTML = `<!doctype html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <meta name="harness-sandbox" content="${SHELL_CONFIG_PLACEHOLDER}" />
    <title>ZCode Plugin Sandbox</title>
    <style>
      html, body, #root { margin: 0; padding: 0; width: 100%; height: 100%; overflow: hidden; background: transparent; }
      #root iframe { display: block; border: 0; width: 100%; height: 100%; }
      #error { font: 12px system-ui, sans-serif; color: #8a4b08; padding: 12px; }
    </style>
  </head>
  <body>
    <div id="root"></div>
    <div id="error" hidden></div>
    <script defer src="${SHELL_SCRIPT_ASSET}"></script>
  </body>
</html>
`;

interface SandboxRecord {
  sandboxId: string;
  initId: number;
  contentKind?: "gen-ui";
  permissions?: string[];
  instance: { appIdentity: string };
}

interface SandboxHandle {
  instance: { appIdentity: string };
  sandboxId: string;
  initId: number;
  shellUrl: string;
  partition: string;
  resourceMeta?: unknown;
}

interface SandboxRegistry {
  register(input: unknown): SandboxHandle;
  get(sandboxId: string): SandboxRecord | null;
  dispose(sandboxId: string, initId?: number): void;
  disposeOwner(ownerWebContentsId: number): void;
  pin(sandboxId: string): void;
  size(): number;
  clear(): void;
}

/** The subset of dist-vendor/pluginSandboxRuntime.js this module uses. */
export interface PluginSandboxVendor {
  createPluginSandboxRegistry(options?: Json): SandboxRegistry;
  createPluginSandboxProtocolHandler(options: {
    registry: SandboxRegistry;
    assets: { rendererDir: string; aliasDir: string };
    readAsset?: (absolutePath: string) => Promise<Uint8Array>;
    logger?: { warn: (...args: unknown[]) => void; info?: (...args: unknown[]) => void };
  }): (request: Request) => Promise<Response>;
  buildMcpAppsIframeAllow(permissions: readonly string[] | undefined): string;
  hostPluginSandboxRegisterRequestResponseSchema: {
    safeParse(value: unknown): { success: true; data: Json & { requestId: string; ownerWebContentsId: number } } | { success: false };
  };
}

export interface PluginSandboxOwnerOptions {
  /** Delivers a message to the ZCode Host as if Main sent it (`parentPort` listeners). */
  postToHost(message: Json): void;
  /** Writes a `host` envelope to the Rust shell. */
  postToShell(message: Json): void;
  /** `packages/desktop/out/plugin-sandbox` (alias script, Gen UI runtime and vendor files). */
  assetsDir: string;
  /** Directory holding `pluginSandboxShell.js`; defaults to host-node's dist-vendor/. */
  vendorDir?: string;
  platform?: NodeJS.Platform;
  loadVendor?: () => Promise<PluginSandboxVendor>;
}

export interface PluginSandboxOwner {
  /** Handles an outbound Host message; true when it was a sandbox request (not for the shell). */
  interceptHostMessage(message: unknown): boolean;
  /** Handles one `harness-web-remote-sandbox-call` from the shell. Never throws. */
  handleShellMessage(message: Json): void;
  dispose(): void;
}

const defaultVendorDir = (): string =>
  // src/ (tests) and dist/ (runtime) sit at the same depth as dist-vendor/.
  path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "dist-vendor");

async function loadBundledVendor(): Promise<PluginSandboxVendor> {
  const bundle = path.join(defaultVendorDir(), "pluginSandboxRuntime.js");
  return (await import(pathToFileURL(bundle).href)) as PluginSandboxVendor;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function isRecord(value: unknown): value is Json {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Origin of a Harness sandbox document as the webview sees it (WebView2 maps custom schemes to http). */
export function harnessSandboxOrigin(kind: "shell" | "plugin", h32: string, platform: NodeJS.Platform): string {
  return platform === "win32"
    ? `http://${HARNESS_SANDBOX_SCHEME}.${kind}-${h32}.localhost`
    : `${HARNESS_SANDBOX_SCHEME}://${kind}-${h32}.localhost`;
}

/** Rewrites single CSP directives by name; every other directive keeps its exact bytes. */
export function rewriteCspDirectives(csp: string, replace: Record<string, string | null>, append: string[] = []): string {
  const directives = csp.split("; ").map((directive) => {
    const name = directive.split(" ", 1)[0] ?? "";
    return Object.hasOwn(replace, name) ? replace[name] : directive;
  });
  return [...directives.filter((directive): directive is string => directive !== null), ...append].join("; ");
}

interface ParsedHarnessUrl {
  kind: "shell" | "plugin";
  h32: string;
  sandboxId: string;
  /** Path and query, starting with `/instance/`. */
  rest: string;
}

/** Parses `hsandbox://<kind>-<h32>.localhost/instance/<id>/...`; anything else is null (→ 404). */
export function parseHarnessSandboxUrl(raw: string): ParsedHarnessUrl | null {
  let url: URL;
  try {
    url = new URL(raw);
  } catch {
    return null;
  }
  if (url.protocol !== `${HARNESS_SANDBOX_SCHEME}:` || url.port || url.username || url.password) return null;
  const match = /^(shell|plugin)-([a-f0-9]+)\.localhost$/.exec(url.hostname);
  if (!match || !H32.test(match[2] ?? "")) return null;
  const segments = url.pathname.split("/");
  if (segments[1] !== "instance" || !segments[2]) return null;
  return {
    kind: match[1] as "shell" | "plugin",
    h32: match[2] as string,
    sandboxId: segments[2],
    rest: `${url.pathname}${url.search}`
  };
}

/**
 * Creates the owner for the launcher and routes the Host's outbound sandbox requests to it.
 * Wraps `shim.postMessage` the same way the browser owner does, so both owners compose.
 */
export function installPluginSandbox(input: {
  shim: { postMessage(message: unknown): void; emitHostMessage(message: unknown, ports: unknown[]): void };
  postToShell(message: Json): void;
  hostEntry: string;
}): PluginSandboxOwner {
  const owner = createPluginSandboxOwner({
    postToHost: (message) => input.shim.emitHostMessage(message, []),
    postToShell: input.postToShell,
    // packages/desktop/out/host/index.js → packages/desktop/out/plugin-sandbox (dev and staged layouts).
    assetsDir: path.resolve(path.dirname(input.hostEntry), "..", "plugin-sandbox")
  });
  const forward = input.shim.postMessage;
  input.shim.postMessage = (message: unknown) => {
    if (!owner.interceptHostMessage(message)) forward(message);
  };
  return owner;
}

export function createPluginSandboxOwner(options: PluginSandboxOwnerOptions): PluginSandboxOwner {
  const platform = options.platform ?? process.platform;
  const vendorDir = options.vendorDir ?? defaultVendorDir();
  const shellRoot = path.join(vendorDir, "harness-plugin-sandbox-shell");
  let runtimePromise:
    | Promise<{ vendor: PluginSandboxVendor; registry: SandboxRegistry; handle: (request: Request) => Promise<Response> }>
    | undefined;
  let genUiVendorUrls: Promise<Array<{ url: string; file: string }>> | undefined;
  const pinned = new Set<string>();
  let disposed = false;

  const readAsset = async (absolutePath: string): Promise<Uint8Array> => {
    const normalized = path.normalize(absolutePath);
    if (normalized === path.join(shellRoot, SHELL_HTML_FILE)) return new TextEncoder().encode(SHELL_HTML);
    if (normalized === path.join(shellRoot, SHELL_SCRIPT_ASSET)) {
      return readFile(path.join(vendorDir, "pluginSandboxShell.js"));
    }
    // 虚拟 shell 目录下除上述两个文件外一律 404，不能落到磁盘上的同名路径。
    if (normalized.startsWith(shellRoot + path.sep)) throw new Error("not found");
    return readFile(normalized);
  };

  const runtime = () => {
    runtimePromise ??= (options.loadVendor ?? loadBundledVendor)().then((vendor) => {
      const registry = vendor.createPluginSandboxRegistry({
        onDispose: (record: SandboxRecord) => pinned.delete(record.sandboxId)
      });
      const handle = vendor.createPluginSandboxProtocolHandler({
        registry,
        assets: { rendererDir: shellRoot, aliasDir: options.assetsDir },
        readAsset,
        logger: {
          warn: (...args: unknown[]) => log("warn", args.map(String).join(" ")),
          info: (...args: unknown[]) => log("debug", args.map(String).join(" "))
        }
      });
      return { vendor, registry, handle };
    });
    runtimePromise.catch(() => {
      runtimePromise = undefined;
    });
    return runtimePromise;
  };

  const readGenUiVendorUrls = () => {
    genUiVendorUrls ??= readFile(path.join(options.assetsDir, "vendor/manifest.json"), "utf8")
      .then((text) => {
        const manifest = JSON.parse(text) as { resources?: Array<{ url?: unknown; file?: unknown }> };
        return (manifest.resources ?? []).flatMap((item) =>
          typeof item.url === "string" && typeof item.file === "string" ? [{ url: item.url, file: item.file }] : []
        );
      })
      .catch(() => []);
    return genUiVendorUrls;
  };

  const replyRegister = (requestId: string, payload: Json): void => {
    options.postToHost({ type: REGISTER_RESULT, requestId, ...payload });
  };

  const register = async (message: Json): Promise<void> => {
    const requestId = typeof message.requestId === "string" ? message.requestId : "";
    try {
      const { vendor, registry } = await runtime();
      const parsed = vendor.hostPluginSandboxRegisterRequestResponseSchema.safeParse(message);
      if (!parsed.success) throw new Error("Invalid plugin sandbox registration request.");
      // 与 Main 相同：只接受本窗口（owner id 固定为 1）的登记。
      if (parsed.data.ownerWebContentsId !== HARNESS_OWNER_WEB_CONTENTS_ID) {
        throw new Error("Plugin UI sandbox is unavailable.");
      }
      const { type: _type, requestId: _requestId, ...input } = parsed.data;
      const handle = registry.register(input);
      const h32 = handle.instance.appIdentity.slice(0, 32);
      const shellUrl = `${harnessSandboxOrigin("shell", h32, platform)}/instance/${handle.sandboxId}/#init=${handle.initId}`;
      log("info", "plugin sandbox registered", { sandboxId: handle.sandboxId, initId: handle.initId });
      replyRegister(requestId, { ok: true, ...handle, shellUrl });
    } catch (error) {
      replyRegister(requestId, { ok: false, error: errorMessage(error) });
    }
  };

  const fetchSandbox = async (params: Json): Promise<Json> => {
    const url = typeof params.url === "string" ? params.url : "";
    const method = typeof params.method === "string" ? params.method : "GET";
    const embedderOrigin = typeof params.embedderOrigin === "string" ? params.embedderOrigin : "";
    const notFound = { status: 404, headers: [["Cache-Control", "no-store"]], bodyBase64: "" };
    if (!EMBEDDER_ORIGIN.test(embedderOrigin)) throw new Error("invalid embedder origin");
    const target = parseHarnessSandboxUrl(url);
    if (!target) return notFound;
    const { registry, handle, vendor } = await runtime();
    const record = registry.get(target.sandboxId);
    // 修复依据：Harness 主机名只携带 appIdentity 前 32 位；必须与登记的完整身份前缀一致，否则跨实例读取。
    if (!record || !record.instance.appIdentity.startsWith(target.h32)) return notFound;
    const zcodeUrl = `${ZCODE_SANDBOX_SCHEME}://${target.kind}-${record.instance.appIdentity}${target.rest}`;
    const response = await handle(new Request(zcodeUrl, { method }));
    const headers = new Headers(response.headers);
    let body = new Uint8Array(await response.arrayBuffer());
    const shellOrigin = harnessSandboxOrigin("shell", target.h32, platform);
    const pluginOrigin = harnessSandboxOrigin("plugin", target.h32, platform);
    const csp = headers.get("Content-Security-Policy");
    if (csp !== null) {
      headers.set(
        "Content-Security-Policy",
        target.kind === "shell"
          ? rewriteCspDirectives(csp, { "frame-src": `frame-src ${pluginOrigin}` }, [`frame-ancestors ${embedderOrigin}`])
          : rewriteCspDirectives(csp, { "frame-ancestors": `frame-ancestors ${shellOrigin} ${embedderOrigin}` })
      );
    }
    const isDocument = response.status === 200 && (headers.get("Content-Type") ?? "").startsWith("text/html");
    if (isDocument && target.kind === "shell") {
      const config = JSON.stringify({
        sandboxId: record.sandboxId,
        embedderOrigin,
        pluginOrigin,
        pluginUrl: `${pluginOrigin}/instance/${record.sandboxId}/index.html`,
        allow: vendor.buildMcpAppsIframeAllow(record.permissions)
      }).replaceAll("&", "&amp;").replaceAll('"', "&quot;").replaceAll("<", "&lt;");
      body = new TextEncoder().encode(new TextDecoder().decode(body).replace(SHELL_CONFIG_PLACEHOLDER, config));
      // ZCode 在 guest attach 时 pin；Harness 在 shell 文档首次加载时 pin，活页面不因 TTL 过期。
      if (!pinned.has(record.sandboxId)) {
        pinned.add(record.sandboxId);
        registry.pin(record.sandboxId);
      }
    } else if (isDocument && record.contentKind === "gen-ui") {
      // ZCode 用 session.webRequest 把打包的 CDN 地址重定向到本地 vendor；Harness 无该拦截，改写文档中的同一组地址。
      let html = new TextDecoder().decode(body);
      for (const { url: cdnUrl, file } of await readGenUiVendorUrls()) {
        html = html.replaceAll(cdnUrl, `/instance/${record.sandboxId}/__zcode__/vendor/${file}`);
      }
      body = new TextEncoder().encode(html);
    }
    const bodyBase64 = Buffer.from(body).toString("base64");
    // 修复依据：stdout 单行上限 16 MiB（超限 shell 会判协议错误）；超大响应按 413 失败关闭，而不是拖垮 sidecar。
    if (bodyBase64.length > MAX_BODY_BASE64_CHARS) {
      log("warn", "plugin sandbox response too large", { sandboxId: record.sandboxId, bytes: body.byteLength });
      return { status: 413, headers: [["Cache-Control", "no-store"]], bodyBase64: "" };
    }
    return { status: response.status, headers: [...headers.entries()], bodyBase64 };
  };

  const invoke = async (method: string, params: Json): Promise<unknown> => {
    switch (method) {
      case "fetch":
        return fetchSandbox(params);
      case "dispose": {
        if (typeof params.sandboxId !== "string") throw new Error("sandboxId is required");
        const { registry } = await runtime();
        registry.dispose(params.sandboxId, typeof params.initId === "number" ? params.initId : undefined);
        return null;
      }
      case "resetOwner": {
        // 渲染层重新加载即旧窗口销毁：与 Main 的 owner destroyed 一致，释放其全部登记。
        const { registry } = await runtime();
        registry.disposeOwner(HARNESS_OWNER_WEB_CONTENTS_ID);
        return null;
      }
      case "stats": {
        const { registry } = await runtime();
        return { size: registry.size(), pinned: pinned.size };
      }
      default:
        throw new Error(`unknown plugin sandbox method: ${method}`);
    }
  };

  return {
    interceptHostMessage(message) {
      if (!isRecord(message) || message.type !== REGISTER_REQUEST) return false;
      if (disposed) {
        replyRegister(typeof message.requestId === "string" ? message.requestId : "", {
          ok: false,
          error: "Plugin UI sandbox is shutting down."
        });
        return true;
      }
      void register(message);
      return true;
    },
    handleShellMessage(message) {
      const id = typeof message.id === "string" ? message.id : undefined;
      const method = typeof message.method === "string" ? message.method : "";
      if (id === undefined) {
        log("warn", "ignored a malformed plugin sandbox call");
        return;
      }
      const reply = (result: { ok: true; value: unknown } | { ok: false; error: string }): void =>
        options.postToShell({ type: SANDBOX_RESULT, id, ...result });
      if (disposed) {
        reply({ ok: false, error: "Plugin UI sandbox is shutting down." });
        return;
      }
      invoke(method, isRecord(message.params) ? message.params : {}).then(
        (value) => reply({ ok: true, value: value ?? null }),
        (error: unknown) => reply({ ok: false, error: errorMessage(error) })
      );
    },
    dispose() {
      if (disposed) return;
      disposed = true;
      void runtimePromise?.then(({ registry }) => registry.clear()).catch(() => undefined);
    }
  };
}
