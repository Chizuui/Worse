import { invoke } from "@tauri-apps/api/core";
import type { AppInfo } from "../generated/AppInfo.js";
import type { Capabilities } from "../generated/Capabilities.js";
import type { SelectDirectoryResponse } from "../generated/SelectDirectoryResponse.js";
import type { SelectFileResponse } from "../generated/SelectFileResponse.js";

/** Reads shell identity and protocol metadata. */
export function getAppInfo(): Promise<AppInfo> {
  return invoke<AppInfo>("get_app_info");
}

/** Reads the capability flags for the active surface. */
export function getCapabilities(): Promise<Capabilities> {
  return invoke<Capabilities>("get_capabilities");
}

/** Asks the shell to open an allowlisted external URL. */
export function openExternal(url: string): Promise<void> {
  return invoke<void>("open_external", { request: { url } });
}

/** Asks the shell to reveal a path in the native file manager. */
export function revealPath(path: string): Promise<void> {
  return invoke<void>("reveal_path", { request: { path } });
}

/** Opens the native directory picker; the response carries the selection. */
export function selectDirectory(): Promise<SelectDirectoryResponse> {
  return invoke<SelectDirectoryResponse>("select_directory");
}

/** Opens the native file picker; the response carries the selection. */
export function selectFile(): Promise<SelectFileResponse> {
  return invoke<SelectFileResponse>("select_file");
}
