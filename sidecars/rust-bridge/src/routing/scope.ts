/**
 * Whether an attachment's Host message targets the window's local services.
 *
 * `init-local` is always local. `attach-service-port` carries `scope.kind`;
 * remote and source-settings scopes route `file`/`git` to the remote source and
 * must never be intercepted (spec R5). Anything unrecognised is treated as
 * non-local (fail closed: the TS Host answers).
 */
export function isLocalScopeHostMessage(hostMessage: Record<string, unknown>): boolean {
  if (hostMessage.type === "init-local") return true;
  if (hostMessage.type !== "attach-service-port") return false;
  const scope = hostMessage.scope;
  if (typeof scope !== "object" || scope === null) return false;
  return (scope as Record<string, unknown>).kind === "local";
}

/**
 * Whether the attachment is a desktop renderer on local scope. Only these get the
 * Rust `terminal` route: desktop terminals are Host-global, while phone
 * (`web-remote-replayable`) terminals are owned by the TS Host's page-owner
 * scopes, which the bridge does not replicate (spec T3). `init-local` is the
 * window's own desktop renderer.
 */
export function isDesktopContinuousLocalHostMessage(hostMessage: Record<string, unknown>): boolean {
  if (hostMessage.type === "init-local") return true;
  return isLocalScopeHostMessage(hostMessage) && hostMessage.clientMode === "desktop-continuous";
}
