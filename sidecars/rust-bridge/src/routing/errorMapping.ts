import { ErrorCode, type RustErrorObject } from "../process/messages.ts";
import type { RpcErrorPayload } from "../rpc/frames.ts";

/** libuv `uv_strerror` texts for the errno names harness-host reports. */
const UV_DESCRIPTIONS: Readonly<Record<string, string>> = {
  ENOENT: "no such file or directory",
  ENOTDIR: "not a directory",
  EISDIR: "illegal operation on a directory",
  EACCES: "permission denied",
  EPERM: "operation not permitted",
  EEXIST: "file already exists",
  EBUSY: "resource busy or locked",
  ELOOP: "too many symbolic links encountered",
  EINVAL: "invalid argument",
  EMFILE: "too many open files",
  ENAMETOOLONG: "name too long",
  ENOTEMPTY: "directory not empty",
  EIO: "i/o error",
  ENOSPC: "no space left on device",
  EROFS: "read-only file system",
  EXDEV: "cross-device link not permitted"
};

export type ErrorDecision =
  | { kind: "surface"; payload: RpcErrorPayload }
  | { kind: "fallback"; reason: string };

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Rebuilds the Node error message `ENOENT: no such file or directory, stat '<path>'`. */
export function formatNodeIoMessage(errno: string, syscall: string, path: string): string {
  const description = UV_DESCRIPTIONS[errno] ?? "unknown error";
  return `${errno}: ${description}, ${syscall} '${path}'`;
}

function errorPayload(name: string, message: string, code: string | undefined): RpcErrorPayload {
  const payload: RpcErrorPayload = { message, name, stack: [`${name}: ${message}`, "    at harness-host (rust)"] };
  if (code !== undefined) payload.code = code;
  return payload;
}

/**
 * Decides what the renderer sees for a Rust error (contract section 4):
 * IoError/AppError are real answers and are surfaced in the TS error shape;
 * every protocol/internal code makes the bridge replay the request on TS.
 */
export function decideRustError(error: RustErrorObject): ErrorDecision {
  const data = isRecord(error.data) ? error.data : {};
  if (error.code === ErrorCode.IoError) {
    const { errno, syscall, path } = data;
    if (typeof errno !== "string" || typeof syscall !== "string" || typeof path !== "string") {
      return { kind: "fallback", reason: "IoError without errno/syscall/path" };
    }
    return { kind: "surface", payload: errorPayload("Error", formatNodeIoMessage(errno, syscall, path), errno) };
  }
  if (error.code === ErrorCode.AppError) {
    const name = typeof data.name === "string" ? data.name : "Error";
    const code = typeof data.code === "string" ? data.code : undefined;
    return { kind: "surface", payload: errorPayload(name, error.message, code) };
  }
  return { kind: "fallback", reason: `rust error ${error.code}: ${error.message}` };
}
