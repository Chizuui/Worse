// Harness-native methods (contract 6.1/6.2) called directly over the NDJSON connection, each
// against a Node oracle: fs.writeFileAtomic, fs.pathIdentity, search.text (+ cancellation).
import { randomBytes } from "node:crypto";
import { readFile, readdir, realpath, rename, rm, stat, writeFile } from "node:fs/promises";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { repoRoot } from "../phase2/environment.mjs";
import { firstDifference, median } from "./compare.mjs";

const BRIDGE_DIST = path.join(repoRoot, "Worse/sidecars/rust-bridge/dist/index.js");
const SNIFF_BYTES = 8 * 1024;
const MAX_LINE_TEXT = 500;

export class NativeCheckError extends Error {
  constructor(op, reason) {
    super(reason);
    this.op = op;
  }
}

function isProbablyBinary(buffer) {
  if (buffer.length === 0) return false;
  let suspicious = 0;
  for (const value of buffer) {
    if (value === 0) return true;
    const whitespace = value === 9 || value === 10 || value === 12 || value === 13;
    const control = (value >= 1 && value <= 8) || (value >= 14 && value <= 31) || value === 127;
    if (!whitespace && control) suspicious += 1;
  }
  return suspicious / buffer.length > 0.3;
}

function unpackFiles(packed) {
  const files = [];
  for (const line of packed.split("\n")) {
    const tab = line.indexOf("\t");
    if (tab === -1 || line.slice(0, tab) !== "file") continue;
    files.push(line.slice(tab + 1).replace(/\\(.)/g, (_m, c) => (c === "t" ? "\t" : c === "n" ? "\n" : c)));
  }
  return files;
}

/** Node oracle for `search.text`, literal or JS-regex, over the TS index's file list. */
async function grepOracle(rootPath, files, { query, isRegex, caseSensitive, maxResults, maxFileBytes }) {
  const pattern = isRegex
    ? new RegExp(query, caseSensitive ? "" : "i")
    : new RegExp(query.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"), caseSensitive ? "" : "i");
  const matches = [];
  const filesWithMatches = new Set();
  for (const relativePath of files) {
    const absolute = path.join(rootPath, ...relativePath.split("/"));
    const info = await stat(absolute).catch(() => null);
    if (!info || !info.isFile() || info.size > maxFileBytes) continue;
    const bytes = await readFile(absolute);
    if (isProbablyBinary(bytes.subarray(0, SNIFF_BYTES))) continue;
    const lines = bytes.toString("utf8").split("\n");
    if (lines.at(-1) === "") lines.pop();
    for (let index = 0; index < lines.length; index += 1) {
      const text = lines[index].replace(/\r$/, "");
      const found = pattern.exec(text);
      if (!found) continue;
      if (matches.length >= maxResults) return { matches, fileCount: filesWithMatches.size, truncated: true };
      matches.push({ relativePath, line: index + 1, column: found.index + 1, text: text.slice(0, MAX_LINE_TEXT) });
      filesWithMatches.add(relativePath);
    }
  }
  return { matches, fileCount: filesWithMatches.size, truncated: false };
}

async function timed(fn, reps) {
  const samples = [];
  let first;
  for (let rep = 0; rep < reps; rep += 1) {
    const startedAt = performance.now();
    const value = await fn(rep);
    samples.push(performance.now() - startedAt);
    if (rep === 0) first = value;
  }
  return { first, ms: median(samples) };
}

async function checkWriteAtomic(connection, fx, reps) {
  const op = "fs.writeFileAtomic";
  const target = path.join(fx.atomic, "target 中文.bin");
  const payload = randomBytes(256 * 1024);
  const rust = await timed(
    () => connection.call(op, { path: target, contentBase64: payload.toString("base64") }),
    reps,
  );
  if (rust.first !== null) throw new NativeCheckError(op, `expected null, got ${JSON.stringify(rust.first)}`);
  const written = await readFile(target);
  if (!written.equals(payload)) throw new NativeCheckError(op, "written bytes differ from the payload");
  const leftovers = (await readdir(fx.atomic)).filter((name) => name.endsWith(".tmp"));
  if (leftovers.length > 0) throw new NativeCheckError(op, `temp files left behind: ${leftovers.join(", ")}`);
  const missingParent = await connection
    .call(op, { path: path.join(fx.atomic, "no-such-dir", "x.txt"), contentBase64: "" })
    .then(() => "resolved", (error) => error?.rpcError?.data?.errno ?? String(error));
  if (missingParent !== "ENOENT") throw new NativeCheckError(op, `missing parent should be ENOENT, got ${missingParent}`);

  const nodeTarget = path.join(fx.atomic, "node.bin");
  const ts = await timed(async () => {
    const temp = path.join(fx.atomic, `.node.bin.${randomBytes(8).toString("hex")}.tmp`);
    await writeFile(temp, payload, { flag: "wx" });
    await rename(temp, nodeTarget).catch(async (error) => {
      await rm(temp, { force: true });
      throw error;
    });
  }, reps);
  return { op, tsMs: ts.ms, rustMs: rust.ms, cases: 2 };
}

async function checkPathIdentity(connection, fx, reps) {
  const op = "fs.pathIdentity";
  const cases = [
    { path: fx.ws },
    { path: path.join(fx.ws, "nested", "deep") },
    { path: fx.ws, workspaceIdentity: "  remote:abc  " },
    { path: fx.ws, workspaceIdentity: "   " },
  ];
  if (fx.links.dirLink) cases.push({ path: path.join(fx.ws, "link-to-src") });
  let rustMs = 0;
  let tsMs = 0;
  for (const params of cases) {
    const rust = await timed(() => connection.call(op, params), reps);
    const ts = await timed(async () => {
      const realPath = await realpath(params.path);
      return {
        realPath,
        identityKey: params.workspaceIdentity?.trim() || realPath,
        caseInsensitive: process.platform === "win32" || process.platform === "darwin",
      };
    }, reps);
    const diff = firstDifference(ts.first, rust.first);
    if (diff) throw new NativeCheckError(op, `${JSON.stringify(params)} ${diff}`);
    rustMs += rust.ms;
    tsMs += ts.ms;
  }
  return { op, tsMs, rustMs, cases: cases.length };
}

async function checkSearchText(connection, fx, packedIndexes, reps) {
  const op = "search.text";
  const base = { maxResults: 2000, maxFileBytes: 1024 * 1024 };
  const cases = [
    { rootPath: fx.ws, query: "needle", isRegex: false, caseSensitive: false },
    { rootPath: fx.ws, query: "NEEDLE", isRegex: false, caseSensitive: true },
    { rootPath: fx.ws, query: "中文", isRegex: false, caseSensitive: false },
    { rootPath: fx.ws, query: "export (const|function) \\w+", isRegex: true, caseSensitive: true },
    { rootPath: fx.ws, query: "needle", isRegex: false, caseSensitive: false, maxResults: 3 },
    { rootPath: fx.ws, query: "needle-5", isRegex: false, caseSensitive: false, maxFileBytes: 64 * 1024 * 1024 },
    { rootPath: fx.repo, query: "export", isRegex: false, caseSensitive: false },
    { rootPath: fx.ws, query: "zzzz-no-match", isRegex: false, caseSensitive: false },
    { rootPath: fx.bulk, query: "bulkneedle", isRegex: false, caseSensitive: false },
    { rootPath: fx.bulk, query: String.raw`value\d+ = 7\b`, isRegex: true, caseSensitive: true },
  ];
  let rustMs = 0;
  let tsMs = 0;
  for (const raw of cases) {
    const params = { ...base, ...raw };
    const files = unpackFiles(packedIndexes.get(params.rootPath));
    const rust = await timed(async () => {
      const streamed = [];
      const result = await connection.call(op, params, {
        timeoutMs: 60_000,
        onProgress: (value) => streamed.push(...(value?.matches ?? [])),
      });
      return { matches: streamed, result };
    }, reps);
    const ts = await timed(() => grepOracle(params.rootPath, files, params), reps);
    const expected = {
      matches: ts.first.matches,
      result: { matchCount: ts.first.matches.length, fileCount: ts.first.fileCount, truncated: ts.first.truncated, cancelled: false },
    };
    const diff = firstDifference(expected, rust.first);
    if (diff) throw new NativeCheckError(op, `${JSON.stringify(raw)} ${diff}`);
    rustMs += rust.ms;
    tsMs += ts.ms;
  }
  return { op, tsMs, rustMs, cases: cases.length };
}

/** A cancel issued right after the request must end it promptly (-32800 or a finished result). */
async function checkCancel(connection, bridge, fx) {
  const op = "search.text:cancel";
  const controller = new AbortController();
  const startedAt = performance.now();
  const pending = connection.call(
    "search.text",
    { rootPath: fx.ws, query: "lorem", isRegex: false, caseSensitive: false, maxResults: 1_000_000, maxFileBytes: 64 * 1024 * 1024 },
    { signal: controller.signal, timeoutMs: 30_000 },
  );
  setTimeout(() => controller.abort(), 5);
  const outcome = await pending.then(
    (result) => (result?.cancelled ? "cancelled-result" : "finished"),
    (error) => (error instanceof bridge.RustCancelledError ? "cancelled" : `error: ${String(error)}`),
  );
  if (outcome.startsWith("error")) throw new NativeCheckError(op, outcome);
  const ping = await connection.call("ping", {}, { timeoutMs: 5_000 }).then(() => "ok", (error) => String(error));
  if (ping !== "ok") throw new NativeCheckError(op, `host unhealthy after cancel: ${ping}`);
  return { op, outcome, ms: Math.round(performance.now() - startedAt) };
}

export async function runNativeChecks({ binaryPath, fx, packedIndexes, reps }) {
  const bridge = await import(pathToFileURL(BRIDGE_DIST).href);
  const host = await bridge.startRustHostProcess({
    binaryPath,
    cwd: fx.root,
    onStderr: () => undefined,
    onDiagnostic: (message) => process.stderr.write(`harness-host protocol: ${message}\n`),
  });
  try {
    const require = (method) => {
      if (!host.methods.has(method)) throw new NativeCheckError(method, "method not advertised by harness-host");
    };
    ["fs.writeFileAtomic", "fs.pathIdentity", "search.text"].forEach(require);
    const rows = [
      await checkWriteAtomic(host.connection, fx, reps),
      await checkPathIdentity(host.connection, fx, reps),
      await checkSearchText(host.connection, fx, packedIndexes, reps),
    ];
    const cancel = await checkCancel(host.connection, bridge, fx);
    return { rows, cancel, server: host.info };
  } finally {
    await host.dispose();
  }
}
