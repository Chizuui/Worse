// Signing policy: the single place that decides whether a build signs, skips or fails.
// Spec: Worse/docs/specs/harness/phase-6-packaging.md section 5.1. Secrets are read from the
// environment only and never printed.
import process from "node:process";

const env = (name) => process.env[name]?.trim() || "";

/** release = HARNESS_RELEASE=1 or CI=true; anything else is a local build. */
export function buildMode() {
  return env("HARNESS_RELEASE") === "1" || env("CI").toLowerCase() === "true" ? "release" : "local";
}

export function unsignedAllowed() {
  return env("HARNESS_ALLOW_UNSIGNED") === "1";
}

/** Which Windows credential source is configured, or null. */
export function windowsCredentials() {
  if (env("HARNESS_WINDOWS_SIGN_COMMAND")) return { kind: "command", command: env("HARNESS_WINDOWS_SIGN_COMMAND") };
  if (env("HARNESS_WINDOWS_CERT_THUMBPRINT")) return { kind: "thumbprint", thumbprint: env("HARNESS_WINDOWS_CERT_THUMBPRINT") };
  if (env("HARNESS_WINDOWS_CERT_FILE")) {
    return { kind: "pfx", file: env("HARNESS_WINDOWS_CERT_FILE"), password: process.env.HARNESS_WINDOWS_CERT_PASSWORD ?? "" };
  }
  return null;
}

/** macOS: identity + one complete notarization set (Tauri's APPLE_* variables). */
export function macosCredentials() {
  const identity = env("APPLE_SIGNING_IDENTITY");
  const apiKey = env("APPLE_API_KEY") && env("APPLE_API_ISSUER") && env("APPLE_API_KEY_PATH");
  const appleId = env("APPLE_ID") && env("APPLE_PASSWORD") && env("APPLE_TEAM_ID");
  if (!identity) return null;
  return { kind: "developer-id", identity, notarize: Boolean(apiKey || appleId) };
}

export function linuxCredentials() {
  const key = env("HARNESS_LINUX_GPG_KEY");
  return key ? { kind: "gpg", key } : null;
}

export function credentialsFor(platform = process.platform) {
  if (platform === "win32") return windowsCredentials();
  if (platform === "darwin") {
    const mac = macosCredentials();
    // 发布模式下缺少公证凭据等同于未签名：未公证的 DMG 在 Gatekeeper 下同样无法打开。
    return mac && (mac.notarize || buildMode() === "local") ? mac : null;
  }
  return linuxCredentials();
}

/**
 * Resolves the signing decision: { action: "sign" | "skip", mode, credentials, reason }.
 * Release without credentials and without HARNESS_ALLOW_UNSIGNED=1 is { action: "fail" }.
 */
export function signingDecision(platform = process.platform) {
  const mode = buildMode();
  const credentials = credentialsFor(platform);
  if (credentials) return { action: "sign", mode, credentials, reason: `${credentials.kind} credentials` };
  if (mode === "local") return { action: "skip", mode, credentials: null, reason: "local build without signing credentials" };
  if (unsignedAllowed()) return { action: "skip", mode, credentials: null, reason: "UNSIGNED BUILD (HARNESS_ALLOW_UNSIGNED=1)" };
  return {
    action: "fail",
    mode,
    credentials: null,
    reason:
      `release build without ${platform} signing credentials; configure them (see ` +
      "Worse/docs/specs/harness/phase-6-packaging.md section 5) or set HARNESS_ALLOW_UNSIGNED=1 for a test build",
  };
}

/** Prints the decision and exits non-zero on "fail". Returns the decision otherwise. */
export function enforceSigningPolicy(platform = process.platform, label = "signing") {
  const decision = signingDecision(platform);
  if (decision.action === "fail") {
    process.stderr.write(`error: ${label}: ${decision.reason}\n`);
    process.exit(1);
  }
  const line = decision.action === "skip" && decision.mode === "release" ? decision.reason : `${decision.action} (${decision.reason})`;
  process.stdout.write(`${label}: ${line}\n`);
  return decision;
}
