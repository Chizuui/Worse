const { parentPort } = process;

if (!parentPort) {
  process.stderr.write("fake host requires process.parentPort\n");
  process.exit(3);
}

const startedAt = Date.now();

parentPort.on("message", (event) => {
  const message = event?.data;
  if (!message || typeof message !== "object") return;

  if (message.type === "dispose") {
    process.exit(0);
  }

  if (message.type !== "init-local" && message.type !== "attach-service-port") return;

  const port = event.ports?.[0];
  if (!port) {
    parentPort.postMessage({ type: "fake-host-error", message: "missing port" });
    return;
  }

  port.on("message", (inner) => {
    const payload = inner?.data;
    if (payload instanceof Uint8Array) {
      port.postMessage(payload);
      return;
    }
    if (payload && typeof payload === "object" && "__zcodeRpcControl" in payload) {
      return;
    }
    port.postMessage(new TextEncoder().encode("unexpected"));
  });
  port.start();

  if (message.type === "init-local") {
    parentPort.postMessage({
      type: "database-startup-state",
      state: {
        databaseStartupId: message.databaseStartupId ?? null,
        phase: "ready",
        attemptId: "fake-attempt",
        startedAt,
        updatedAt: Date.now()
      }
    });
  }

  parentPort.postMessage({
    type: "fake-host-attached",
    messageType: message.type,
    attachmentId: message.attachmentId ?? null
  });
});
