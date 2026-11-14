// Normalization and deep comparison for TS vs Rust results (contract section 8).
import { isDeepStrictEqual } from "node:util";

const MTIME_KEYS = new Set(["mtimeMs", "modifiedAtMs", "ctimeMs", "atimeMs", "birthtimeMs"]);

/** Bytes become base64, mtimes are rounded to 1 ms; everything else is kept verbatim. */
export function normalize(value, key = "") {
  if (value instanceof Uint8Array) return { $bytes: Buffer.from(value).toString("base64") };
  if (Array.isArray(value)) return value.map((item) => normalize(item));
  if (value && typeof value === "object") {
    const out = {};
    for (const [childKey, child] of Object.entries(value)) {
      if (child === undefined) continue;
      out[childKey] = normalize(child, childKey);
    }
    return out;
  }
  if (typeof value === "number" && MTIME_KEYS.has(key)) return Math.round(value);
  return value;
}

export function normalizeOutcome(outcome) {
  return outcome.ok ? { ok: true, value: normalize(outcome.value) } : { ok: false, error: normalize(outcome.error) };
}

/** First differing JSON path, for a readable failure reason. */
export function firstDifference(left, right, at = "$") {
  if (isDeepStrictEqual(left, right)) return null;
  if (left && right && typeof left === "object" && typeof right === "object" && Array.isArray(left) === Array.isArray(right)) {
    const keys = new Set([...Object.keys(left), ...Object.keys(right)]);
    for (const key of keys) {
      const diff = firstDifference(left[key], right[key], `${at}.${key}`);
      if (diff) return diff;
    }
  }
  const show = (value) => {
    const text = JSON.stringify(value);
    return text === undefined ? "undefined" : text.length > 300 ? `${text.slice(0, 300)}...` : text;
  };
  return `${at}: ts=${show(left)} rust=${show(right)}`;
}

export function median(samples) {
  const sorted = [...samples].sort((a, b) => a - b);
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
}
