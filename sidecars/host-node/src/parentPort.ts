import { log } from "./log.ts";
import { serializeSidecarLine } from "./protocol.ts";

export interface ParentPortEvent {
  data: unknown;
  ports: unknown[];
}

export type ParentPortListener = (event: ParentPortEvent) => void | Promise<void>;

export interface ParentPortShim {
  postMessage(message: unknown): void;
  on(event: "message", listener: ParentPortListener): void;
  off(event: "message", listener: ParentPortListener): void;
  once(event: "message", listener: ParentPortListener): void;
  emitHostMessage(message: unknown, ports: unknown[]): void;
  close(): void;
}

type ProcessWithParentPort = NodeJS.Process & { parentPort?: ParentPortShim };

export function createParentPortShim(writeLine: (line: string) => void): ParentPortShim {
  const listeners = new Set<ParentPortListener>();
  let closed = false;

  const postMessage = (message: unknown): void => {
    if (closed) {
      log("debug", "parentPort.postMessage after close; dropped");
      return;
    }
    if (typeof message !== "object" || message === null || Array.isArray(message)) {
      log("warn", "parentPort.postMessage received a non-object payload; dropped");
      return;
    }
    writeLine(
      serializeSidecarLine({
        v: 1,
        t: "host",
        message: message as Record<string, unknown>
      })
    );
  };

  const on = (_event: "message", listener: ParentPortListener): void => {
    listeners.add(listener);
  };

  const off = (_event: "message", listener: ParentPortListener): void => {
    listeners.delete(listener);
  };

  const once = (_event: "message", listener: ParentPortListener): void => {
    const wrapper: ParentPortListener = (event) => {
      listeners.delete(wrapper);
      return listener(event);
    };
    listeners.add(wrapper);
  };

  const emitHostMessage = (message: unknown, ports: unknown[]): void => {
    if (closed) return;
    for (const listener of Array.from(listeners)) {
      try {
        void Promise.resolve(listener({ data: message, ports })).catch((error: unknown) => {
          log("error", "parentPort message listener rejected", error);
        });
      } catch (error) {
        log("error", "parentPort message listener threw", error);
      }
    }
  };

  const close = (): void => {
    closed = true;
    listeners.clear();
  };

  return { postMessage, on, off, once, emitHostMessage, close };
}

export function installParentPort(shim: ParentPortShim): void {
  (process as ProcessWithParentPort).parentPort = shim;
}
