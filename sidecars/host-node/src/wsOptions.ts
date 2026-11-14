const DEFAULT_ALLOWED_ORIGINS: readonly string[] = [
  "tauri://localhost",
  "http://tauri.localhost",
  "https://tauri.localhost",
  "http://localhost:1430",
  "http://127.0.0.1:1430"
];

const DEFAULT_HELLO_TIMEOUT_MS = 5_000;
const DEFAULT_MAX_PAYLOAD_BYTES = 64 * 1024 * 1024;
const DEFAULT_MAX_BUFFERED_BYTES = 32 * 1024 * 1024;

export interface AttachmentServerOptions {
  allowedOrigins?: readonly string[];
  allowNoOrigin?: boolean;
  helloTimeoutMs?: number;
  maxPayloadBytes?: number;
  maxBufferedBytes?: number;
}

export interface ResolvedServerOptions {
  allowedOrigins: ReadonlySet<string>;
  allowNoOrigin: boolean;
  helloTimeoutMs: number;
  maxPayloadBytes: number;
  maxBufferedBytes: number;
}

function parsePositiveInt(raw: string | undefined): number | null {
  if (raw === undefined || raw.trim() === "") return null;
  const value = Number(raw);
  return Number.isFinite(value) && value > 0 ? Math.floor(value) : null;
}

function originsFromEnv(): string[] {
  return (process.env.HARNESS_ALLOWED_ORIGINS ?? "")
    .split(",")
    .map((value) => value.trim())
    .filter((value) => value.length > 0);
}

/** Merges explicit options over the documented environment variables and defaults. */
export function resolveServerOptions(options: AttachmentServerOptions): ResolvedServerOptions {
  const env = process.env;
  return {
    allowedOrigins: new Set<string>([
      ...DEFAULT_ALLOWED_ORIGINS,
      ...(options.allowedOrigins ?? []),
      ...originsFromEnv()
    ]),
    allowNoOrigin: options.allowNoOrigin ?? env.HARNESS_ALLOW_NO_ORIGIN === "1",
    helloTimeoutMs: options.helloTimeoutMs ?? DEFAULT_HELLO_TIMEOUT_MS,
    maxPayloadBytes:
      options.maxPayloadBytes ?? parsePositiveInt(env.HARNESS_MAX_PAYLOAD_BYTES) ?? DEFAULT_MAX_PAYLOAD_BYTES,
    maxBufferedBytes:
      options.maxBufferedBytes ??
      parsePositiveInt(env.HARNESS_MAX_BUFFERED_BYTES) ??
      DEFAULT_MAX_BUFFERED_BYTES
  };
}

/** Exact-match origin check; a missing Origin passes only in headless mode. */
export function isOriginAllowed(origin: string | undefined, options: ResolvedServerOptions): boolean {
  if (origin === undefined || origin === "") return options.allowNoOrigin;
  return options.allowedOrigins.has(origin);
}
