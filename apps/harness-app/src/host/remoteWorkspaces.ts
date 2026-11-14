import { RemoteServiceAccess } from "@zcode/client";
import { ChannelClient, MessagePortProtocol } from "@zcode/rpc";
import type { IServiceAccessor } from "@zcode/services";
import { scopedServicePortPayloadSchema, type ScopedServicePortPayload } from "@zcode/shared";
import {
  createRemoteWorkspaceDisconnectedError,
  registerBaseWorkspaceServices,
  registerRemoteWorkspaceSession,
} from "@zcode/ui";
import { logger } from "@/logger.js";
import { createHostMessagePort, type HostMessagePort } from "./messagePort.js";
import { buildRemoteWorkspaceSessionServicesForRegistration } from "./remoteWorkspaceServices.js";

/**
 * Renderer half of Phase 4 remote workspaces (spec phase-4-remote-workspaces.md).
 *
 * The sidecar runs ZCode Main's remote session manager; this module plays the preload + renderer
 * role: `rw.req` requests and `rw.evt` events ride the window (base) attachment, and every
 * `scoped-service-port` event becomes its own WebSocket attachment whose services are registered
 * in the shared remote-workspace session store, exactly as the Electron renderer registers the
 * transferred MessagePort.
 */

type EventHandler = (payload: unknown) => void;

interface PendingRequest {
  resolve(value: unknown): void;
  reject(error: Error): void;
}

interface ScopedPortEvent {
  payload: ScopedServicePortPayload;
  token: string;
}

interface BaseAttachment {
  port: HostMessagePort;
  url: string;
  /** Set once the app root is entered (Electron: `registerBaseWorkspaceServices`). */
  services: IServiceAccessor | null;
}

function asRecord(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

function toError(value: unknown): Error {
  const record = asRecord(value);
  const message =
    typeof record?.message === "string" ? record.message : "remote workspace request failed";
  const error = new Error(message);
  if (typeof record?.code === "string") {
    Object.assign(error, { code: record.code });
  }
  return error;
}

function readScopedPortEvent(value: unknown): ScopedPortEvent | null {
  const record = asRecord(value);
  const token = record?.token;
  const parsed = scopedServicePortPayloadSchema.safeParse(record?.payload);
  if (typeof token !== "string" || token.length === 0 || !parsed.success) {
    return null;
  }
  return { payload: parsed.data, token };
}

export interface RemoteWorkspaceBridge {
  /** Binds the window attachment as soon as it exists (events may precede the app root). */
  bindPort(input: { port: HostMessagePort; url: string }): () => void;
  /** Registers the window's base services and flushes scoped ports that arrived before them. */
  setBaseServices(port: HostMessagePort, services: IServiceAccessor): void;
  request(method: string, params?: unknown): Promise<unknown>;
  onEvent(event: string, handler: EventHandler): () => void;
}

export function createRemoteWorkspaceBridge(): RemoteWorkspaceBridge {
  const pending = new Map<string, PendingRequest>();
  const handlers = new Map<string, Set<EventHandler>>();
  // 与 ZCode renderer 相同：base services 就绪前到达的 scoped port 先缓存，不能丢弃，
  // 否则刷新时远端会话仍在线，UI 却进入断连状态。
  const earlyPorts: ScopedPortEvent[] = [];
  let base: BaseAttachment | null = null;
  let nextId = 0;

  const failPending = (reason: Error): void => {
    for (const entry of pending.values()) {
      entry.reject(reason);
    }
    pending.clear();
  };

  const emit = (event: string, payload: unknown): void => {
    for (const handler of Array.from(handlers.get(event) ?? [])) {
      try {
        handler(payload);
      } catch (error) {
        logger.warn("remote workspace event handler threw", { event, error });
      }
    }
  };

  const request = (method: string, params?: unknown): Promise<unknown> => {
    const current = base;
    if (current === null) {
      return Promise.reject(
        new Error("Remote workspaces are unavailable until the Host is attached."),
      );
    }
    nextId += 1;
    const id = `rw-${nextId}`;
    return new Promise<unknown>((resolve, reject) => {
      pending.set(id, { resolve, reject });
      current.port.sendControl({ t: "rw.req", id, method, params });
    });
  };

  const registerScopedPort = (
    target: BaseAttachment,
    services: IServiceAccessor,
    event: ScopedPortEvent,
  ): void => {
    const { payload } = event;
    const port = createHostMessagePort({
      url: target.url,
      token: event.token,
      attachmentId: payload.attachmentId,
    });
    const protocol = new MessagePortProtocol(port);
    const client = new ChannelClient(protocol);
    const remoteServices = new RemoteServiceAccess(client);
    let disposed = false;
    const dispose = (reason?: Error): void => {
      if (disposed) {
        return;
      }
      disposed = true;
      client.dispose(reason ?? createRemoteWorkspaceDisconnectedError());
      protocol.disconnect();
      port.close();
    };
    registerRemoteWorkspaceSession({
      sessionId: payload.sessionId,
      ...(payload.sourceServerId ? { sourceServerId: payload.sourceServerId } : {}),
      ...(payload.workspacePath ? { workspacePath: payload.workspacePath } : {}),
      ...(payload.workspaceIdentity ? { workspaceIdentity: payload.workspaceIdentity } : {}),
      ...(payload.workspacePurpose ? { workspacePurpose: payload.workspacePurpose } : {}),
      target: payload.target,
      services: buildRemoteWorkspaceSessionServicesForRegistration({
        baseServices: services,
        remoteServices,
        targetKind: payload.target.kind,
        sourceServerId: payload.sourceServerId,
      }),
      dispose,
    });
    // 与 ZCode 一致：store 登记新代 services 后才确认 ready，bind 返回后调用方读到的是新代 services。
    request("scopedPortReady", {
      sessionId: payload.sessionId,
      attachmentId: payload.attachmentId,
    }).catch((error: unknown) => {
      logger.warn("failed to confirm the remote workspace port", { error });
    });
  };

  const handleControl = (frame: Record<string, unknown>): void => {
    if (frame.t === "rw.res") {
      const id = typeof frame.id === "string" ? frame.id : "";
      const entry = pending.get(id);
      if (entry === undefined) {
        return;
      }
      pending.delete(id);
      if (frame.ok === true) {
        entry.resolve(frame.result);
      } else {
        entry.reject(toError(frame.error));
      }
      return;
    }
    if (frame.t !== "rw.evt" || typeof frame.event !== "string") {
      return;
    }
    if (frame.event === "scoped-service-port") {
      const event = readScopedPortEvent(frame.payload);
      if (event === null) {
        logger.warn("ignored an invalid scoped service port event");
        return;
      }
      if (base?.services) {
        registerScopedPort(base, base.services, event);
      } else {
        earlyPorts.push(event);
      }
      return;
    }
    emit(frame.event, frame.payload);
  };

  return {
    bindPort(input) {
      const attachment: BaseAttachment = { port: input.port, url: input.url, services: null };
      base = attachment;
      const stopControl = input.port.onControl(handleControl);
      const detach = (reason: Error): void => {
        if (base === attachment) {
          base = null;
          earlyPorts.length = 0;
          failPending(reason);
        }
      };
      void input.port.closed.then(detach);
      return () => {
        stopControl();
        detach(new Error("The Host attachment was detached."));
      };
    },
    setBaseServices(port, services) {
      const attachment = base;
      if (attachment === null || attachment.port !== port) {
        return;
      }
      attachment.services = services;
      registerBaseWorkspaceServices(services);
      for (const event of earlyPorts.splice(0)) {
        registerScopedPort(attachment, services, event);
      }
    },
    request,
    onEvent(event, handler) {
      const set = handlers.get(event) ?? new Set<EventHandler>();
      set.add(handler);
      handlers.set(event, set);
      return () => {
        set.delete(handler);
      };
    },
  };
}

/** The window's bridge: the platform adapter and the boot sequence share it. */
export const remoteWorkspaceBridge = createRemoteWorkspaceBridge();
