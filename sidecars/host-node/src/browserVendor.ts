// Typed view of dist-vendor/browserRuntime.js: ZCode Main's BrowserGuestManager (bundled unchanged by
// scripts/build-browser-vendor.mjs). Types are the subset this sidecar relies on.
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

export type BrowserClientMode = "desktop-continuous" | "web-remote-replayable";

export interface BrowserExecutionContext {
  controlScopeId?: string;
  requestId: string;
  browserId: string;
  browserGeneration: number;
  windowId: number;
  workspaceKey: string;
  sessionId: string;
  turnId?: string;
  remoteSessionId?: string;
  clientMode: BrowserClientMode;
}

export interface BrowserCommandLike {
  method: string;
  [key: string]: unknown;
}

export interface BrowserCommandResultLike {
  ok: boolean;
  error?: { code: string; message: string; sideEffect?: string };
  elapsedMs: number;
  [key: string]: unknown;
}

export interface BrowserViewport {
  width: number;
  height: number;
}

export interface ScreenshotSurfaceLease {
  readonly invalidated?: AbortSignal;
  surfaceScale: number;
  webContentsId: number;
  viewport: BrowserViewport;
  release(): void;
}

export interface ScreenshotSurfaceCoordinator {
  prepare(input: { webContentsId: number; viewport: BrowserViewport; signal: AbortSignal }): Promise<ScreenshotSurfaceLease>;
}

export interface BrowserGuestManagerResidencyOptions {
  warn?: (message: string) => void;
  recording?: { createRecorder?: unknown; tempRoot?: string };
}

export interface BrowserGuestAttachResult {
  ok: boolean;
  reason?: string;
  recoveryRequested?: boolean;
}

export interface BrowserGuestManager {
  getBackendIdentity(windowId: number): { id: string; generation: number };
  attachGuest(
    tabId: string,
    webContentsId: number,
    options?: {
      active?: boolean;
      windowId?: number;
      workspaceKey?: string;
      remoteSessionId?: string;
      sessionId?: string;
      residencyGeneration?: number;
    }
  ): BrowserGuestAttachResult;
  execute(context: BrowserExecutionContext, command: BrowserCommandLike, signal?: AbortSignal): Promise<BrowserCommandResultLike>;
  getTabOwner(tabId: string): BrowserExecutionContext | null;
  listGuestWebContentsIds(): number[];
  closeWindow(windowId: number): void;
  disposeAll(): void;
}

export interface BrowserGuestManagerConstructor {
  new (
    log?: (message: string) => void,
    attachTimeoutMs?: number,
    onCloseTabRequested?: (tabId: string, owner?: BrowserExecutionContext) => void,
    onOpenTabRequested?: (tabId: string, owner: BrowserExecutionContext) => void,
    onVisibilityChanged?: (visible: boolean, owner: BrowserExecutionContext, tabId?: string) => void,
    onViewportChanged?: (viewport: BrowserViewport | null, owner: BrowserExecutionContext, tabId: string) => void,
    resizeScreenshotToCssPixels?: (base64Png: string, target: BrowserViewport) => Promise<string | undefined> | string | undefined,
    screenshotSurfaceCoordinator?: ScreenshotSurfaceCoordinator,
    residencyOptions?: BrowserGuestManagerResidencyOptions,
    resolveOwnerWindow?: (windowId: number) => null,
    cdpIdleReleaseMs?: number
  ): BrowserGuestManager;
}

export interface BrowserControlMainBridge {
  list(): Promise<Array<Record<string, unknown>>>;
  execute(input: {
    requestId?: string;
    sessionId: string;
    turnId?: string;
    workspaceKey?: string;
    workspacePath?: string;
    workspaceIdentity?: string;
    remoteSessionId?: string;
    clientMode?: BrowserClientMode;
    command: BrowserCommandLike;
  }): Promise<BrowserCommandResultLike>;
  handleResult(message: { requestId: string; result: BrowserCommandResultLike }): Promise<void>;
  dispose(): void;
}

export interface BrowserRuntimeVendor {
  BrowserGuestManager: BrowserGuestManagerConstructor;
  createBrowserControlMainBridge(deps: {
    getBackendIdentity: () => { id: string; generation: number } | undefined;
    postToMain: (message: Record<string, unknown>) => void;
  }): BrowserControlMainBridge;
  browserCommandResultSchema: { safeParse(value: unknown): { success: boolean; error?: { message: string } } };
  browserCommandSchema: { safeParse(value: unknown): { success: boolean; error?: { message: string } } };
  registerGuestWebContents(guest: { readonly id: number; isDestroyed(): boolean }): () => void;
}

/** Loads the bundle; src/ (tests, gates) and dist/ (runtime) sit at the same depth as dist-vendor/. */
export async function loadBrowserRuntimeVendor(): Promise<BrowserRuntimeVendor> {
  const here = path.dirname(fileURLToPath(import.meta.url));
  const bundle = path.resolve(here, "..", "dist-vendor", "browserRuntime.js");
  return (await import(pathToFileURL(bundle).href)) as BrowserRuntimeVendor;
}
