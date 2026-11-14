import type { RustHostProcess } from "../process/rustHostProcess.ts";

export type SessionKind = "pty" | "watch";

/** Receives the notifications of one Rust session (contract r2 section 9.1). */
export interface SessionSink {
  notify(method: string, params: Record<string, unknown>): void;
  /** The harness-host process that owns the session is gone (crash, kill, dispose). */
  hostLost(): void;
}

/**
 * Process-level map from Rust session ids to the attachment-side sink that owns
 * them. harness-host notifications arrive on one shared stdio connection; this is
 * the single place that fans them out. One listener pair is attached per
 * harness-host instance, so a restarted process starts with an empty table and
 * sessions of a dead process are told exactly once.
 */
export interface SessionDirectory {
  /** Registers a session; returns the unregister function (idempotent). */
  register(host: RustHostProcess, kind: SessionKind, rustId: string, sink: SessionSink): () => void;
  /** Live sessions across all hosts (tests, diagnostics). */
  size(): number;
}

function sessionKey(kind: SessionKind, rustId: string): string {
  return `${kind}:${rustId}`;
}

function keyOfNotification(method: string, params: Record<string, unknown>): string | null {
  if (method.startsWith("$/pty/") && typeof params.sessionId === "string") return sessionKey("pty", params.sessionId);
  if (method.startsWith("$/watch/") && typeof params.watchId === "string") return sessionKey("watch", params.watchId);
  return null;
}

export function createSessionDirectory(): SessionDirectory {
  const byHost = new Map<RustHostProcess, Map<string, SessionSink>>();

  const attach = (host: RustHostProcess): Map<string, SessionSink> => {
    const existing = byHost.get(host);
    if (existing !== undefined) return existing;
    const sinks = new Map<string, SessionSink>();
    byHost.set(host, sinks);
    host.connection.onNotification((method, params) => {
      const key = keyOfNotification(method, params);
      if (key !== null) sinks.get(key)?.notify(method, params);
    });
    const lost = (): void => {
      if (byHost.get(host) !== sinks) return;
      byHost.delete(host);
      const orphaned = Array.from(sinks.values());
      sinks.clear();
      for (const sink of orphaned) sink.hostLost();
    };
    // 修复说明：进程崩溃时 stdout 关闭与 exit 事件先后不定；两条路径都可能先到，
    // 这里任一先到即一次性通知所有会话，避免终端/监视器静默挂起（规则 O2）。
    host.connection.onClose(lost);
    void host.exited.then(lost);
    return sinks;
  };

  return {
    register(host, kind, rustId, sink) {
      const sinks = attach(host);
      const key = sessionKey(kind, rustId);
      sinks.set(key, sink);
      return () => {
        if (sinks.get(key) === sink) sinks.delete(key);
      };
    },
    size() {
      let total = 0;
      for (const sinks of byHost.values()) total += sinks.size;
      return total;
    }
  };
}
