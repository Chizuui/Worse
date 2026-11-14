// Stand-in for the `electron` module inside the vendored ZCode Main session manager.
// Only the window registry is used at runtime (`BrowserWindow.getAllWindows/fromId/fromWebContents`);
// `MessageChannelMain` is never constructed because the manager receives `createMessageChannel`.

export interface StubWebContents {
  readonly id: number;
  isDestroyed(): boolean;
  send(channel: string, payload: unknown): void;
  postMessage(channel: string, payload: unknown, ports?: unknown[]): void;
}

export interface StubWindow {
  readonly id: number;
  readonly webContents: StubWebContents;
  isDestroyed(): boolean;
}

const windows = new Set<StubWindow>();

/** Registers the sidecar's window adapter; returns the unregister function. */
export function registerStubWindow(win: StubWindow): () => void {
  windows.add(win);
  return () => {
    windows.delete(win);
  };
}

export class BrowserWindow {
  static getAllWindows(): StubWindow[] {
    return Array.from(windows);
  }

  static fromId(id: number): StubWindow | null {
    return Array.from(windows).find((win) => win.id === id) ?? null;
  }

  static fromWebContents(webContents: { id: number }): StubWindow | null {
    return Array.from(windows).find((win) => win.webContents.id === webContents.id) ?? null;
  }
}

export class MessageChannelMain {
  constructor() {
    throw new Error("MessageChannelMain is not available in the Harness sidecar");
  }
}
