import type { RustService } from "./routeTable.ts";
import { isDesktopContinuousLocalHostMessage } from "./scope.ts";

/**
 * Services an attachment may route to Rust. `store` (Phase 3c) only serves desktop
 * renderers on local scope: web-remote attachments decorate the project service with
 * activity data that the Rust read path does not reproduce (spec phase-3c-store.md S4).
 */
export function attachmentServices(
  services: ReadonlySet<RustService>,
  hostMessage: Record<string, unknown>
): ReadonlySet<RustService> {
  if (!services.has("store") || isDesktopContinuousLocalHostMessage(hostMessage)) return services;
  return new Set(Array.from(services).filter((service) => service !== "store"));
}
