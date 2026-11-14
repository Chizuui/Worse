import { RemoteServiceAccess } from "@zcode/client";
import { ChannelClient, MessagePortProtocol, type MessagePortLike } from "@zcode/rpc";
import type { IServiceAccessor } from "@zcode/services";

/** Host-backed service accessor plus the one-shot connection teardown. */
export interface HostServicesConnection {
  services: IServiceAccessor;
  dispose(reason?: Error): void;
}

/**
 * Connects the shared UI to the Host over one attached message port.
 *
 * `dispose` is idempotent: it releases the channel client and detaches the
 * protocol so pending calls fail closed on a renderer reload.
 */
export function connectHostServices(port: MessagePortLike): HostServicesConnection {
  const protocol = new MessagePortProtocol(port);
  const client = new ChannelClient(protocol);
  const services = new RemoteServiceAccess(client, {
    sourceServerManagementService: true,
    deviceSyncStateService: true,
  });

  let disposed = false;
  return {
    services,
    dispose(reason?: Error): void {
      if (disposed) {
        return;
      }
      disposed = true;
      client.dispose(reason);
      protocol.disconnect();
    },
  };
}
