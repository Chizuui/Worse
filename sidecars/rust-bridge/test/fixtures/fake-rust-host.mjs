// Minimal stand-in for `harness-host` speaking the Phase 3a NDJSON protocol.
// Behaviour is selected per method so one fixture covers success, errors,
// progress, cancellation, slow answers and crashes.
import process from "node:process";

const mode = process.env.FAKE_RUST_HOST_MODE ?? "normal";
const methods = ["file.stat", "file.readFileRange", "file.readdir", "slow.method", "crash.method", "git.refresh",
  "pty.spawn", "pty.write", "pty.resize", "pty.kill", "watch.subscribe", "watch.unsubscribe",
  "store.readTree", "store.readGroups"];
let nextSession = 1;
const killed = [];
const timers = new Map();
let buffer = "";

const send = (message) => process.stdout.write(`${JSON.stringify(message)}\n`);

function handle(message) {
  if (message.method === "$/cancel") {
    const timer = timers.get(message.params.id);
    if (timer) {
      clearTimeout(timer);
      timers.delete(message.params.id);
      send({ id: message.params.id, error: { code: -32800, message: "cancelled" } });
    }
    return;
  }
  const { id, method, params } = message;
  switch (method) {
    case "initialize":
      if (mode === "bad-version") return send({ id, result: { protocolVersion: 99, methods } });
      if (mode === "crash-on-init") process.exit(3);
      return send({ id, result: { protocolVersion: 1, server: "fake", serverVersion: "0.0.0", services: ["fs"], methods } });
    case "file.stat":
      if (params.path === "missing") {
        return send({ id, error: { code: 1, message: "not found", data: { errno: "ENOENT", syscall: "stat", path: params.path } } });
      }
      if (params.path === "internal") return send({ id, error: { code: -32603, message: "boom" } });
      if (params.path === "app") return send({ id, error: { code: 2, message: `Path is not a file: ${params.path}` } });
      return send({ id, result: { path: params.path, type: "file", size: 3, mtimeMs: 1.5 } });
    case "file.readFileRange":
      return send({ id, result: { base64: Buffer.from("hey").toString("base64") } });
    case "file.readdir":
      send({ method: "$/progress", params: { id, value: { n: 1 } } });
      return send({ id, result: [] });
    case "slow.method":
      timers.set(id, setTimeout(() => send({ id, result: "late" }), 5_000));
      return;
    // Phase 3b sessions: output right after the spawn result (exercises buffering before any listener).
    case "pty.spawn": {
      const sessionId = `p${nextSession++}`;
      send({ id, result: { sessionId, pid: 4242, shell: params.shell } });
      return send({ method: "$/pty/output", params: { sessionId, data: `hello ${params.cols}x${params.rows}` } });
    }
    case "pty.write":
      if (params.sessionId === "gone") return send({ id, error: { code: 2, message: `Terminal not found: ${params.sessionId}` } });
      send({ id, result: null });
      if (params.data === "crash") process.exit(5);
      if (params.data.startsWith("exit ")) return send({ method: "$/pty/exit", params: { sessionId: params.sessionId, exitCode: Number(params.data.slice(5)) } });
      return send({ method: "$/pty/output", params: { sessionId: params.sessionId, data: `echo:${params.data}` } });
    case "pty.resize":
      return send({ id, result: null });
    case "pty.kill":
      killed.push(params.sessionId);
      return send({ id, result: null });
    case "watch.subscribe": {
      if (params.path === "missing") return send({ id, error: { code: 2, message: `无法监视目录 '${params.path}': ENOENT` } });
      const watchId = `w${nextSession++}`;
      send({ id, result: { watchId } });
      return;
    }
    case "watch.unsubscribe":
      killed.push(params.watchId);
      return send({ id, result: null });
    case "test.fireWatch":
      send({ method: "$/watch/event", params: { watchId: params.watchId, event: params.event } });
      return send({ id, result: null });
    case "test.killed":
      return send({ id, result: killed });
    // Phase 3c store: ordered JSON text; integer-like keys exercise JSON.parse ordering.
    case "store.readGroups":
      return send({ id, result: { json: '{"groups":[{"id":"g","b":1,"a":null}],"members":[],"order":[]}' } });
    case "store.readTree":
      if (params.includeDeleted === "fallback") return send({ id, error: { code: -32603, message: "store fallback: schema" } });
      if (params.includeDeleted === "bad-text") return send({ id, result: { json: "{" } });
      return send({ id, result: { json: JSON.stringify({ projects: [], echo: params, tasks: [] }) } });
    case "crash.method":
      process.exit(4);
      return;
    default:
      return send({ id, error: { code: -32601, message: `method not found: ${method}` } });
  }
}

process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline = buffer.indexOf("\n");
  while (newline !== -1) {
    const line = buffer.slice(0, newline).trim();
    buffer = buffer.slice(newline + 1);
    if (line) handle(JSON.parse(line));
    newline = buffer.indexOf("\n");
  }
});
process.stdin.on("end", () => process.exit(0));
