import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import {
  databaseStartupStateSchema,
  type DatabaseStartupControl,
  type DatabaseStartupState,
} from "@zcode/shared";
import type { CommandError } from "../generated/CommandError.js";
import type { HostAttachmentGrant } from "../generated/HostAttachmentGrant.js";
import type { HostStateDto } from "../generated/HostStateDto.js";
import type { HostStatus } from "../generated/HostStatus.js";

const HOST_OPEN_ATTACHMENT = "host_open_attachment";
const HOST_DATABASE_STARTUP_CONTROL = "host_database_startup_control";
const HOST_STATUS = "host_status";
const HOST_DATABASE_STARTUP_EVENT = "harness://host/database-startup";
const HOST_STATE_EVENT = "harness://host/state";
const HOST_STATES: ReadonlySet<string> = new Set<HostStateDto>(["stopped", "starting", "ready", "failed"]);

/** Typed failure surfaced when a Host shell command or attachment rejects. */
export interface HostCommandFailure {
  code: string;
  message: string;
}

/** Reads the shell Host lifecycle state without spawning the sidecar. */
export function hostStatus(): Promise<HostStatus> {
  return invoke<HostStatus>(HOST_STATUS);
}

/** Spawns the Host on first use and returns the attachment grant and cached startup snapshot. */
export function openHostAttachment(): Promise<HostAttachmentGrant> {
  return invoke<HostAttachmentGrant>(HOST_OPEN_ATTACHMENT);
}

/** Forwards a database startup control command to the Host. */
export function hostDatabaseStartupControl(control: DatabaseStartupControl): Promise<void> {
  return invoke<void>(HOST_DATABASE_STARTUP_CONTROL, { control });
}

/**
 * Subscribes to database startup snapshots.
 *
 * Register this before `openHostAttachment` so no state published while the
 * attachment opens is missed.
 */
export function onHostDatabaseStartup(
  handler: (state: DatabaseStartupState) => void,
): Promise<UnlistenFn> {
  return listen<unknown>(HOST_DATABASE_STARTUP_EVENT, (event) => {
    const parsed = databaseStartupStateSchema.safeParse(event.payload);
    if (parsed.success) {
      handler(parsed.data);
    }
  });
}

/** Subscribes to the shell's Host lifecycle state (`harness://host/state`). */
export function onHostState(handler: (status: HostStatus) => void): Promise<UnlistenFn> {
  return listen<unknown>(HOST_STATE_EVENT, (event) => {
    const status = parseHostStatus(event.payload);
    if (status !== null) {
      handler(status);
    }
  });
}

function parseHostStatus(payload: unknown): HostStatus | null {
  if (typeof payload !== "object" || payload === null) {
    return null;
  }
  const candidate = payload as Partial<Record<keyof HostStatus, unknown>>;
  if (typeof candidate.state !== "string" || !HOST_STATES.has(candidate.state)) {
    return null;
  }
  return {
    state: candidate.state as HostStateDto,
    errorCode: typeof candidate.errorCode === "string" ? candidate.errorCode : null,
    errorMessage: typeof candidate.errorMessage === "string" ? candidate.errorMessage : null,
  };
}

/** Normalizes a rejected command or transport error into a typed surface payload. */
export function describeHostFailure(error: unknown): HostCommandFailure {
  const commandError = asCommandError(error);
  if (commandError !== null) {
    return commandError;
  }
  if (error instanceof Error) {
    return {
      code: error.name === "ConnectionClosed" ? "connectionClosed" : "internal",
      message: error.message,
    };
  }
  return { code: "internal", message: String(error) };
}

function asCommandError(error: unknown): HostCommandFailure | null {
  if (typeof error !== "object" || error === null) {
    return null;
  }
  const candidate = error as Partial<CommandError>;
  if (typeof candidate.code !== "string") {
    return null;
  }
  return {
    code: candidate.code,
    message: typeof candidate.message === "string" ? candidate.message : "",
  };
}
