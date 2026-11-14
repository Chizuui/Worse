import { ResponseType, encodePromiseRequest, readBody, readOutboundResponseHeader } from "../rpc/frames.ts";
import { isRecord } from "./types.ts";

/**
 * Bridge-owned request ids for calls injected into the TS Host. Renderer ids
 * count up from 0 and never get near 2^30; the VQL codec on both sides handles
 * this range without sign issues.
 */
export const HOST_CALL_ID_BASE = 0x4000_0000;

export class HostCallError extends Error {
  override readonly name = "HostCallError";
}

/**
 * Calls the TS Host's own services over the attachment port, as if the renderer
 * had asked (used to read `setting.get` for the terminal profile). The request
 * is delivered to the Host's port listeners; the matching response is taken out
 * of the Host's outbound stream and never reaches the renderer.
 */
export interface HostCaller {
  call(channel: string, method: string, args: unknown[], timeoutMs: number): Promise<unknown>;
  /** True when an outbound Host frame answered one of our calls (consumed). */
  handleOutbound(frame: Uint8Array): boolean;
  dispose(): void;
}

interface PendingCall {
  resolve(value: unknown): void;
  reject(error: Error): void;
  timer: ReturnType<typeof setTimeout>;
}

export function createHostCaller(injectToHost: (frame: Uint8Array) => void): HostCaller {
  const pending = new Map<number, PendingCall>();
  /** Ids we stopped waiting for (timeout, attachment closed); their late answers are still swallowed. */
  const abandoned = new Set<number>();
  let nextId = HOST_CALL_ID_BASE;
  let disposed = false;

  const settle = (id: number): PendingCall | undefined => {
    const entry = pending.get(id);
    if (entry === undefined) return undefined;
    pending.delete(id);
    clearTimeout(entry.timer);
    return entry;
  };

  return {
    call(channel, method, args, timeoutMs) {
      if (disposed) return Promise.reject(new HostCallError("attachment closed"));
      const id = nextId;
      nextId += 1;
      return new Promise<unknown>((resolve, reject) => {
        const timer = setTimeout(() => {
          abandoned.add(id);
          settle(id)?.reject(new HostCallError(`${channel}.${method} timed out after ${timeoutMs}ms`));
        }, timeoutMs);
        timer.unref?.();
        pending.set(id, { resolve, reject, timer });
        injectToHost(encodePromiseRequest(id, channel, method, args));
      });
    },
    handleOutbound(frame) {
      if (pending.size === 0 && abandoned.size === 0) return false;
      const header = readOutboundResponseHeader(frame);
      if (header === null || header.id < HOST_CALL_ID_BASE) return false;
      // 超时或 attachment 关闭后迟到的应答属于 bridge 自己的调用，不能漏给 renderer。
      if (abandoned.delete(header.id)) return true;
      const entry = settle(header.id);
      if (entry === undefined) return false;
      const body = readBody(frame, header.bodyOffset);
      if (header.type === ResponseType.PromiseSuccess) {
        entry.resolve(body);
      } else {
        const message = isRecord(body) && typeof body.message === "string" ? body.message : "Host call failed";
        entry.reject(new HostCallError(message));
      }
      return true;
    },
    dispose() {
      disposed = true;
      for (const id of Array.from(pending.keys())) {
        abandoned.add(id);
        settle(id)?.reject(new HostCallError("attachment closed"));
      }
    }
  };
}
