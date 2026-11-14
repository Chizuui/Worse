// Typed view of dist-vendor/remoteWorkspace.js (ZCode Main's remote session manager and helpers,
// bundled unchanged by scripts/build-vendor.mjs). Types are the subset this sidecar relies on.
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

export type Json = Record<string, unknown>;

export interface SafeParse<T = unknown> {
  safeParse(value: unknown): { success: true; data: T } | { success: false; error: unknown };
}

export interface RemoteAssetDirs {
  mockCdnDir?: string;
  remoteCdnBaseUrl?: string;
  remoteCdnBaseUrls?: string[];
  remoteCacheDir?: string;
  sourceServerArtifactPolicy?: "development-content-addressed" | "release-versioned";
}

export interface ProviderProvisioningLaneStatus {
  lane: string;
  [key: string]: unknown;
}

/** The manager surface returned by `createRemoteWorkspaceSessionManager` that Harness uses. */
export interface RemoteWorkspaceSessionManager {
  createRemoteWorkspaceSession(
    win: unknown,
    target: Json,
    requestId?: string,
    context?: { workspacePath?: string; workspaceIdentity?: string; confirmSourceServerInstall?: boolean },
    lifecycle?: { remoteUsageTelemetryEligible?: boolean; onConnected?: (descriptor: Json) => void }
  ): Promise<string>;
  bindRemoteWorkspaceSessionContext(
    sessionId: string,
    context: { workspacePath: string; workspaceIdentity?: string; workspacePurpose?: string },
    expectedWebContentsId?: number
  ): Promise<unknown>;
  confirmRendererAttachmentReady(webContentsId: number, payload: { sessionId: string; attachmentId: string }): void;
  reattachRemoteWorkspaceSessionsForWindow(win: unknown, reason: string): void;
  disposeRemoteWorkspaceSession(sessionId: string, reason: string, signalGracePeriodMs?: number): void;
  disposeRemoteWorkspaceSessionsForWindow(webContentsId: number, reason?: string): void;
  cancelPendingRemoteWorkspaceSessionsForWindow(webContentsId: number, reason: string, requestId?: string): void;
  disposeAllAndWaitForAppShutdown(reason: string): Promise<void>;
  getProviderProvisioningLaneStatus(
    sourceServerId: string,
    options?: { waitForIdle?: boolean }
  ): Promise<ProviderProvisioningLaneStatus>;
  retryProviderProvisioning(sourceServerId: string): Promise<ProviderProvisioningLaneStatus>;
  attachRemoteWorkspaceSessionHost(params: Json): { process: unknown; port: unknown; remoteKind: string };
  prepareProjectWorkspace(win: unknown, project: Json, requestId: string): Promise<unknown>;
  getRemoteConnectionStats(): { activeSessionCount: number; activeTargetCount: number };
  [key: string]: unknown;
}

export interface RemoteWorkspaceVendor {
  createRemoteWorkspaceSessionManager(options: Json): RemoteWorkspaceSessionManager;
  createCanonicalWslTargetResolver(options?: Json): (target: Json) => Promise<Json>;
  resolveRemoteCdnBaseUrls(options: {
    env?: string;
    locale?: string;
    overrideBaseUrl?: string;
    version?: string;
  }): string[];
  isDockerAvailable(): Promise<boolean>;
  listDockerContainers(): Promise<unknown[]>;
  listWSLDistros(): Promise<unknown[]>;
  listSSHConfigAliasesFromLocalConfig(): Promise<unknown[]>;
  formatZodError(error: unknown): string;
  hostRemoteWorkspaceConnectFailedResponseSchema: { shape: { profileId: SafeParse<string | undefined>; resolvedWslTarget: SafeParse<Json | undefined> } };
  remoteTargetSchema: SafeParse<Json & { kind: string }>;
  normalizeRemoteConnectionError(error: unknown): { message: string; code?: string };
  isPublicZCodeErrorCode(value: unknown): boolean;
  normalizeUnknownError(error: unknown): { message: string; code?: string };
  ZCODE_VERSION: string;
  ZCODE_ENV: string;
  registerStubWindow(win: unknown): () => void;
}

/** Loads the bundle; src/ (tests) and dist/ (runtime) sit at the same depth as dist-vendor/. */
export async function loadRemoteWorkspaceVendor(): Promise<RemoteWorkspaceVendor> {
  const here = path.dirname(fileURLToPath(import.meta.url));
  const bundle = path.resolve(here, "..", "dist-vendor", "remoteWorkspace.js");
  return (await import(pathToFileURL(bundle).href)) as RemoteWorkspaceVendor;
}
