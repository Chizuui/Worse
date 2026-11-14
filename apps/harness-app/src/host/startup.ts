import type { DatabaseStartupState } from "@zcode/shared";

/** Minimal port contract the admission needs to release stale attachments. */
export interface StartupAdmissionPort {
  close(): void;
}

/** Port generation tag carried by an attachment grant. */
export interface StartupPortPayload {
  databaseStartupId: string;
}

interface PendingPort<TPort extends StartupAdmissionPort> {
  startupId: string;
  port: TPort;
}

/**
 * Page-side startup admission.
 *
 * A ready snapshot only completes startup together with the port from the same
 * generation, so one attempt's ready state can never be paired with another
 * attempt's port.
 */
export class StartupAdmission<TPort extends StartupAdmissionPort> {
  private state: DatabaseStartupState | null = null;
  private pending: PendingPort<TPort> | null = null;
  private disposed = false;

  /** Latest accepted snapshot, or null before the first state arrives. */
  get currentState(): DatabaseStartupState | null {
    return this.state;
  }

  /** Accepts a newer snapshot; same-generation older sequences are dropped. */
  acceptState(state: DatabaseStartupState): boolean {
    if (this.disposed) {
      return false;
    }
    const current = this.state;
    if (
      current !== null &&
      current.startupId === state.startupId &&
      current.sequence >= state.sequence
    ) {
      return false;
    }
    if (current !== null && current.startupId !== state.startupId) {
      // 新代快照与旧代端口不能拼接，旧端口必须释放。
      this.dropPending();
    }
    this.state = state;
    return true;
  }

  /** Holds the single service port until its generation reaches ready. */
  acceptPort(payload: StartupPortPayload, port: TPort): void {
    if (this.disposed) {
      port.close();
      return;
    }
    if (this.pending !== null && this.pending.port !== port) {
      this.pending.port.close();
    }
    this.pending = { startupId: payload.databaseStartupId, port };
  }

  /** Returns the port exactly once, and only for the current ready generation. */
  takeReadyPort(): TPort | undefined {
    if (this.disposed || this.state?.phase !== "ready") {
      return undefined;
    }
    const pending = this.pending;
    if (pending === null || pending.startupId !== this.state.startupId) {
      return undefined;
    }
    this.pending = null;
    return pending.port;
  }

  /** Releases the held port; later snapshots and ports are ignored. */
  dispose(): void {
    if (this.disposed) {
      return;
    }
    this.disposed = true;
    this.dropPending();
    this.state = null;
  }

  private dropPending(): void {
    if (this.pending === null) {
      return;
    }
    const { port } = this.pending;
    this.pending = null;
    port.close();
  }
}
