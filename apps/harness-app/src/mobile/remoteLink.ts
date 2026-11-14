/**
 * Validates the web remote-control link a phone opens after pairing
 * (Phase 4, spec `Worse/docs/specs/harness/phase-4-web-remote-control.md`).
 *
 * The desktop shows the link as a QR code: `<origin>/remote/v3|v4?sid=<deviceSid>`.
 * The mobile shell only navigates its webview to links that pass every check,
 * so an arbitrary page can never be loaded in place of the remote client
 * (fail closed).
 */

/** The ZCode production endpoint; the only origin allowed without configuration. */
export const DEFAULT_REMOTE_ORIGIN = "https://zcode.z.ai";

const REMOTE_PATH = /^\/remote\/v[34]\/?$/u;
const LOOPBACK_HOSTS = new Set(["localhost", "127.0.0.1", "10.0.2.2"]);

export type RemoteLinkResult =
  | { readonly ok: true; readonly url: string }
  | { readonly ok: false; readonly reason: RemoteLinkRejection };

export type RemoteLinkRejection = "empty" | "invalid" | "origin" | "insecure" | "path" | "sid";

/**
 * Reads the allowed origins: the production endpoint plus a comma-separated,
 * build-time `VITE_HARNESS_REMOTE_ORIGINS` list (used for local relay tests).
 */
export function readAllowedRemoteOrigins(extra: string | undefined): ReadonlySet<string> {
  const origins = new Set([DEFAULT_REMOTE_ORIGIN]);
  for (const entry of (extra ?? "").split(",")) {
    const trimmed = entry.trim();
    if (trimmed.length === 0) {
      continue;
    }
    try {
      origins.add(new URL(trimmed).origin);
    } catch {
      // 配置里无法解析的项直接忽略，不扩大白名单。
    }
  }
  return origins;
}

/** Checks one pasted or scanned link against the allowlist. */
export function validateRemoteLink(raw: string, allowedOrigins: ReadonlySet<string>): RemoteLinkResult {
  const text = raw.trim();
  if (text.length === 0) {
    return { ok: false, reason: "empty" };
  }
  let url: URL;
  try {
    url = new URL(text);
  } catch {
    return { ok: false, reason: "invalid" };
  }
  if (url.username !== "" || url.password !== "") {
    return { ok: false, reason: "invalid" };
  }
  if (!allowedOrigins.has(url.origin)) {
    return { ok: false, reason: "origin" };
  }
  // 只有显式加入白名单的本机回环地址（模拟器测试）可以用 http，其余必须 https。
  if (url.protocol !== "https:" && !(url.protocol === "http:" && LOOPBACK_HOSTS.has(url.hostname))) {
    return { ok: false, reason: "insecure" };
  }
  if (!REMOTE_PATH.test(url.pathname)) {
    return { ok: false, reason: "path" };
  }
  const sid = url.searchParams.get("sid")?.trim() ?? "";
  if (sid.length === 0) {
    return { ok: false, reason: "sid" };
  }
  url.hash = "";
  return { ok: true, url: url.toString() };
}
