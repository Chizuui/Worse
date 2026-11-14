// Harness owner of the Electron Main platform handlers for temporary text attachments and the MCP
// user directory (spec docs/specs/harness/phase-4-web-remote-control.md, section 12).
//
// ZCode served these from Main for two callers: the desktop renderer (preload IPC) and the phone
// (`platform-request` through `webRemoteControlManager.platformHandlers`). Harness has no Main, so
// the same ZCode functions (bundled unchanged into dist-vendor/mcpTempAttachmentRuntime.js) run
// here, and both callers use the one object this module creates:
// - the vendored web remote-control manager, via `platformHandlers` in webRemoteHost.ts;
// - the desktop renderer, via `remote_control_call("platformRequest", { method, args })`.
//
// Paths are ZCode's own: temp files under `<data root>/.zcode/tmp/paste-attachments/<date>/`
// (`getZCodeDataRootDir()`), MCP config under `<HOME>/.zcode/cli/config.json` (and workspace
// `.zcode/config.json`). Nothing here deletes temp files; as in ZCode, the storage clean of the
// `toolOutputs` category removes them.
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { log } from "./log.ts";

type Json = Record<string, unknown>;

/** The subset of dist-vendor/mcpTempAttachmentRuntime.js this module uses. */
export interface McpTempAttachmentVendor {
  createTempTextAttachment(payload: unknown): Promise<unknown>;
  loadCliMcpFromUserDirectory(payload?: unknown): Promise<unknown>;
  saveCliMcpToUserDirectory(payload: unknown): Promise<void>;
  migrateLegacyCommonMcp(payload?: unknown): Promise<unknown>;
}

/** Method names shared with ZCode's `WebRemoteControlPlatformMethod` for these four handlers. */
export const MCP_TEMP_ATTACHMENT_METHODS = [
  "createTempTextAttachment",
  "loadMcpFromUserDirectory",
  "saveMcpToUserDirectory",
  "migrateLegacyCommonMcp"
] as const;

export type McpTempAttachmentMethod = (typeof MCP_TEMP_ATTACHMENT_METHODS)[number];

/** Same shapes as Main's `platformHandlers` entries for these methods. */
export interface McpTempAttachmentHandlers {
  createTempTextAttachment(payload: unknown): Promise<unknown>;
  loadMcpFromUserDirectory(payload?: unknown): Promise<unknown>;
  saveMcpToUserDirectory(payload: unknown): Promise<{ success: boolean; error?: string }>;
  migrateLegacyCommonMcp(payload?: unknown): Promise<unknown>;
}

const METHOD_SET: ReadonlySet<string> = new Set<string>(MCP_TEMP_ATTACHMENT_METHODS);

export function isMcpTempAttachmentMethod(method: unknown): method is McpTempAttachmentMethod {
  return typeof method === "string" && METHOD_SET.has(method);
}

async function loadBundledVendor(): Promise<McpTempAttachmentVendor> {
  // src/ (tests) and dist/ (runtime) sit at the same depth as dist-vendor/.
  const here = path.dirname(fileURLToPath(import.meta.url));
  const bundle = path.resolve(here, "..", "dist-vendor", "mcpTempAttachmentRuntime.js");
  return (await import(pathToFileURL(bundle).href)) as McpTempAttachmentVendor;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function createMcpTempAttachmentHandlers(
  loadVendor: () => Promise<McpTempAttachmentVendor> = loadBundledVendor
): McpTempAttachmentHandlers {
  let vendorPromise: Promise<McpTempAttachmentVendor> | undefined;
  const vendor = (): Promise<McpTempAttachmentVendor> => {
    vendorPromise ??= loadVendor().catch((error: unknown) => {
      vendorPromise = undefined;
      throw error;
    });
    return vendorPromise;
  };

  return {
    createTempTextAttachment: async (payload) => (await vendor()).createTempTextAttachment(payload),
    loadMcpFromUserDirectory: async (payload) => (await vendor()).loadCliMcpFromUserDirectory(payload),
    // 与 Main 的 IPC/手机 handler 一致：保存失败返回 {success:false, error}，而不是抛错。
    saveMcpToUserDirectory: async (payload) => {
      try {
        await (await vendor()).saveCliMcpToUserDirectory(payload);
        return { success: true };
      } catch (error) {
        const message = errorMessage(error);
        log("warn", "[mcp-user-directory] save failed", message);
        return { success: false, error: message };
      }
    },
    migrateLegacyCommonMcp: async (payload) => (await vendor()).migrateLegacyCommonMcp(payload)
  };
}

/**
 * Runs one desktop-renderer `platformRequest` (`{ method, args }`). The method must be one of the
 * four names above; anything else is rejected before any IO.
 */
export function runMcpTempAttachmentRequest(handlers: McpTempAttachmentHandlers, params: Json): Promise<unknown> {
  const method = params.method;
  if (!isMcpTempAttachmentMethod(method)) {
    return Promise.reject(new Error(`unsupported platform request: ${typeof method === "string" ? method : "<missing>"}`));
  }
  return handlers[method](params.args);
}
