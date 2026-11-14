import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  IPlatformService,
  PostUpdateReleaseNotesPayload,
  UpdateCheckResultPayload,
  UpdateStatePayload,
} from "@zcode/shared";
// The shared UI logger (packages/ui/src/logger.ts); @zcode/ui does not re-export it.
import { logger } from "@/logger.js";

/**
 * Desktop auto-update (Phase 6, spec `Worse/docs/specs/harness/phase-6-updater.md`).
 *
 * The Rust shell owns the update state machine, the signed download and the
 * Host-draining install. This module only maps the `IPlatformService` update
 * members onto the `updater_call` command and the `harness://updater/*`
 * events; it keeps no update state of its own.
 */

type UpdaterMethod =
  | "getState"
  | "check"
  | "download"
  | "cancelDownload"
  | "skipVersion"
  | "quitAndInstall"
  | "getPreferences"
  | "setAutoDownload"
  | "takeReleaseNotes"
  | "ackReleaseNotes";

const EVENTS = {
  state: "harness://updater/state",
  checkResult: "harness://updater/check-result",
  ready: "harness://updater/ready",
} as const;

function call<T>(method: UpdaterMethod, params: Record<string, unknown> = {}): Promise<T> {
  return invoke<T>("updater_call", { method, params });
}

function describeError(error: unknown): string {
  if (typeof error === "object" && error !== null && "message" in error) {
    return String(error.message);
  }
  return String(error);
}

function subscribe<T>(event: string, handler: (payload: T) => void): () => void {
  let disposed = false;
  let unlisten: UnlistenFn | null = null;
  listen<T>(event, (message) => handler(message.payload))
    .then((stop) => {
      if (disposed) {
        stop();
      } else {
        unlisten = stop;
      }
    })
    .catch((error: unknown) =>
      logger.warn("failed to subscribe to an updater event", { event, error: describeError(error) }),
    );
  return () => {
    disposed = true;
    unlisten?.();
    unlisten = null;
  };
}

type UpdateMembers = Pick<
  IPlatformService,
  | "onUpdateReady"
  | "onUpdateCheckResult"
  | "onUpdateStateChanged"
  | "getUpdateState"
  | "downloadUpdate"
  | "cancelUpdateDownload"
  | "getAutoUpdatePreferences"
  | "setAutoDownloadAndInstallUpdates"
  | "skipUpdateVersion"
  | "onPostUpdateReleaseNotes"
  | "acknowledgePostUpdateReleaseNotes"
  | "quitAndInstallUpdate"
>;

/** Update members for the desktop shell. */
export function createDesktopUpdatePlatform(): UpdateMembers {
  return {
    onUpdateReady: (callback) => subscribe<string>(EVENTS.ready, callback),
    onUpdateCheckResult: (callback) =>
      subscribe<UpdateCheckResultPayload>(EVENTS.checkResult, callback),
    onUpdateStateChanged: (callback) => subscribe<UpdateStatePayload>(EVENTS.state, callback),
    getUpdateState: () => call<UpdateStatePayload>("getState"),
    downloadUpdate: () => call<void>("download"),
    cancelUpdateDownload: () => call<void>("cancelDownload"),
    getAutoUpdatePreferences: () =>
      call<{ autoDownloadAndInstallUpdates: boolean }>("getPreferences"),
    setAutoDownloadAndInstallUpdates: (enabled) => call<void>("setAutoDownload", { enabled }),
    skipUpdateVersion: (version) => call<void>("skipVersion", { version }),
    onPostUpdateReleaseNotes: (callback) => {
      // 发布说明只在启动后取一次（升级后首个窗口），由 ack 清除；与 ZCode 主进程推送语义一致。
      let disposed = false;
      call<PostUpdateReleaseNotesPayload | null>("takeReleaseNotes")
        .then((notes) => {
          if (!disposed && notes) {
            callback(notes);
          }
        })
        .catch((error: unknown) =>
          logger.warn("failed to read post-update release notes", { error: describeError(error) }),
        );
      return () => {
        disposed = true;
      };
    },
    acknowledgePostUpdateReleaseNotes: (version) => call<void>("ackReleaseNotes", { version }),
    // On Windows the shell exits inside this call once the installer starts.
    quitAndInstallUpdate: () => call<void>("quitAndInstall"),
  };
}

/** Menu / caption "Check for Updates": a manual check reported through check-result toasts. */
export function checkForUpdates(): Promise<void> {
  return call<void>("check", { manual: true });
}

/** Phone remote-control clients are store-distributed; they have no updater. */
export function createMobileUpdatePlatform(): UpdateMembers {
  const noopDisposer = (): (() => void) => () => {};
  return {
    onUpdateReady: noopDisposer,
    onUpdateCheckResult: noopDisposer,
    downloadUpdate: () => Promise.resolve(),
    cancelUpdateDownload: () => Promise.resolve(),
    skipUpdateVersion: () => Promise.resolve(),
    onPostUpdateReleaseNotes: noopDisposer,
    acknowledgePostUpdateReleaseNotes: () => Promise.resolve(),
    quitAndInstallUpdate: () => Promise.resolve(),
  };
}
