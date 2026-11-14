import { invoke } from "@tauri-apps/api/core";
import type { IPlatformService } from "@zcode/shared";

/**
 * Desktop members that Electron Main answered in ZCode: long-paste temporary text attachments and
 * the MCP user directory (spec `Worse/docs/specs/harness/phase-4-web-remote-control.md`,
 * section 12).
 *
 * The host-node sidecar runs ZCode's own Main handlers (`mcpTempAttachmentHandlers.ts`), the same
 * object that answers the phone's `platform-request`s, so the desktop and the phone share one
 * owner and one set of files. This module only forwards; it keeps no state.
 */

type PlatformRequestMethod =
  | "createTempTextAttachment"
  | "loadMcpFromUserDirectory"
  | "saveMcpToUserDirectory"
  | "migrateLegacyCommonMcp";

function platformRequest<T>(method: PlatformRequestMethod, args: unknown): Promise<T> {
  return invoke<T>("remote_control_call", { method: "platformRequest", params: { method, args } });
}

type McpTempAttachmentMembers = Required<
  Pick<
    IPlatformService,
    | "createTempTextAttachment"
    | "loadMcpFromUserDirectory"
    | "saveMcpToUserDirectory"
    | "migrateLegacyCommonMcp"
  >
>;

export function createDesktopMcpTempAttachment(): McpTempAttachmentMembers {
  return {
    createTempTextAttachment: (payload) => platformRequest("createTempTextAttachment", payload),
    loadMcpFromUserDirectory: (payload) => platformRequest("loadMcpFromUserDirectory", payload),
    saveMcpToUserDirectory: (payload) => platformRequest("saveMcpToUserDirectory", payload),
    migrateLegacyCommonMcp: (payload) => platformRequest("migrateLegacyCommonMcp", payload),
  };
}
