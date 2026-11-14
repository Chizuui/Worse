export type RouteOutcome = "rust" | "surfaced-error" | "fallback" | "cancelled";

export interface MethodStats {
  rust: number;
  surfacedError: number;
  fallback: number;
  cancelled: number;
}

/** Per-method routing counters; read by tests and the differential gate. */
export interface BridgeStats {
  record(method: string, outcome: RouteOutcome): void;
  snapshot(): Record<string, MethodStats>;
}

export function createBridgeStats(): BridgeStats {
  const byMethod = new Map<string, MethodStats>();
  return {
    record(method, outcome) {
      let entry = byMethod.get(method);
      if (entry === undefined) {
        entry = { rust: 0, surfacedError: 0, fallback: 0, cancelled: 0 };
        byMethod.set(method, entry);
      }
      if (outcome === "rust") entry.rust += 1;
      else if (outcome === "surfaced-error") entry.surfacedError += 1;
      else if (outcome === "fallback") entry.fallback += 1;
      else entry.cancelled += 1;
    },
    snapshot() {
      return Object.fromEntries(Array.from(byMethod, ([method, entry]) => [method, { ...entry }]));
    }
  };
}
