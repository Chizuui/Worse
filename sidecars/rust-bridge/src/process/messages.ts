/** JSON-RPC envelope of the `harness-host` NDJSON protocol (contract section 2). */

export const PROTOCOL_VERSION = 1;
export const MAX_LINE_BYTES = 64 * 1024 * 1024;

export const ErrorCode = {
  ParseError: -32700,
  InvalidRequest: -32600,
  MethodNotFound: -32601,
  InvalidParams: -32602,
  InternalError: -32603,
  ServerNotInitialized: -32002,
  RequestCancelled: -32800,
  IoError: 1,
  AppError: 2
} as const;

export interface RustErrorObject {
  code: number;
  message: string;
  data?: unknown;
}

export type InboundMessage =
  | { kind: "result"; id: number; result: unknown }
  | { kind: "error"; id: number | null; error: RustErrorObject }
  | { kind: "progress"; id: number; value: unknown }
  /** Server notification other than $/progress (r2: $/watch/*, $/pty/*). */
  | { kind: "notification"; method: string; params: Record<string, unknown> };

export interface InitializeResult {
  protocolVersion: number;
  server: string;
  serverVersion: string;
  services: string[];
  methods: string[];
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function readErrorObject(value: unknown): RustErrorObject | null {
  if (!isRecord(value) || typeof value.code !== "number" || typeof value.message !== "string") return null;
  return "data" in value ? { code: value.code, message: value.message, data: value.data } : { code: value.code, message: value.message };
}

/** Parses one stdout line. Returns null for lines that are not a valid envelope. */
export function parseInboundLine(line: string): InboundMessage | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(line);
  } catch {
    return null;
  }
  if (!isRecord(parsed)) return null;
  if (parsed.method === "$/progress") {
    const params = parsed.params;
    if (!isRecord(params) || typeof params.id !== "number") return null;
    return { kind: "progress", id: params.id, value: params.value };
  }
  if (typeof parsed.method === "string" && parsed.method.startsWith("$/") && !("id" in parsed)) {
    return isRecord(parsed.params) ? { kind: "notification", method: parsed.method, params: parsed.params } : null;
  }
  const id = parsed.id;
  if ("error" in parsed) {
    const error = readErrorObject(parsed.error);
    if (error === null || (typeof id !== "number" && id !== null)) return null;
    return { kind: "error", id, error };
  }
  if ("result" in parsed && typeof id === "number") return { kind: "result", id, result: parsed.result };
  return null;
}

export function readInitializeResult(value: unknown): InitializeResult | null {
  if (!isRecord(value)) return null;
  const { protocolVersion, server, serverVersion, services, methods } = value;
  if (typeof protocolVersion !== "number" || !Array.isArray(methods)) return null;
  return {
    protocolVersion,
    server: typeof server === "string" ? server : "unknown",
    serverVersion: typeof serverVersion === "string" ? serverVersion : "unknown",
    services: Array.isArray(services) ? services.filter((item): item is string => typeof item === "string") : [],
    methods: methods.filter((item): item is string => typeof item === "string")
  };
}
