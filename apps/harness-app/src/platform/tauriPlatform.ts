import { DesktopCommandIds, type IPlatformService } from "@zcode/shared";
import * as commands from "./commands.js";
import { createRemoteWorkspacePlatform } from "./remoteWorkspacePlatform.js";
import { createDesktopWebRemoteControl, createMobileWebRemoteControl } from "./webRemoteControl.js";
import { createDesktopMcpTempAttachment } from "./mcpTempAttachment.js";
import { createHarnessPluginSandboxPlatform, harnessPluginSandboxSupported } from "./pluginSandbox.js";
import { checkForUpdates, createDesktopUpdatePlatform, createMobileUpdatePlatform } from "./updatePlatform.js";
import {
  executeWindowCommand,
  getWindowChromeState,
  onWindowChromeStateChanged,
} from "./windowControls.js";

const MOBILE_PLATFORMS = new Set(["android", "ios"]);

interface TauriEnvironment {
  readonly TAURI_ENV_PLATFORM?: string;
  readonly DEV?: boolean;
}

const tauriEnvironment = (import.meta as ImportMeta & { readonly env?: TauriEnvironment }).env;

/** True when the shell was compiled for a phone remote-control surface. */
export const isMobileSurface = MOBILE_PLATFORMS.has(tauriEnvironment?.TAURI_ENV_PLATFORM ?? "");

const noop = (): void => {};

const noopDisposer = (): (() => void) => noop;

function describeError(error: unknown): string {
  if (
    typeof error === "object" &&
    error !== null &&
    "message" in error &&
    typeof error.message === "string"
  ) {
    return error.message;
  }
  return String(error);
}

function readDeviceId(): string {
  const parts = [
    globalThis.navigator.platform ?? "",
    String(globalThis.screen?.width ?? ""),
    String(globalThis.screen?.height ?? ""),
    String(globalThis.screen?.colorDepth ?? ""),
  ];
  return parts.filter((part) => part.length > 0).join("|");
}

/**
 * Builds the Tauri-backed platform adapter for the shared UI.
 *
 * Native operations go through the typed command wrappers, so the webview
 * never calls Tauri APIs outside this module. Behaviors the Rust shell does not
 * implement yet return explicit safe fallbacks instead of pretending to work.
 */
export function createTauriPlatform(): IPlatformService {
  return {
    canOpenLocalFiles: !isMobileSurface,
    canSelectFilePath: !isMobileSurface,
    isLocalDevelopmentRuntime: tauriEnvironment?.DEV === true,
    selectDirectory: async () => (await commands.selectDirectory()).path,
    selectFile: async () => (await commands.selectFile()).path,
    activateOrSetWorkspace: () => Promise.resolve({ activated: false }),
    // Phase 4: SSH / WSL / Docker remote workspaces through the sidecar's remote session manager.
    ...createRemoteWorkspacePlatform(),
    // Phase 4: desktop drives the sidecar's remote-control owner; mobile is a remote client only.
    ...(isMobileSurface ? createMobileWebRemoteControl() : createDesktopWebRemoteControl()),
    // Phase 4 fix B: long-paste temp attachments and the MCP user directory (Main's handlers in the sidecar).
    ...(isMobileSurface ? {} : createDesktopMcpTempAttachment()),
    // Phase 5 (plugins): MCP App / plugin UI / Gen UI sandbox frames served by the host-node sandbox owner
    // (Windows only for now; absent port = ZCode's own "unsupported" UI path).
    ...(isMobileSurface || !harnessPluginSandboxSupported() ? {} : { pluginSandbox: createHarnessPluginSandboxPlatform() }),
    openExternal: (url) => {
      void commands.openExternal(url).catch(() => {
        // A rejected invoke means the shell denied or could not open the URL.
      });
    },
    openFeedback: () => Promise.resolve(),
    openCommunity: () => Promise.resolve(),
    canOpenCommunity: () => Promise.resolve(false),
    openInFileManager: async (path) => {
      try {
        await commands.revealPath(path);
        return { success: true };
      } catch (error) {
        return { success: false, error: describeError(error) };
      }
    },
    openExternalFile: () =>
      Promise.resolve({
        success: false,
        error: "Opening files with the default application is not available yet.",
      }),
    registerOAuthState: noop,
    onOAuthCallback: () => noopDisposer(),
    onPaymentCallback: () => noopDisposer(),
    notifyRendererReady: noop,
    showTaskNotification: (payload) => {
      if (document.hasFocus()) {
        return;
      }
      if (
        typeof window.Notification === "undefined" ||
        window.Notification.permission !== "granted"
      ) {
        return;
      }
      try {
        new window.Notification(payload.title, { body: payload.body, silent: true });
      } catch {
        // 某些 WebView 在权限已授予时仍会拒绝构造通知，忽略即可，不影响任务流程。
      }
    },
    reportTelemetryEvent: () => Promise.resolve(),
    reportArmsCustomEvent: () => Promise.resolve(),
    syncWindowTabs: noop,
    syncWindowUnreadCount: noop,
    syncActiveTaskSession: noop,
    onFocusTab: () => noopDisposer(),
    onNewTab: () => noopDisposer(),
    onNewTask: () => noopDisposer(),
    onOpenWorkspace: () => noopDisposer(),
    onWindowFullscreenChanged: () => noopDisposer(),
    // Frameless window: the UI-drawn buttons and maximized state map onto the Tauri window (desktop only).
    ...(isMobileSurface
      ? {}
      : {
          getDesktopWindowChromeState: getWindowChromeState,
          onDesktopWindowChromeStateChanged: onWindowChromeStateChanged,
        }),
    onTaskNotificationClick: () => noopDisposer(),
    exportLogs: () =>
      Promise.resolve({ success: false, error: "Log export is not available in Harness yet." }),
    // Phase 6 (updater B): the shell-owned auto-updater; phones have none.
    ...(isMobileSurface ? createMobileUpdatePlatform() : createDesktopUpdatePlatform()),
    getInstalledEditors: () => Promise.resolve([]),
    openInEditor: () =>
      Promise.resolve({
        success: false,
        error: "External editors are not available in Harness yet.",
      }),
    executeDesktopCommand: (command) => {
      if (isMobileSurface) {
        return Promise.resolve();
      }
      // Phase 6 (updater B): menu / caption "Check for Updates" runs a manual check.
      return command === DesktopCommandIds.CheckForUpdates ? checkForUpdates() : executeWindowCommand(command);
    },
    setApplicationLocale: () => Promise.resolve(),
    setTitleBarTheme: () => Promise.resolve(),
    getDeviceId: readDeviceId,
  };
}