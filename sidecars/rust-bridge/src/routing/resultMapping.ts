import type { ResultShape } from "./routeTable.ts";

export type MappedResult = { ok: true; value: unknown } | { ok: false; reason: string };

/**
 * Converts a Rust JSON result into the value the TS service returns.
 * `base64Bytes` exists because JSON cannot carry bytes; the TS service returns a
 * top-level `Uint8Array`, which the RPC codec sends on its raw-bytes fast path.
 */
export function mapRustResult(shape: ResultShape, result: unknown): MappedResult {
  if (shape === "json") return { ok: true, value: result };
  if (shape === "jsonText") return mapJsonText(result);
  if (typeof result !== "object" || result === null || Array.isArray(result)) {
    return { ok: false, reason: "expected {base64} result" };
  }
  const base64 = (result as Record<string, unknown>).base64;
  if (typeof base64 !== "string") return { ok: false, reason: "expected {base64} result" };
  return { ok: true, value: new Uint8Array(Buffer.from(base64, "base64")) };
}

/** `{json: "<text>"}` -> the parsed value (`JSON.parse` keeps the text's key order). */
function mapJsonText(result: unknown): MappedResult {
  if (typeof result !== "object" || result === null || Array.isArray(result)) {
    return { ok: false, reason: "expected {json} result" };
  }
  const text = (result as Record<string, unknown>).json;
  if (typeof text !== "string") return { ok: false, reason: "expected {json} result" };
  try {
    return { ok: true, value: JSON.parse(text) as unknown };
  } catch {
    return { ok: false, reason: "invalid {json} text" };
  }
}
