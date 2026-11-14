/** Wire types for the NDJSON sidecar protocol (contract section 2). */
export type SidecarErrorCode =
  | "bad_line"
  | "unsupported_version"
  | "unknown_kind"
  | "line_too_large"
  | "attach_duplicate"
  | "attach_unknown"
  | "attach_expired"
  | "attach_rejected"
  | "ws_origin_rejected"
  | "ws_token_rejected"
  | "ws_backpressure"
  | "host_import_failed"
  | "host_exited"
  | "stdin_closed"
  | "internal";

export type AttachmentCloseReason =
  | "ws-closed"
  | "host-exit"
  | "expired"
  | "rejected"
  | "backpressure"
  | "shutdown";

export type AttachmentMessageType = "init-local" | "attach-service-port";

export interface ReadyLine {
  v: 1;
  t: "ready";
  hostPid: number;
  wsPort: number;
  protocol: 1;
}

export interface AttachOpenedLine {
  v: 1;
  t: "attach.opened";
  requestId: string;
  attachmentId: string;
}

export interface AttachActiveLine {
  v: 1;
  t: "attach.active";
  attachmentId: string;
  messageType: AttachmentMessageType;
}

export interface AttachClosedLine {
  v: 1;
  t: "attach.closed";
  attachmentId: string;
  reason: AttachmentCloseReason;
}

export interface AttachFailedLine {
  v: 1;
  t: "attach.failed";
  requestId: string;
  code: SidecarErrorCode;
  message: string;
}

export interface HostMessageLine {
  v: 1;
  t: "host";
  message: Record<string, unknown>;
}

export interface FatalLine {
  v: 1;
  t: "fatal";
  code: SidecarErrorCode;
  message: string;
}

export interface LogLine {
  v: 1;
  t: "log";
  level: "info" | "warn" | "error";
  message: string;
}

export interface PongLine {
  v: 1;
  t: "pong";
  id: string;
}

export type SidecarLine =
  | ReadyLine
  | AttachOpenedLine
  | AttachActiveLine
  | AttachClosedLine
  | AttachFailedLine
  | HostMessageLine
  | FatalLine
  | LogLine
  | PongLine;

export interface AttachOpenLine {
  v: 1;
  t: "attach.open";
  requestId: string;
  attachmentId: string;
  token: string;
  ttlMs: number;
  hostMessage: Record<string, unknown>;
}

export interface PingLine {
  v: 1;
  t: "ping";
  id: string;
}

export type ShellLine = AttachOpenLine | HostMessageLine | PingLine;
