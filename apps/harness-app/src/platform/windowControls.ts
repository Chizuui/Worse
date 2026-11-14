import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  DesktopCommandIds,
  type DesktopCommandId,
  type DesktopWindowChromeState,
} from "@zcode/shared";
// The shared UI logger (packages/ui/src/logger.ts); @zcode/ui does not re-export it.
import { logger } from "@/logger.js";

/**
 * Window chrome for the frameless Harness window.
 *
 * The shared UI draws its own minimize/maximize/close buttons and calls
 * `executeDesktopCommand`; this module maps those commands onto the current
 * Tauri window. `close()` emits `CloseRequested`, so the Rust shell runs its
 * graceful Host shutdown before the window goes away.
 */

const windowCommands: Partial<Record<DesktopCommandId, () => Promise<void>>> = {
  [DesktopCommandIds.MinimizeWindow]: () => getCurrentWindow().minimize(),
  [DesktopCommandIds.ToggleMaximizeWindow]: () => getCurrentWindow().toggleMaximize(),
  [DesktopCommandIds.CloseWindow]: () => getCurrentWindow().close(),
};

/** Runs a window command; commands the shell does not implement yet resolve as no-ops. */
export function executeWindowCommand(command: DesktopCommandId): Promise<void> {
  const run = windowCommands[command];
  if (run === undefined) {
    logger.debug("desktop command is not available in Harness", { command });
    return Promise.resolve();
  }
  return run();
}

/** Toggles maximize; used by the title-bar double click. */
export function toggleMaximizeWindow(): Promise<void> {
  return getCurrentWindow().toggleMaximize();
}

async function readChromeState(): Promise<DesktopWindowChromeState> {
  return {
    isMaximized: await getCurrentWindow().isMaximized(),
    macOSMajorVersion: null,
    supportsNativeRoundedCorners: false,
  };
}

/** Reads the current maximized state for the self-drawn window buttons. */
export function getWindowChromeState(): Promise<DesktopWindowChromeState> {
  return readChromeState();
}

/**
 * Subscribes to maximized-state changes.
 *
 * Tauri has no dedicated maximize event; maximize, restore and snap all resize
 * the window, so every resize re-reads the state and only changes are emitted.
 */
export function onWindowChromeStateChanged(
  handler: (state: DesktopWindowChromeState) => void,
): () => void {
  let disposed = false;
  let lastMaximized: boolean | null = null;
  let unlisten: (() => void) | null = null;

  const publish = (): void => {
    readChromeState()
      .then((state) => {
        if (disposed || state.isMaximized === lastMaximized) {
          return;
        }
        lastMaximized = state.isMaximized;
        handler(state);
      })
      .catch((error: unknown) => logger.warn("failed to read the window maximized state", { error }));
  };

  getCurrentWindow()
    .onResized(publish)
    .then((stop) => {
      if (disposed) {
        stop();
      } else {
        unlisten = stop;
      }
    })
    .catch((error: unknown) => logger.warn("failed to subscribe to window resize events", { error }));

  return () => {
    disposed = true;
    unlisten?.();
    unlisten = null;
  };
}
