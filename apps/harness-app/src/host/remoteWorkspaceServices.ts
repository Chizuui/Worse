// Remote workspace service composition, copied from the ZCode desktop renderer
// (packages/desktop/src/renderer/src/remoteWorkspaceSessionServices.ts) so Harness routes exactly the
// same services to the remote Host attachment. Keep in sync with that file.
import type { IHighspeedCardService, IServiceAccessor } from "@zcode/services";
import type { RemoteTarget } from "@zcode/shared";

/**
 * 高速卡与闲时任务只在本地生效（2026-10-21 产品决定），远程 workspace 一律不得路由到远端。
 * 远端合同没有 highspeed-card / off-peak-task 频道，路由过去会等满未知频道超时。
 */
const remoteUnsupportedHighspeedCardService: IHighspeedCardService = {
  prepareTurn: async () => ({ kind: "fallback", reason: "unavailable", nextDrawAt: null }),
  getSnapshot: async () => ({ card: null, nextDrawAt: null, drawing: false }),
  invalidateCard: async () => false,
  healthy: async () => {
    throw new Error("highspeed.remote-unsupported");
  },
};

function withLocalOnlyAccountServices(
  baseServices: IServiceAccessor,
  services: IServiceAccessor,
): IServiceAccessor {
  return {
    ...services,
    highspeedCardService: remoteUnsupportedHighspeedCardService,
    offPeakTaskService: baseServices.offPeakTaskService,
  };
}

/** Legacy (non-Source) attachment: workspace-scoped services remote, account/UI services local. */
function buildRemoteWorkspaceSessionServices(
  baseServices: IServiceAccessor,
  remoteServices: IServiceAccessor,
): IServiceAccessor {
  return withLocalOnlyAccountServices(baseServices, {
    ...baseServices,
    fileService: remoteServices.fileService,
    genUiService: remoteServices.genUiService,
    mediaPreviewService: remoteServices.mediaPreviewService,
    gitService: remoteServices.gitService,
    gitCheckpointService: remoteServices.gitCheckpointService,
    systemService: remoteServices.systemService,
    terminalService: remoteServices.terminalService,
    promptAttachmentTransferService: remoteServices.promptAttachmentTransferService,
    zcodeAgentService: remoteServices.zcodeAgentService,
    zcodeTaskService: remoteServices.zcodeTaskService,
    zcodeSessionService: remoteServices.zcodeSessionService,
    conversationShareService: remoteServices.conversationShareService,
    fileWatcherService: remoteServices.fileWatcherService,
    modelSelectionService: remoteServices.modelSelectionService,
    providerSettingsService: remoteServices.providerSettingsService,
    skillsService: remoteServices.skillsService,
    skillSyncService: remoteServices.skillSyncService,
    mcpSyncService: remoteServices.mcpSyncService,
    pluginSyncService: remoteServices.pluginSyncService,
    userConfigSyncService: remoteServices.userConfigSyncService,
    pluginsService: remoteServices.pluginsService,
    pluginManagementService: remoteServices.pluginManagementService,
    commandsService: remoteServices.commandsService,
    hooksService: remoteServices.hooksService,
  });
}

/**
 * Source Server attachments (or `server` targets) use the full remote service set; only the
 * pre-Source legacy attachment keeps the desktop-attached mixed services.
 */
export function buildRemoteWorkspaceSessionServicesForRegistration(params: {
  baseServices: IServiceAccessor;
  remoteServices: IServiceAccessor;
  targetKind: RemoteTarget["kind"];
  sourceServerId?: string;
}): IServiceAccessor {
  return params.targetKind === "server" || params.sourceServerId?.trim()
    ? withLocalOnlyAccountServices(params.baseServices, params.remoteServices)
    : buildRemoteWorkspaceSessionServices(params.baseServices, params.remoteServices);
}
