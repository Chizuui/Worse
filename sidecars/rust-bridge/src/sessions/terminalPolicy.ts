import { isRecord } from "./types.ts";

/**
 * ZCode's own terminal policy, bundled from the ZCode source at build time
 * (`vendor/terminalPolicy.entry.ts` -> `dist-vendor/terminalPolicy.js`, see
 * `scripts/build-vendor.mjs`). Using the original functions keeps shell, cwd,
 * env and the font/theme profile identical to the TS Host by construction.
 * The bundle is untyped JavaScript, so every value is validated here.
 */

export interface TerminalSettings {
  terminalFontFamily?: string;
  terminalInheritSystemProfile?: boolean;
}

export type TerminalFontFamilySource = "custom" | "system" | "fallback";

export interface TerminalFontProfile {
  fontFamily: string;
  fontSize?: number;
  theme?: Record<string, unknown>;
  source: TerminalFontFamilySource;
}

export interface TerminalWindowsPtyInfo {
  backend: string;
  buildNumber?: number;
}

export interface TerminalPolicy {
  resolveShell(): string;
  resolveCwd(cwd: string | undefined): string;
  resolveEnv(env: NodeJS.ProcessEnv): Record<string, string>;
  resolveFontProfile(settings: TerminalSettings, env: NodeJS.ProcessEnv): TerminalFontProfile;
  resolveWindowsPtyInfo(): TerminalWindowsPtyInfo | undefined;
}

type UnknownFunction = (...args: unknown[]) => unknown;

export class TerminalPolicyError extends Error {
  override readonly name = "TerminalPolicyError";
}

function isFunction(value: unknown): value is UnknownFunction {
  return typeof value === "function";
}

function readFunction(module: Record<string, unknown>, name: string): UnknownFunction {
  const value = module[name];
  if (!isFunction(value)) throw new TerminalPolicyError(`terminal policy bundle lacks ${name}`);
  return value;
}

function expectString(value: unknown, what: string): string {
  if (typeof value !== "string") throw new TerminalPolicyError(`${what} returned a non-string`);
  return value;
}

function readEnv(value: unknown): Record<string, string> {
  if (!isRecord(value)) throw new TerminalPolicyError("resolveTerminalEnv returned a non-object");
  const env: Record<string, string> = {};
  for (const [key, entry] of Object.entries(value)) {
    if (typeof entry === "string") env[key] = entry;
  }
  return env;
}

const FONT_SOURCES: ReadonlySet<string> = new Set(["custom", "system", "fallback"]);

function isFontSource(value: unknown): value is TerminalFontFamilySource {
  return typeof value === "string" && FONT_SOURCES.has(value);
}

function readFontProfile(value: unknown): TerminalFontProfile {
  if (!isRecord(value) || typeof value.fontFamily !== "string" || !isFontSource(value.source)) {
    throw new TerminalPolicyError("resolveTerminalFontProfile returned a malformed profile");
  }
  const { fontSize, theme } = value;
  if (fontSize !== undefined && typeof fontSize !== "number") throw new TerminalPolicyError("fontSize is not a number");
  if (theme !== undefined && !isRecord(theme)) throw new TerminalPolicyError("theme is not an object");
  // 保持与 TS create 相同的字段集合；undefined 字段在 RPC 编码时被省略。
  return { fontFamily: value.fontFamily, fontSize, theme, source: value.source };
}

function readWindowsPtyInfo(value: unknown): TerminalWindowsPtyInfo | undefined {
  if (value === undefined) return undefined;
  if (!isRecord(value) || typeof value.backend !== "string") throw new TerminalPolicyError("malformed windowsPty info");
  const { buildNumber } = value;
  if (buildNumber !== undefined && typeof buildNumber !== "number") throw new TerminalPolicyError("buildNumber is not a number");
  return { backend: value.backend, buildNumber };
}

/** Wraps an untyped policy module after checking its exports. */
export function createTerminalPolicy(module: unknown): TerminalPolicy {
  if (!isRecord(module)) throw new TerminalPolicyError("terminal policy bundle is not a module object");
  const shell = readFunction(module, "resolveTerminalShell");
  const cwd = readFunction(module, "resolveTerminalCwd");
  const env = readFunction(module, "resolveTerminalEnv");
  const font = readFunction(module, "resolveTerminalFontProfile");
  const windowsPty = readFunction(module, "resolveTerminalWindowsPtyInfo");
  return {
    resolveShell: () => expectString(shell(), "resolveTerminalShell"),
    resolveCwd: (requested) => expectString(cwd(requested), "resolveTerminalCwd"),
    resolveEnv: (source) => readEnv(env(source)),
    resolveFontProfile: (settings, source) => readFontProfile(font({ settings, env: source })),
    resolveWindowsPtyInfo: () => readWindowsPtyInfo(windowsPty())
  };
}

/** `dist-vendor/` sits next to `src/` and `dist/`, so the same relative URL works from both. */
const BUNDLE_URL = new URL("../../dist-vendor/terminalPolicy.js", import.meta.url);

let loading: Promise<TerminalPolicy> | null = null;

/** Loads the bundled policy once; a failed load is retried on the next call. */
export function loadTerminalPolicy(): Promise<TerminalPolicy> {
  loading ??= import(BUNDLE_URL.href).then(
    (module: unknown) => createTerminalPolicy(module),
    (error: unknown) => {
      loading = null;
      throw new TerminalPolicyError(`terminal policy bundle unavailable: ${error instanceof Error ? error.message : String(error)}`);
    }
  );
  return loading;
}

/** Settings fields the profile reads, from a `setting.get` result (TS defaults on anything else). */
export function readTerminalSettings(value: unknown): TerminalSettings {
  if (!isRecord(value)) return { terminalInheritSystemProfile: true };
  const settings: TerminalSettings = {};
  if (typeof value.terminalFontFamily === "string") settings.terminalFontFamily = value.terminalFontFamily;
  if (typeof value.terminalInheritSystemProfile === "boolean") {
    settings.terminalInheritSystemProfile = value.terminalInheritSystemProfile;
  }
  return settings;
}
