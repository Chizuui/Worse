// Fake Host sidecar for harness-host-control tests. It speaks the NDJSON
// bridge protocol only; no WebSocket server and no real Host are involved.
// FAKE_MODE: "ok" (default) | "fatal" | "reject" | "exit-after-ready" | "usage-silent".
import { createInterface } from "node:readline";

const mode = process.env.FAKE_MODE ?? "ok";
const send = (line) => process.stdout.write(`${JSON.stringify({ v: 1, ...line })}\n`);
const host = (message) => send({ t: "host", message });

if (mode === "fatal") {
  send({ t: "fatal", code: "host_import_failed", message: "fake import failure" });
  process.exit(2);
}

send({ t: "log", level: "info", message: "fake sidecar booting" });
send({ t: "ready", hostPid: process.pid, wsPort: 45123, protocol: 1 });
if (mode === "exit-after-ready") setTimeout(() => process.exit(3), 200);

let startupId = "";
let sequence = 0;
const publishStartup = (phase) => {
  sequence += 1;
  host({
    type: "database-startup-state",
    state: { schemaVersion: 1, startupId, sequence, phase },
  });
};

const lines = createInterface({ input: process.stdin });
lines.on("line", (raw) => {
  const line = JSON.parse(raw);
  if (line.t === "attach.open") {
    if (mode === "reject") {
      send({ t: "attach.failed", requestId: line.requestId, code: "attach_rejected", message: "no" });
      return;
    }
    send({ t: "attach.opened", requestId: line.requestId, attachmentId: line.attachmentId });
    const type = line.hostMessage.type;
    // Report what the shell asked for so tests can assert init vs reattach.
    host({ type: "agent-process-ready", echoHostMessageType: type, tokenLength: line.token.length });
    send({ t: "attach.active", attachmentId: line.attachmentId, messageType: type });
    if (type === "init-local") {
      startupId = line.hostMessage.databaseStartupId;
      host({ type: "database-startup-state", state: { startupId: "foreign", sequence: 99 } });
      publishStartup("starting");
      publishStartup("ready");
      host({ type: "browser-execute-request", requestId: "browser-1" });
    }
    return;
  }
  if (line.t === "host") {
    const message = line.message;
    if (message.type === "dispose") process.exit(0);
    // Resource usage: answer like the Host responder unless the mode keeps the request in flight.
    if (message.type === "resource-usage-snapshot-request" && mode !== "usage-silent") {
      host({ type: "resource-usage-snapshot-result", requestId: "stale-id", sampledAt: 1, processes: [] });
      host({
        type: "resource-usage-snapshot-result",
        requestId: message.requestId,
        sampledAt: 1700000000000,
        processes: [
          {
            pid: process.pid,
            name: "node",
            category: "base",
            groupKey: "host",
            groupLabel: "Host",
            cpuPercent: 0.5,
            memoryBytes: 1048576,
          },
        ],
      });
      return;
    }
    // Echo every shell reply back as an observed event.
    host({ type: "agent-process-spawned", echo: message });
  }
});
lines.on("close", () => process.exit(0));
