import { Button } from "@zcode/ui/button";
import type { HostCommandFailure } from "./bridge.js";
import { reconnectHost } from "./HostConnectionLost.js";

/**
 * Plain failure surface for boot steps 1-5; shows the typed command error code
 * instead of leaving the window on a blank pending fallback. Retrying re-runs
 * the boot, which respawns a failed Host.
 */
export function HostBootFailure({ failure }: { failure: HostCommandFailure }) {
  return (
    <div
      className="flex h-full min-h-dvh flex-col items-center justify-center gap-3 bg-background px-6 text-center text-foreground"
      role="alert"
    >
      <h1 className="text-ui-lg font-medium">Harness could not start the local Host</h1>
      <p className="max-w-md text-ui-base text-foreground-subtle">{failure.message}</p>
      <code className="text-ui-caption text-foreground-subtle" data-testid="host-boot-error-code">
        {failure.code}
      </code>
      <Button type="button" variant="outline" onClick={reconnectHost}>
        Retry
      </Button>
    </div>
  );
}
