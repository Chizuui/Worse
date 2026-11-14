import { useCallback, useEffect, useState } from "react";
import {
  databaseStartupStateSchema,
  type DatabaseStartupControl,
  type DatabaseStartupState,
} from "@zcode/shared";
import type { IServiceAccessor } from "@zcode/services";
import {
  describeHostFailure,
  hostDatabaseStartupControl,
  onHostDatabaseStartup,
  openHostAttachment,
  type HostCommandFailure,
} from "./bridge.js";
import { watchHostConnection } from "./connectionLoss.js";
import { createHostMessagePort, type HostMessagePort } from "./messagePort.js";
import { remoteWorkspaceBridge } from "./remoteWorkspaces.js";
import { connectHostServices, type HostServicesConnection } from "./services.js";
import { StartupAdmission } from "./startup.js";

export type HostBootState =
  | { phase: "starting" }
  | { phase: "failed"; failure: HostCommandFailure }
  | { phase: "ready"; services: IServiceAccessor };

export interface HostBoot {
  boot: HostBootState;
  startupState: DatabaseStartupState | null;
  /** Set when the admitted attachment is lost after boot (Host crash or transport close). */
  connectionLoss: HostCommandFailure | null;
  retry(): void;
  exit(): void;
}

/**
 * Runs the desktop boot sequence (contract 6.4, steps 1-5):
 * subscribe to startup states, open the attachment, hold the port until its
 * startup generation is ready, then connect the Host-backed services.
 */
export function useHostBoot(): HostBoot {
  const [boot, setBoot] = useState<HostBootState>({ phase: "starting" });
  const [startupState, setStartupState] = useState<DatabaseStartupState | null>(null);
  const [connectionLoss, setConnectionLoss] = useState<HostCommandFailure | null>(null);

  useEffect(() => {
    const admission = new StartupAdmission<HostMessagePort>();
    let cancelled = false;
    let unsubscribe: (() => void) | null = null;
    let connection: HostServicesConnection | null = null;
    let port: HostMessagePort | null = null;
    let stopWatching: (() => void) | null = null;
    let unbindRemote: (() => void) | null = null;

    const fail = (error: unknown): void => {
      if (!cancelled) {
        setBoot({ phase: "failed", failure: describeHostFailure(error) });
      }
    };

    const enterIfReady = (): void => {
      if (cancelled || connection !== null) {
        return;
      }
      const readyPort = admission.takeReadyPort();
      if (readyPort === undefined) {
        return;
      }
      try {
        connection = connectHostServices(readyPort);
        // Phase 4: remote workspace sessions are layered on the window's base services.
        remoteWorkspaceBridge.setBaseServices(readyPort, connection.services);
        stopWatching = watchHostConnection(readyPort, setConnectionLoss);
        setBoot({ phase: "ready", services: connection.services });
      } catch (error) {
        fail(error);
      }
    };

    // Like the Electron renderer, startup states are ignored once the app root is entered;
    // a lost Host after boot is reported by `watchHostConnection` instead.
    const applyState = (state: DatabaseStartupState): void => {
      if (cancelled || connection !== null || !admission.acceptState(state)) {
        return;
      }
      setStartupState(state);
      enterIfReady();
    };

    const run = async (): Promise<void> => {
      // Step 1: subscribe before opening, so no state published meanwhile is missed.
      unsubscribe = await onHostDatabaseStartup(applyState);
      if (cancelled) {
        return;
      }
      // Step 2: spawn (first call) and grant.
      const grant = await openHostAttachment();
      if (cancelled) {
        return;
      }
      // Step 3: port + cached snapshot + port admission.
      port = createHostMessagePort({
        url: grant.url,
        token: grant.token,
        attachmentId: grant.attachmentId,
      });
      port.ready.catch(fail);
      // Remote-workspace control frames share this attachment; scoped ports may arrive before entry.
      unbindRemote = remoteWorkspaceBridge.bindPort({ port, url: grant.url });
      const cached = databaseStartupStateSchema.safeParse(grant.startupState);
      if (cached.success) {
        applyState(cached.data);
      }
      admission.acceptPort({ databaseStartupId: grant.databaseStartupId }, port);
      // Steps 4-5: enter now if already ready, otherwise when a ready state arrives.
      enterIfReady();
    };

    run().catch(fail);

    return () => {
      cancelled = true;
      // Stop watching before closing the port, so a local close is never reported as a loss.
      stopWatching?.();
      unsubscribe?.();
      unbindRemote?.();
      connection?.dispose();
      port?.close();
      admission.dispose();
    };
  }, []);

  const sendControl = useCallback((control: DatabaseStartupControl): void => {
    hostDatabaseStartupControl(control).catch((error: unknown) => {
      setBoot({ phase: "failed", failure: describeHostFailure(error) });
    });
  }, []);

  const retry = useCallback((): void => {
    if (startupState !== null) {
      sendControl({ action: "retry", attemptId: startupState.attemptId });
    }
  }, [sendControl, startupState]);

  // The shell disposes the Host and quits the app on `exit`.
  const exit = useCallback((): void => sendControl({ action: "exit" }), [sendControl]);

  return { boot, startupState, connectionLoss, retry, exit };
}
