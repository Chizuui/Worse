import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  IPlatformService,
  WebRemoteControlReconnectWorkspaceRequest,
  WebRemoteControlReconnectWorkspaceResult,
  WebRemoteControlRendererActionRequest,
  WebRemoteControlRendererActionResult,
  WebRemoteControlStatus,
} from "@zcode/shared";
// The shared UI logger (packages/ui/src/logger.ts); @zcode/ui does not re-export it.
import { logger } from "@/logger.js";

/**
 * Desktop web remote control (Phase 4, spec
 * `Worse/docs/specs/harness/phase-4-web-remote-control.md`).
 *
 * The ZCode manager runs in the host-node sidecar; this module only maps the
 * `IPlatformService` web remote-control members onto the `remote_control_call`
 * command and the `harness://web-remote/*` events. It keeps no remote state:
 * status always comes from the sidecar, and renderer round-trips are answered
 * with whatever the shared UI handler returns.
 */

type WebRemoteMethod =
  | "start"
  | "stop"
  | "getStatus"
  | "notifyAuthStateChanged"
  | "syncWorkspaces"
  | "reconnectResult"
  | "rendererActionResult";

const EVENTS = {
  status: "harness://web-remote/status",
  reconnectRequest: "harness://web-remote/reconnect-request",
  rendererActionRequest: "harness://web-remote/renderer-action-request",
} as const;

function call<T>(method: WebRemoteMethod, params: Record<string, unknown> = {}): Promise<T> {
  return invoke<T>("remote_control_call", { method, params });
}

function describeError(error: unknown): string {
  if (typeof error === "object" && error !== null && "message" in error) {
    return String(error.message);
  }
  return String(error);
}

/** Fire-and-forget notification; failures are logged, never thrown into the UI. */
function notify(method: WebRemoteMethod, params: Record<string, unknown>): void {
  call<unknown>(method, params).catch((error: unknown) =>
    logger.warn("web remote control notification failed", { method, error: describeError(error) }),
  );
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
      logger.warn("failed to subscribe to a web remote control event", { event, error }),
    );
  return () => {
    disposed = true;
    unlisten?.();
    unlisten = null;
  };
}

type WebRemoteControlMembers = Pick<
  IPlatformService,
  | "startWebRemoteControl"
  | "stopWebRemoteControl"
  | "getWebRemoteControlStatus"
  | "notifyWebRemoteControlAuthStateChanged"
  | "onWebRemoteControlStatusChanged"
  | "syncWebRemoteControlWorkspaces"
  | "onWebRemoteControlReconnectWorkspace"
  | "onWebRemoteControlRendererAction"
>;

/** Desktop members backed by the sidecar's remote-control owner. */
export function createDesktopWebRemoteControl(): WebRemoteControlMembers {
  return {
    startWebRemoteControl: (context) => call("start", { context }),
    stopWebRemoteControl: async () => {
      await call<unknown>("stop");
    },
    getWebRemoteControlStatus: () => call<WebRemoteControlStatus>("getStatus"),
    notifyWebRemoteControlAuthStateChanged: (change) => notify("notifyAuthStateChanged", { change }),
    onWebRemoteControlStatusChanged: (handler) => subscribe<WebRemoteControlStatus>(EVENTS.status, handler),
    syncWebRemoteControlWorkspaces: (workspaces) => notify("syncWorkspaces", { workspaces }),
    onWebRemoteControlReconnectWorkspace: (handler) =>
      subscribe<WebRemoteControlReconnectWorkspaceRequest>(EVENTS.reconnectRequest, (request) => {
        void handler(request)
          .catch(
            (error: unknown): WebRemoteControlReconnectWorkspaceResult => ({
              requestId: request.requestId,
              workspaceKey: request.workspaceKey,
              success: false,
              error: describeError(error),
            }),
          )
          .then((result) => notify("reconnectResult", { result }));
      }),
    onWebRemoteControlRendererAction: (handler) =>
      subscribe<WebRemoteControlRendererActionRequest>(EVENTS.rendererActionRequest, (request) => {
        void handler(request)
          .catch(
            (error: unknown): WebRemoteControlRendererActionResult => ({
              requestId: request.requestId,
              success: false,
              error: describeError(error),
            }),
          )
          .then((result) => notify("rendererActionResult", { result }));
      }),
  };
}

/**
 * Mobile shells are remote clients: they never host remote control, so the
 * members report idle and refuse to start instead of pretending to work.
 */
export function createMobileWebRemoteControl(): WebRemoteControlMembers {
  return {
    startWebRemoteControl: () =>
      Promise.reject(new Error("This device is a remote client; start web remote control on the desktop.")),
    stopWebRemoteControl: () => Promise.resolve(),
    getWebRemoteControlStatus: () => Promise.resolve({ status: "idle" }),
  };
}
