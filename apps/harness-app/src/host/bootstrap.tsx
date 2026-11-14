import { useCallback } from "react";
import { createRoot } from "react-dom/client";
import {
  AppErrorBoundary,
  GlobalDatabaseStartupLoading,
  Root,
  ZCodeIntlProvider,
} from "@zcode/ui";
import { DesktopCommandIds, type IPlatformService } from "@zcode/shared";
import { logger } from "@/logger.js";
import { createTauriPlatform } from "../platform/tauriPlatform.js";
import { installWindowDragRegions } from "../platform/windowDrag.js";
import { HostBootFailure } from "./HostBootFailure.js";
import { HostConnectionLost } from "./HostConnectionLost.js";
import { useHostBoot } from "./useHostBoot.js";

const isMacDesktop = navigator.userAgent.includes("Mac");
const isWindowsDesktop = navigator.userAgent.includes("Windows");

/**
 * Desktop boot: admit the Host startup snapshot, attach the service port, then
 * render the shared application root against the Host-backed services.
 */
export function startHarnessDesktop(): void {
  const container = document.getElementById("root");
  if (container === null) {
    throw new Error("Harness root element is missing");
  }
  // The window is frameless: the UI's own title areas move it.
  installWindowDragRegions();
  createRoot(container).render(<HarnessDesktopBoot platform={createTauriPlatform()} />);
}

function HarnessDesktopBoot({ platform }: { platform: IPlatformService }) {
  const { boot, startupState, connectionLoss, retry, exit } = useHostBoot();
  const services = boot.phase === "ready" ? boot.services : null;

  const closeWindow = useCallback((): void => {
    platform
      .executeDesktopCommand(DesktopCommandIds.CloseWindow)
      .catch((error: unknown) => logger.warn("failed to close the window", { error }));
  }, [platform]);

  const copyDetails = useCallback(async (details: string): Promise<void> => {
    await navigator.clipboard.writeText(details);
  }, []);

  let content;
  if (boot.phase === "failed") {
    content = <HostBootFailure failure={boot.failure} />;
  } else if (services === null) {
    content = (
      <GlobalDatabaseStartupLoading
        state={startupState}
        onRetry={retry}
        onCopy={copyDetails}
        onExit={exit}
      />
    );
  } else {
    content = (
      <Root
        services={services}
        platform={platform}
        isDesktop
        isMacDesktop={isMacDesktop}
        isWindowsDesktop={isWindowsDesktop}
        restoreSession
        supportsSettings
        allowOpenWorkspace
        allowRemoteWorkspace
        supportsEmbeddedBrowser={false}
        preferDirectoryBrowser={false}
      />
    );
  }

  return (
    <AppErrorBoundary isDesktop isMacDesktop={isMacDesktop} isWindowsDesktop={isWindowsDesktop}>
      <ZCodeIntlProvider
        settingService={services?.settingService}
        broadcastService={services?.broadcastService}
      >
        {content}
        {connectionLoss !== null && boot.phase === "ready" ? (
          <HostConnectionLost loss={connectionLoss} onCloseWindow={closeWindow} />
        ) : null}
      </ZCodeIntlProvider>
    </AppErrorBoundary>
  );
}
