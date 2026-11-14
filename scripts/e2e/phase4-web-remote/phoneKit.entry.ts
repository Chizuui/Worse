// Gate-only bundle entry (built by phase4-web-remote.mjs into Worse/.work/tmp): the real ZCode relay
// test fixture and the real phone-side transport pieces, re-exported verbatim from the ZCode source.
// The local relay is ZCode's own `createFakeRelayV2` (packages/shared/test/fixtures); no relay was
// written for Harness.
export { createFakeRelayV2 } from "../../../../packages/shared/test/fixtures/fake-relay-v2.ts";
export { createAcknowledgedWebRemoteControlRelayProtocol } from "../../../../packages/client/src/webRemoteControlAcknowledgedRelayProtocol.ts";
export { measureWebRemoteControlRpcRelayEnvelopeBytes } from "../../../../packages/shared/src/web-remote-control-rpc-transport.ts";
export { ChannelClient, MessagePortProtocol } from "../../../../packages/rpc/src/index.ts";
// Fix B (W12): ZCode's own Resource Manager storage clean, used to remove paste attachments the
// same way a ZCode user does (category `toolOutputs`, which owns `<data root>/tmp`).
export { createStorageService } from "../../../../packages/services/src/storage/app/storageService.ts";
export { createFsStorageCleaner } from "../../../../packages/services/src/storage/adapters/fsCleaner.ts";
export { createStorageRootsResolver } from "../../../../packages/services/src/storage/adapters/rootsResolver.ts";
