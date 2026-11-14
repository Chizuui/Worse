import { RUST_SERVICES, type RustService } from "./routeTable.ts";

/**
 * Default when `HARNESS_RUST_SERVICES` is unset. Decided by the Phase 3a
 * differential gate (`Worse/.work/phase3/bridge.md`): all three services are
 * result-identical. `search` is faster (index ~2x, text ~19x on a release
 * build) and, after the runner fix in `Worse/.work/fixes/git-perf.md`, `git`
 * is at parity or faster (1.01-1.69x). `fs` is latency-neutral, so it stays
 * opt-in. Phase 3b gate (`Worse/.work/phase3b/bridge.md`): `pty` and `watch`
 * are behaviour-identical; `pty` is faster (terminal.create ~2.5x, explicit
 * spawn ~7x, echo at parity) so it is on by default; `watch` is latency-neutral
 * (debounce-bound) and, like `fs`, stays opt-in. Phase 3c `store` (read-only
 * task-index reads, `Worse/.work/phase3c/storeA.md`) is opt-in. `all` enables all six.
 */
export const DEFAULT_RUST_SERVICES: readonly RustService[] = ["search", "git", "pty"];

export interface ParsedFlags {
  services: ReadonlySet<RustService>;
  /** Tokens that were not recognised; reported once by the caller. */
  unknown: string[];
}

function isRustService(value: string): value is RustService {
  return (RUST_SERVICES as readonly string[]).includes(value);
}

/** Parses `fs,search,git`, `all`, `none` or empty. Unknown tokens are ignored. */
export function parseRustServiceFlags(raw: string | undefined): ParsedFlags {
  if (raw === undefined) return { services: new Set(DEFAULT_RUST_SERVICES), unknown: [] };
  const services = new Set<RustService>();
  const unknown: string[] = [];
  for (const token of raw.split(",").map((part) => part.trim().toLowerCase())) {
    if (token === "" || token === "none" || token === "off" || token === "0") continue;
    if (token === "all" || token === "1" || token === "on") {
      for (const service of RUST_SERVICES) services.add(service);
    } else if (isRustService(token)) {
      services.add(token);
    } else {
      unknown.push(token);
    }
  }
  return { services, unknown };
}
