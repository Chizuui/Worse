import type { Readable, Writable } from "node:stream";
import { createLineSplitter } from "./lineSplitter.ts";
import { ErrorCode, MAX_LINE_BYTES, parseInboundLine, type RustErrorObject } from "./messages.ts";

/** The server answered with a JSON-RPC error object. */
export class RustCallError extends Error {
  override readonly name = "RustCallError";
  readonly rpcError: RustErrorObject;

  constructor(rpcError: RustErrorObject) {
    super(rpcError.message);
    this.rpcError = rpcError;
  }
}

/** The transport failed: closed, timed out, overflowed or malformed. */
export class RustTransportError extends Error {
  override readonly name = "RustTransportError";
  readonly reason: "closed" | "timeout" | "overflow" | "write";

  constructor(reason: RustTransportError["reason"], message: string) {
    super(message);
    this.reason = reason;
  }
}

/** The request was cancelled by the caller. */
export class RustCancelledError extends Error {
  override readonly name = "RustCancelledError";
}

export interface CallOptions {
  timeoutMs?: number;
  signal?: AbortSignal;
  onProgress?: (value: unknown) => void;
  /**
   * Runs synchronously when the result line is parsed, before any later stdout line is handled.
   * Session methods use it to register the new session id before its first notification arrives.
   */
  onResult?: (value: unknown) => void;
}

export interface JsonRpcConnection {
  call(method: string, params: unknown, options?: CallOptions): Promise<unknown>;
  close(message: string): void;
  onClose(handler: (error: RustTransportError) => void): void;
  /** Server notifications other than $/progress (r2). Handlers run in stdout order. */
  onNotification(handler: (method: string, params: Record<string, unknown>) => void): void;
  readonly isClosed: boolean;
}

interface Pending {
  resolve(value: unknown): void;
  reject(error: Error): void;
  onProgress: ((value: unknown) => void) | undefined;
  onResult: ((value: unknown) => void) | undefined;
  cleanup(): void;
}

/**
 * Request/response correlation over an NDJSON stdio pair. Responses may
 * arrive out of order; progress notifications go to the request's callback.
 * Malformed stdout lines and server-side `id: null` errors are reported via
 * `onDiagnostic` and otherwise ignored. Ids start at 0 so the first call
 * (`initialize`) gets the reserved id 0 (contract section 3).
 */
export function createJsonRpcConnection(
  input: Writable,
  output: Readable,
  onDiagnostic: (message: string) => void,
  firstId = 0
): JsonRpcConnection {
  const pending = new Map<number, Pending>();
  const closeHandlers: Array<(error: RustTransportError) => void> = [];
  const notificationHandlers: Array<(method: string, params: Record<string, unknown>) => void> = [];
  let nextId = firstId;
  let closed = false;

  const fail = (error: RustTransportError): void => {
    if (closed) return;
    closed = true;
    for (const entry of pending.values()) {
      entry.cleanup();
      entry.reject(error);
    }
    pending.clear();
    for (const handler of closeHandlers) handler(error);
  };

  const writeLine = (message: Record<string, unknown>): boolean => {
    if (closed || !input.writable) return false;
    try {
      input.write(`${JSON.stringify(message)}\n`);
      return true;
    } catch (error) {
      fail(new RustTransportError("write", error instanceof Error ? error.message : String(error)));
      return false;
    }
  };

  const settle = (id: number, apply: (entry: Pending) => void): void => {
    const entry = pending.get(id);
    if (entry === undefined) return;
    pending.delete(id);
    entry.cleanup();
    apply(entry);
  };

  const onLine = (line: string): void => {
    const message = parseInboundLine(line);
    if (message === null) {
      onDiagnostic(`ignored malformed line: ${line.slice(0, 200)}`);
      return;
    }
    switch (message.kind) {
      case "progress":
        pending.get(message.id)?.onProgress?.(message.value);
        return;
      case "result":
        settle(message.id, (entry) => {
          entry.onResult?.(message.result);
          entry.resolve(message.result);
        });
        return;
      case "notification":
        for (const handler of notificationHandlers) {
          try {
            handler(message.method, message.params);
          } catch (error) {
            onDiagnostic(`notification handler threw: ${error instanceof Error ? error.message : String(error)}`);
          }
        }
        return;
      case "error":
        if (message.id === null) {
          onDiagnostic(`server error without id: ${message.error.message}`);
          return;
        }
        settle(message.id, (entry) =>
          entry.reject(
            message.error.code === ErrorCode.RequestCancelled
              ? new RustCancelledError(message.error.message)
              : new RustCallError(message.error)
          )
        );
        return;
    }
  };

  const splitter = createLineSplitter(MAX_LINE_BYTES, onLine, () =>
    fail(new RustTransportError("overflow", "harness-host stdout line exceeded 64 MiB"))
  );
  output.on("data", (chunk: Buffer | string) => splitter.push(typeof chunk === "string" ? Buffer.from(chunk) : chunk));
  output.on("end", () => {
    const rest = splitter.flush();
    if (rest !== null) onLine(rest);
    fail(new RustTransportError("closed", "harness-host closed stdout"));
  });
  output.on("error", (error: Error) => fail(new RustTransportError("closed", error.message)));
  input.on("error", (error: Error) => fail(new RustTransportError("write", error.message)));

  const call = (method: string, params: unknown, options: CallOptions = {}): Promise<unknown> => {
    if (closed) return Promise.reject(new RustTransportError("closed", "connection is closed"));
    if (options.signal?.aborted === true) return Promise.reject(new RustCancelledError("cancelled before send"));
    const id = nextId;
    nextId += 1;
    return new Promise<unknown>((resolve, reject) => {
      let timer: ReturnType<typeof setTimeout> | undefined;
      const sendCancel = (): void => {
        writeLine({ method: "$/cancel", params: { id } });
      };
      const onAbort = (): void => {
        sendCancel();
        settle(id, (entry) => entry.reject(new RustCancelledError("cancelled")));
      };
      const cleanup = (): void => {
        if (timer !== undefined) clearTimeout(timer);
        options.signal?.removeEventListener("abort", onAbort);
      };
      pending.set(id, { resolve, reject, onProgress: options.onProgress, onResult: options.onResult, cleanup });
      options.signal?.addEventListener("abort", onAbort, { once: true });
      if (options.timeoutMs !== undefined) {
        timer = setTimeout(() => {
          sendCancel();
          settle(id, (entry) =>
            entry.reject(new RustTransportError("timeout", `${method} timed out after ${String(options.timeoutMs)}ms`))
          );
        }, options.timeoutMs);
      }
      if (!writeLine({ id, method, params })) {
        settle(id, (entry) => entry.reject(new RustTransportError("write", "failed to write request")));
      }
    });
  };

  return {
    call,
    close: (message) => fail(new RustTransportError("closed", message)),
    onClose: (handler) => {
      closeHandlers.push(handler);
    },
    onNotification: (handler) => {
      notificationHandlers.push(handler);
    },
    get isClosed() {
      return closed;
    }
  };
}
