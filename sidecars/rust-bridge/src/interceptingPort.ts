import type { BridgeLogger } from "./logger.ts";
import type { AttachmentRouter, RouterIo } from "./routing/attachmentRouter.ts";

type MessageListener = (event: { data: unknown; ports: [] }) => void;

/** Structural twin of host-node's `MessagePortLike` (no package cycle). */
export interface PortLike {
  on(event: "message", listener: MessageListener): void;
  off(event: "message", listener: MessageListener): void;
  once(event: "close", listener: () => void): void;
  postMessage(data: Uint8Array | Record<string, unknown>): void;
  start(): void;
  close(): void;
}

/**
 * Wraps the attachment's virtual port in front of the TS Host.
 *
 * Inbound frames go through the router first; consumed frames never reach the
 * Host. Everything else (and every fallback) is delivered to the Host's
 * listeners unchanged. Outbound Host frames pass through unless they answer a
 * request the bridge injected into the Host (`injectToHost`). `start`
 * is delegated so the inner port keeps its "buffer until start" semantics.
 */
export function createInterceptingPort(
  inner: PortLike,
  createRouter: (io: RouterIo) => AttachmentRouter,
  logger: BridgeLogger
): PortLike {
  const listeners = new Set<MessageListener>();

  const deliver = (data: unknown): void => {
    const event = { data, ports: [] as [] };
    for (const listener of Array.from(listeners)) {
      try {
        listener(event);
      } catch (error) {
        logger("error", "host port listener threw", error);
      }
    }
  };

  const router = createRouter({
    reply: (frame) => inner.postMessage(frame),
    forward: (frame) => deliver(frame),
    injectToHost: (frame) => deliver(frame)
  });

  inner.on("message", (event) => {
    const { data } = event;
    if (data instanceof Uint8Array) {
      let consumed = false;
      try {
        consumed = router.handleInbound(data);
      } catch (error) {
        logger("error", "rust-bridge inbound handling threw; forwarding to the TS Host", error);
      }
      if (consumed) return;
    }
    deliver(data);
  });
  inner.once("close", () => router.dispose());

  return {
    on: (_event, listener) => {
      listeners.add(listener);
    },
    off: (_event, listener) => {
      listeners.delete(listener);
    },
    once: (event, listener) => inner.once(event, listener),
    postMessage: (data) => {
      if (data instanceof Uint8Array) {
        let consumed = false;
        try {
          consumed = router.handleOutbound(data);
        } catch (error) {
          logger("error", "rust-bridge outbound handling threw; passing the frame through", error);
        }
        if (consumed) return;
      }
      inner.postMessage(data);
    },
    start: () => inner.start(),
    close: () => {
      router.dispose();
      inner.close();
    }
  };
}
