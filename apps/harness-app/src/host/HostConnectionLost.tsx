import { Button } from "@zcode/ui/button";
import type { HostCommandFailure } from "./bridge.js";

/**
 * Reconnects by reloading the webview.
 *
 * The reload re-runs the boot sequence: `host_open_attachment` reuses a live
 * Host (`attach-service-port`) or respawns a failed one (`init-local`). An
 * in-place remount is not used because the shared UI keeps module-level
 * stores built from the lost Host's projections; a fresh document cannot
 * render stale server state as fact.
 */
export function reconnectHost(): void {
  window.location.reload();
}

/**
 * Blocking surface shown over the app when the Host attachment is lost after
 * boot, instead of leaving a UI whose service calls silently never answer.
 */
export function HostConnectionLost({
  loss,
  onCloseWindow,
}: {
  loss: HostCommandFailure;
  /** The overlay covers the self-drawn title bar, so it offers its own close action. */
  onCloseWindow: () => void;
}) {
  return (
    <div
      className="fixed inset-0 z-[100] flex items-center justify-center bg-background/70 px-4 backdrop-blur-sm"
      data-testid="host-connection-lost"
    >
      <div
        className="flex w-full max-w-md flex-col gap-3 rounded-2xl border border-popover-border bg-popover p-4 text-foreground shadow-md"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="host-connection-lost-title"
        aria-describedby="host-connection-lost-message"
      >
        <h2 id="host-connection-lost-title" className="text-ui-lg font-medium">
          Lost connection to the local Host
        </h2>
        <p id="host-connection-lost-message" className="text-ui-base text-foreground-subtle">
          {loss.message}
        </p>
        <code className="text-ui-caption text-foreground-subtle" data-testid="host-connection-lost-code">
          {loss.code}
        </code>
        <div className="flex justify-end gap-2">
          <Button type="button" variant="outline" onClick={onCloseWindow}>
            Close window
          </Button>
          <Button type="button" onClick={reconnectHost} autoFocus>
            Reconnect
          </Button>
        </div>
      </div>
    </div>
  );
}
