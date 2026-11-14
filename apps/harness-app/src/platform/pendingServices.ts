import type { Root } from "@zcode/ui";

/** Service accessor shape consumed by the shared application root. */
type RootServices = Parameters<typeof Root>[0]["services"];

const HOST_BRIDGE_UNAVAILABLE = "The Harness host bridge is not connected yet";

function isSubscription(name: string): boolean {
  return /^(on[A-Z]|subscribe|listen)/u.test(name);
}

function createPendingMember(name: string): unknown {
  const member = (..._args: unknown[]): unknown => {
    if (isSubscription(name)) {
      return () => {
        // Subscriptions become no-ops until the Host bridge is available.
      };
    }
    return Promise.reject(new Error(HOST_BRIDGE_UNAVAILABLE));
  };

  return new Proxy(member, {
    get(_target, property) {
      if (property === "then") {
        return undefined;
      }
      return createPendingMember(String(property));
    },
  });
}

/**
 * Placeholder services for the shell while the Rust Host bridge is not wired.
 *
 * Reads resolve to a rejected promise and event registrations return a no-op
 * disposer, so the shared root can render its disconnected state instead of
 * crashing on a missing service object. Replace this factory with the real
 * Host-backed accessor once the bridge lands.
 */
export function createPendingRootServices(): RootServices {
  return createPendingMember("services") as RootServices;
}
