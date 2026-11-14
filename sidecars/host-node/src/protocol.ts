export const PROTOCOL_VERSION = 1;
export const MAX_LINE_BYTES = 16 * 1024 * 1024;

import type {
  SidecarErrorCode,
  AttachmentCloseReason,
  AttachmentMessageType,
  ReadyLine,
  AttachOpenedLine,
  AttachActiveLine,
  AttachClosedLine,
  AttachFailedLine,
  HostMessageLine,
  FatalLine,
  LogLine,
  PongLine,
  SidecarLine,
  AttachOpenLine,
  PingLine,
  ShellLine,
} from "./protocolTypes.ts";

export type {
  SidecarErrorCode,
  AttachmentCloseReason,
  AttachmentMessageType,
  ReadyLine,
  AttachOpenedLine,
  AttachActiveLine,
  AttachClosedLine,
  AttachFailedLine,
  HostMessageLine,
  FatalLine,
  LogLine,
  PongLine,
  SidecarLine,
  AttachOpenLine,
  PingLine,
  ShellLine,
} from "./protocolTypes.ts";

const SIDECAR_ERROR_CODES: ReadonlySet<string> = new Set<SidecarErrorCode>([
  "bad_line",
  "unsupported_version",
  "unknown_kind",
  "line_too_large",
  "attach_duplicate",
  "attach_unknown",
  "attach_expired",
  "attach_rejected",
  "ws_origin_rejected",
  "ws_token_rejected",
  "ws_backpressure",
  "host_import_failed",
  "host_exited",
  "stdin_closed",
  "internal"
]);

const CLOSE_REASONS: ReadonlySet<string> = new Set<AttachmentCloseReason>([
  "ws-closed",
  "host-exit",
  "expired",
  "rejected",
  "backpressure",
  "shutdown"
]);

export class SidecarProtocolError extends Error {
  readonly code: SidecarErrorCode;

  constructor(code: SidecarErrorCode, message: string) {
    super(message);
    this.name = "SidecarProtocolError";
    this.code = code;
  }
}

function parseObject(line: string): Record<string, unknown> {
  let value: unknown;
  try {
    value = JSON.parse(line);
  } catch {
    throw new SidecarProtocolError("bad_line", "line is not valid JSON");
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new SidecarProtocolError("bad_line", "line is not a JSON object");
  }
  const record = value as Record<string, unknown>;
  if (record.v !== PROTOCOL_VERSION) {
    if (typeof record.v === "number") {
      throw new SidecarProtocolError("unsupported_version", `unsupported protocol version: ${record.v}`);
    }
    throw new SidecarProtocolError("bad_line", "missing protocol version");
  }
  if (typeof record.t !== "string") {
    throw new SidecarProtocolError("bad_line", "missing line kind");
  }
  return record;
}

function requireString(record: Record<string, unknown>, field: string): string {
  const value = record[field];
  if (typeof value !== "string") {
    throw new SidecarProtocolError("bad_line", `missing or invalid field: ${field}`);
  }
  return value;
}

function requireNumber(record: Record<string, unknown>, field: string): number {
  const value = record[field];
  if (typeof value !== "number" || !Number.isFinite(value)) {
    throw new SidecarProtocolError("bad_line", `missing or invalid field: ${field}`);
  }
  return value;
}

function requireRecord(record: Record<string, unknown>, field: string): Record<string, unknown> {
  const value = record[field];
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new SidecarProtocolError("bad_line", `missing or invalid field: ${field}`);
  }
  return value as Record<string, unknown>;
}

function requireErrorCode(record: Record<string, unknown>, field: string): SidecarErrorCode {
  const value = requireString(record, field);
  if (!SIDECAR_ERROR_CODES.has(value)) {
    throw new SidecarProtocolError("bad_line", `unknown error code: ${value}`);
  }
  return value as SidecarErrorCode;
}

function requireCloseReason(record: Record<string, unknown>): AttachmentCloseReason {
  const value = requireString(record, "reason");
  if (!CLOSE_REASONS.has(value)) {
    throw new SidecarProtocolError("bad_line", `unknown close reason: ${value}`);
  }
  return value as AttachmentCloseReason;
}

function requireMessageType(record: Record<string, unknown>): AttachmentMessageType {
  const value = requireString(record, "messageType");
  if (value !== "init-local" && value !== "attach-service-port") {
    throw new SidecarProtocolError("bad_line", `unknown message type: ${value}`);
  }
  return value;
}

function requireLogLevel(record: Record<string, unknown>): "info" | "warn" | "error" {
  const value = requireString(record, "level");
  if (value !== "info" && value !== "warn" && value !== "error") {
    throw new SidecarProtocolError("bad_line", `unknown log level: ${value}`);
  }
  return value;
}

export function parseSidecarLine(line: string): SidecarLine {
  const record = parseObject(line);
  const kind = record.t as string;
  switch (kind) {
    case "ready":
      return {
        v: 1,
        t: "ready",
        hostPid: requireNumber(record, "hostPid"),
        wsPort: requireNumber(record, "wsPort"),
        protocol: 1
      };
    case "attach.opened":
      return {
        v: 1,
        t: "attach.opened",
        requestId: requireString(record, "requestId"),
        attachmentId: requireString(record, "attachmentId")
      };
    case "attach.active":
      return {
        v: 1,
        t: "attach.active",
        attachmentId: requireString(record, "attachmentId"),
        messageType: requireMessageType(record)
      };
    case "attach.closed":
      return {
        v: 1,
        t: "attach.closed",
        attachmentId: requireString(record, "attachmentId"),
        reason: requireCloseReason(record)
      };
    case "attach.failed":
      return {
        v: 1,
        t: "attach.failed",
        requestId: requireString(record, "requestId"),
        code: requireErrorCode(record, "code"),
        message: requireString(record, "message")
      };
    case "host":
      return { v: 1, t: "host", message: requireRecord(record, "message") };
    case "fatal":
      return {
        v: 1,
        t: "fatal",
        code: requireErrorCode(record, "code"),
        message: requireString(record, "message")
      };
    case "log":
      return {
        v: 1,
        t: "log",
        level: requireLogLevel(record),
        message: requireString(record, "message")
      };
    case "pong":
      return { v: 1, t: "pong", id: requireString(record, "id") };
    default:
      throw new SidecarProtocolError("unknown_kind", `unknown line kind: ${kind}`);
  }
}

export function parseShellLine(line: string): ShellLine {
  const record = parseObject(line);
  const kind = record.t as string;
  switch (kind) {
    case "attach.open":
      return {
        v: 1,
        t: "attach.open",
        requestId: requireString(record, "requestId"),
        attachmentId: requireString(record, "attachmentId"),
        token: requireString(record, "token"),
        ttlMs: requireNumber(record, "ttlMs"),
        hostMessage: requireRecord(record, "hostMessage")
      };
    case "host":
      return { v: 1, t: "host", message: requireRecord(record, "message") };
    case "ping":
      return { v: 1, t: "ping", id: requireString(record, "id") };
    default:
      throw new SidecarProtocolError("unknown_kind", `unknown line kind: ${kind}`);
  }
}

export function serializeSidecarLine(line: SidecarLine): string {
  return JSON.stringify(line);
}

export function serializeShellLine(line: ShellLine): string {
  return JSON.stringify(line);
}
