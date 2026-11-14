// Docker / WSL / SSH target listings for the phone's `platform-request`s.
//
// ZCode Main answered these from the desktop OS with `@zcode/server/remote` (`docker-detect`,
// `wsl-detect`) and `listSSHConfigAliasesFromLocalConfig` (packages/desktop/src/main/
// desktopRuntimeEnv.ts). Harness runs the very same functions, from the vendored
// dist-vendor/remoteWorkspace.js bundle that Phase 4A already loads for the desktop's own
// discovery (remoteWorkspaceIpc.ts); nothing is reimplemented. Like Main's `platformHandlers`, errors
// are not swallowed: the web remote manager answers `platform-response {success: false}`.
// See docs/specs/harness/phase-4-web-remote-control.md section 5.6.
import { loadRemoteWorkspaceVendor, type RemoteWorkspaceVendor } from "./remoteWorkspaceVendor.ts";

export interface RemoteTargetDiscovery {
  isDockerAvailable(): Promise<boolean>;
  listWSLDistros(): Promise<unknown[]>;
  listDockerContainers(): Promise<unknown[]>;
  listSSHConfigAliases(): Promise<unknown[]>;
}

export function createRemoteTargetDiscovery(
  loadVendor: () => Promise<RemoteWorkspaceVendor> = loadRemoteWorkspaceVendor
): RemoteTargetDiscovery {
  let vendor: Promise<RemoteWorkspaceVendor> | undefined;
  // 打包加载失败时不缓存失败结果，下一次请求重新尝试；失败本身作为错误回给手机（fail closed），不伪装成空列表。
  const load = (): Promise<RemoteWorkspaceVendor> => {
    vendor ??= loadVendor().catch((error: unknown) => {
      vendor = undefined;
      throw error;
    });
    return vendor;
  };
  return {
    isDockerAvailable: async () => (await load()).isDockerAvailable(),
    listWSLDistros: async () => (await load()).listWSLDistros(),
    listDockerContainers: async () => (await load()).listDockerContainers(),
    listSSHConfigAliases: async () => (await load()).listSSHConfigAliasesFromLocalConfig()
  };
}
