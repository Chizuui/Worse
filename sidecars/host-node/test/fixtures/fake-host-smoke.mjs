import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { fileURLToPath } from "node:url";
import WebSocket from "ws";

const SIDECAR_MAIN =
  process.env.HARNESS_SIDECAR_MAIN ??
  fileURLToPath(new URL("../../dist/main.js", import.meta.url));
const FAKE_HOST = fileURLToPath(new URL("./fake-host.mjs", import.meta.url));
const ORIGIN = "http://localhost:1430";
const TIMEOUT_MS = 15000;

function fail(message) {
  process.stderr.write(`SMOKE FAIL: ${message}\n`);
  process.exitCode = 1;
}

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

const child = spawn(process.execPath, ["--no-warnings", SIDECAR_MAIN], {
  env: {
    ...process.env,
    HARNESS_HOST_ENTRY: FAKE_HOST,
    HARNESS_LOG_LEVEL: "warn",
    HARNESS_SHUTDOWN_GRACE_MS: "3000"
  },
  stdio: ["pipe", "pipe", "pipe"]
});

let stderr = "";
child.stderr.setEncoding("utf8");
child.stderr.on("data", (chunk) => {
  stderr += chunk;
});

const lines = [];
const waiters = [];

function waitForLine(label, predicate, timeoutMs = TIMEOUT_MS) {
  const existing = lines.find(predicate);
  if (existing) return Promise.resolve(existing);
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      const index = waiters.findIndex((entry) => entry.resolve === wrappedResolve);
      if (index >= 0) waiters.splice(index, 1);
      reject(new Error(`timed out waiting for ${label}`));
    }, timeoutMs);
    const wrappedResolve = (value) => {
      clearTimeout(timer);
      resolve(value);
    };
    waiters.push({ predicate, resolve: wrappedResolve });
  });
}

let stdoutBuffer = "";
child.stdout.setEncoding("utf8");
child.stdout.on("data", (chunk) => {
  stdoutBuffer += chunk;
  let index = stdoutBuffer.indexOf("\n");
  while (index >= 0) {
    const line = stdoutBuffer.slice(0, index).trim();
    stdoutBuffer = stdoutBuffer.slice(index + 1);
    if (line.length > 0) {
      let parsed;
      try {
        parsed = JSON.parse(line);
      } catch {
        fail(`stdout is not NDJSON: ${line.slice(0, 200)}`);
        continue;
      }
      lines.push(parsed);
      for (const waiter of waiters.slice()) {
        if (waiter.predicate(parsed)) {
          waiters.splice(waiters.indexOf(waiter), 1);
          waiter.resolve(parsed);
        }
      }
    }
    index = stdoutBuffer.indexOf("\n");
  }
});

function nextMessage(client) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("websocket message timeout")), TIMEOUT_MS);
    client.once("message", (data, isBinary) => {
      clearTimeout(timer);
      resolve({ data, isBinary });
    });
  });
}

function waitOpen(client) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("websocket open timeout")), TIMEOUT_MS);
    client.once("open", () => {
      clearTimeout(timer);
      resolve();
    });
    client.once("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
  });
}

const exitPromise = new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error("sidecar did not exit")), TIMEOUT_MS);
  child.once("exit", (code, signal) => {
    clearTimeout(timer);
    resolve({ code, signal });
  });
});

try {
  const ready = await waitForLine("ready", (line) => line.t === "ready");
  assert(ready.v === 1, "ready line must carry v=1");
  assert(ready.protocol === 1, "ready line must carry protocol=1");
  assert(typeof ready.wsPort === "number" && ready.wsPort > 0, "ready line must carry wsPort");

  const attachmentId = randomUUID();
  const token = "ab".repeat(32);
  const requestId = randomUUID();

  child.stdin.write(
    `${JSON.stringify({
      v: 1,
      t: "attach.open",
      requestId,
      attachmentId,
      token,
      ttlMs: 10000,
      hostMessage: {
        type: "init-local",
        databaseStartupId: randomUUID(),
        locale: "en-US",
        agentSpawnFallbackCwd: process.cwd()
      }
    })}\n`
  );

  const opened = await waitForLine("attach.opened", (line) => line.t === "attach.opened");
  assert(opened.attachmentId === attachmentId, "attach.opened must echo the attachment id");
  assert(opened.requestId === requestId, "attach.opened must echo the request id");

  const client = new WebSocket(`ws://127.0.0.1:${ready.wsPort}/v1/attach`, {
    headers: { Origin: ORIGIN }
  });

  await waitOpen(client);
  client.send(JSON.stringify({ t: "hello", attachmentId, token }));

  const attached = await nextMessage(client);
  assert(attached.isBinary === false, "attached frame must be text");
  assert(attached.data.toString() === '{"t":"attached"}', "server must reply attached");

  const active = await waitForLine("attach.active", (line) => line.t === "attach.active");
  assert(active.attachmentId === attachmentId, "attach.active must carry the attachment id");
  assert(active.messageType === "init-local", "first attachment must be init-local");

  const startupState = await waitForLine(
    "database-startup-state",
    (line) => line.t === "host" && line.message?.type === "database-startup-state"
  );
  assert(startupState.message.state.phase === "ready", "fake host must report a ready startup state");

  const payload = new Uint8Array([9, 8, 7, 0, 255]);
  client.send(payload);
  const echo = await nextMessage(client);
  assert(echo.isBinary === true, "echo must be binary");
  assert(Buffer.compare(Buffer.from(echo.data), Buffer.from(payload)) === 0, "binary echo must be byte-identical");

  client.close();

  child.stdin.end();
  const result = await exitPromise;
  assert(result.code === 0, `sidecar must exit 0 after dispose, got ${result.code}`);

  const activeLines = lines.filter((line) => line.t === "attach.active");
  assert(activeLines.length === 1, "exactly one attach.active line is expected");

  process.stdout.write(
    `SMOKE PASS: ready port ${ready.wsPort}, attached, attach.active(init-local), one binary echo, exit ${result.code}\n`
  );
} catch (error) {
  fail(error instanceof Error ? error.message : String(error));
  if (stderr.trim().length > 0) process.stderr.write(`sidecar stderr:\n${stderr}\n`);
  child.kill();
  process.exitCode = 1;
}
