// Remote runtime asset locations for `init-local` and `connect-remote-workspace`, mirroring ZCode
// Main's `resolveRemoteAssetDirs` (desktopRuntimeEnv.ts) for the Harness layout.
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import path from "node:path";
import type { RemoteAssetDirs, RemoteWorkspaceVendor } from "./remoteWorkspaceVendor.ts";

function envValue(env: NodeJS.ProcessEnv, name: string): string | undefined {
  const value = env[name]?.trim();
  return value === undefined || value === "" ? undefined : value;
}

function resolveCacheDir(env: NodeJS.ProcessEnv): string {
  const override = envValue(env, "ZCODE_REMOTE_ASSET_CACHE_DIR");
  if (override !== undefined) return path.resolve(override);
  const base =
    envValue(env, "HARNESS_APP_DATA_DIR") ?? envValue(env, "ZCODE_DATA_BASE_DIR") ?? path.join(homedir(), ".harness");
  return path.join(base, "remote-assets-cache");
}

/**
 * CDN URLs follow ZCode's ordering (domestic first only for a zh locale in UTC+8) and the
 * `ZCODE_REMOTE_ASSET_CDN_BASE_URL` override. The development policy is used only when the dev
 * bootstrap's mock-cdn release directory exists; otherwise the release-versioned CDN path.
 */
export function resolveRemoteAssetDirs(
  vendor: RemoteWorkspaceVendor,
  input: { env: NodeJS.ProcessEnv; cwd: string; locale: string }
): RemoteAssetDirs {
  const runtimeEnv = envValue(input.env, "ZCODE_ENV")?.toLowerCase() === "test" ? "test" : vendor.ZCODE_ENV;
  const remoteCdnBaseUrls = vendor.resolveRemoteCdnBaseUrls({
    env: runtimeEnv,
    locale: input.locale,
    overrideBaseUrl: envValue(input.env, "ZCODE_REMOTE_ASSET_CDN_BASE_URL"),
    version: vendor.ZCODE_VERSION
  });
  const base: RemoteAssetDirs = {
    remoteCdnBaseUrl: remoteCdnBaseUrls[0],
    remoteCdnBaseUrls,
    remoteCacheDir: resolveCacheDir(input.env)
  };
  const mockCdnDir = path.resolve(input.cwd, "packages", "desktop", "mock-cdn");
  if (existsSync(path.join(mockCdnDir, "releases", vendor.ZCODE_VERSION))) {
    return { mockCdnDir, sourceServerArtifactPolicy: "development-content-addressed", ...base };
  }
  return base;
}
