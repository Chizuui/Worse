import { createHash, timingSafeEqual } from "node:crypto";
import type { SidecarErrorCode } from "./protocol.ts";

export interface AttachmentRegistrationInput {
  attachmentId: string;
  token: string;
  ttlMs: number;
}

export type AttachmentClaim =
  | { ok: true; attachmentId: string }
  | { ok: false; code: SidecarErrorCode };

export interface AttachmentRegistry {
  register(input: AttachmentRegistrationInput): void;
  claim(input: { attachmentId: string; token: string }): AttachmentClaim;
  release(attachmentId: string): void;
  close(): void;
}

export interface AttachmentRegistryOptions {
  now?: () => number;
  onExpired: (attachmentId: string) => void;
}

interface AttachmentEntry {
  tokenDigest: Buffer;
  expiresAt: number;
  active: boolean;
  timer: NodeJS.Timeout;
}

function digestToken(token: string): Buffer {
  return createHash("sha256").update(token, "utf8").digest();
}

function tokenMatches(expected: Buffer, actual: string): boolean {
  return timingSafeEqual(expected, digestToken(actual));
}

export function createAttachmentRegistry(options: AttachmentRegistryOptions): AttachmentRegistry {
  const now = options.now ?? Date.now;
  const entries = new Map<string, AttachmentEntry>();

  const clearEntry = (attachmentId: string): void => {
    const entry = entries.get(attachmentId);
    if (!entry) return;
    clearTimeout(entry.timer);
    entries.delete(attachmentId);
  };

  const register = (input: AttachmentRegistrationInput): void => {
    clearEntry(input.attachmentId);
    const ttlMs = Number.isFinite(input.ttlMs) && input.ttlMs > 0 ? input.ttlMs : 0;
    const timer = setTimeout(() => {
      const entry = entries.get(input.attachmentId);
      if (!entry || entry.active) return;
      entries.delete(input.attachmentId);
      options.onExpired(input.attachmentId);
    }, ttlMs);
    timer.unref();
    entries.set(input.attachmentId, {
      tokenDigest: digestToken(input.token),
      expiresAt: now() + ttlMs,
      active: false,
      timer
    });
  };

  const claim = (input: { attachmentId: string; token: string }): AttachmentClaim => {
    const entry = entries.get(input.attachmentId);
    if (!entry) return { ok: false, code: "attach_unknown" };
    if (entry.active) return { ok: false, code: "attach_duplicate" };
    if (now() > entry.expiresAt) {
      clearEntry(input.attachmentId);
      return { ok: false, code: "attach_expired" };
    }
    if (!tokenMatches(entry.tokenDigest, input.token)) {
      return { ok: false, code: "ws_token_rejected" };
    }
    entry.active = true;
    clearTimeout(entry.timer);
    return { ok: true, attachmentId: input.attachmentId };
  };

  const release = (attachmentId: string): void => {
    clearEntry(attachmentId);
  };

  const close = (): void => {
    for (const entry of entries.values()) clearTimeout(entry.timer);
    entries.clear();
  };

  return { register, claim, release, close };
}
