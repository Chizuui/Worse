import assert from "node:assert/strict";
import path from "node:path";
import { test } from "node:test";
import { createLineSplitter } from "../src/process/lineSplitter.ts";
import { decideRustError, formatNodeIoMessage } from "../src/routing/errorMapping.ts";
import { defaultRustHostPath, resolveConptyDir, rustHostEnv } from "../src/config.ts";
import { parseRustServiceFlags } from "../src/routing/flags.ts";
import { mapRustResult } from "../src/routing/resultMapping.ts";
import { findRoute, listRoutes } from "../src/routing/routeTable.ts";
import { isLocalScopeHostMessage } from "../src/routing/scope.ts";

test("flags parse lists, all/none and report unknown tokens", () => {
  assert.deepEqual([...parseRustServiceFlags(undefined).services], ["search", "git", "pty"], "default: search + git + pty");
  assert.deepEqual([...parseRustServiceFlags("all").services].sort(), ["fs", "git", "pty", "search", "store", "watch"]);
  assert.deepEqual([...parseRustServiceFlags("none").services], []);
  const parsed = parseRustServiceFlags(" FS, git ,bogus");
  assert.deepEqual([...parsed.services].sort(), ["fs", "git"]);
  assert.deepEqual(parsed.unknown, ["bogus"]);
});

test("only read methods are routable (single writer)", () => {
  for (const mutation of ["stagePaths", "unstagePaths", "discardPaths", "commit", "push", "switchBranch", "createBranchAndSwitch", "generateCommitMessage"]) {
    assert.equal(findRoute("git", mutation), undefined, mutation);
  }
  for (const write of ["writeWorkspaceFileSearchIgnore", "createScratchWorkspace", "createDefaultWorkspace", "ensureConversationWorkspace"]) {
    assert.equal(findRoute("file", write), undefined, write);
  }
  assert.equal(findRoute("file", "readdir")?.rustMethod, "file.readdir");
  assert.equal(findRoute("git", "refresh")?.service, "git");
  assert.equal(listRoutes().length, 24, "21 + 3 store reads");
});

test("scope: init-local and local attach are routable, remote and unknown are not", () => {
  assert.equal(isLocalScopeHostMessage({ type: "init-local" }), true);
  assert.equal(isLocalScopeHostMessage({ type: "attach-service-port", scope: { kind: "local" } }), true);
  assert.equal(isLocalScopeHostMessage({ type: "attach-service-port", scope: { kind: "remote", remoteSessionId: "r" } }), false);
  assert.equal(isLocalScopeHostMessage({ type: "attach-service-port", scope: { kind: "source-settings" } }), false);
  assert.equal(isLocalScopeHostMessage({ type: "attach-service-port" }), false);
  assert.equal(isLocalScopeHostMessage({ type: "something-else" }), false);
});

test("IoError is rebuilt as the exact Node message; protocol errors fall back", () => {
  assert.equal(formatNodeIoMessage("ENOENT", "stat", "C:\\x"), "ENOENT: no such file or directory, stat 'C:\\x'");
  const io = decideRustError({ code: 1, message: "x", data: { errno: "ENOTDIR", syscall: "scandir", path: "/a" } });
  assert.equal(io.kind, "surface");
  assert.ok(io.kind === "surface");
  assert.equal(io.payload.message, "ENOTDIR: not a directory, scandir '/a'");
  assert.equal(io.payload.code, "ENOTDIR");
  assert.equal(io.payload.name, "Error");

  const app = decideRustError({ code: 2, message: "Path is not a file: /a" });
  assert.ok(app.kind === "surface");
  assert.equal(app.payload.message, "Path is not a file: /a");
  assert.equal(app.payload.code, undefined);

  for (const code of [-32700, -32600, -32601, -32602, -32603, -32002, 99]) {
    assert.equal(decideRustError({ code, message: "m" }).kind, "fallback", String(code));
  }
  assert.equal(decideRustError({ code: 1, message: "io without data" }).kind, "fallback");
});

test("base64Bytes results become a top-level Uint8Array", () => {
  const mapped = mapRustResult("base64Bytes", { base64: Buffer.from("abc").toString("base64") });
  assert.ok(mapped.ok);
  assert.deepEqual(mapped.value, new Uint8Array(Buffer.from("abc")));
  assert.equal(mapRustResult("base64Bytes", "nope").ok, false);
  assert.deepEqual(mapRustResult("json", null), { ok: true, value: null });
});

test("line splitter handles split chunks, multibyte boundaries and overflow", () => {
  const lines: string[] = [];
  let overflowed = 0;
  const splitter = createLineSplitter(16, (line) => lines.push(line), () => (overflowed += 1));
  const bytes = Buffer.from("ab\n中文\n", "utf8");
  for (let index = 0; index < bytes.length; index += 1) splitter.push(bytes.subarray(index, index + 1));
  assert.deepEqual(lines, ["ab", "中文"]);
  splitter.push(Buffer.from("x".repeat(17)));
  assert.equal(overflowed, 1);
});

test("default binary prefers target/release over target/debug", () => {
  const posix = (value: string): string => value.split(path.sep).join("/");
  assert.ok(posix(defaultRustHostPath("win32", () => true)).endsWith("/target/release/harness-host.exe"));
  assert.ok(posix(defaultRustHostPath("linux", () => false)).endsWith("/target/debug/harness-host"));
});

test("conpty sideload: node-pty's bundled conpty.dll + OpenConsole.exe, Windows only, opt-out", () => {
  const nodePty = path.join("C:", "repo", "node_modules", "node-pty");
  const conpty = path.join(nodePty, "prebuilds", "win32-x64", "conpty");
  const binary = path.join("C:", "bin", "harness-host.exe");
  const files = new Set([path.join(conpty, "conpty.dll"), path.join(conpty, "OpenConsole.exe")]);
  const probe = { platform: "win32" as const, arch: "x64", exists: (p: string) => files.has(p), resolveNodePty: () => path.join(nodePty, "package.json") };
  assert.equal(resolveConptyDir({}, binary, probe), conpty);
  assert.equal(resolveConptyDir({}, binary, { ...probe, platform: "linux" }), null);
  assert.equal(resolveConptyDir({ HARNESS_CONPTY_DIR: "off" }, binary, probe), null);
  assert.equal(resolveConptyDir({ HARNESS_CONPTY_DIR: conpty }, binary, { ...probe, resolveNodePty: () => { throw new Error("unused"); } }), path.resolve(conpty));
  // 二进制旁已有 conpty.dll 时 portable-pty 会直接用它，不再改 PATH。
  const beside = new Set([...files, path.join(path.dirname(binary), "conpty.dll")]);
  assert.equal(resolveConptyDir({}, binary, { ...probe, exists: (p: string) => beside.has(p) }), null);
  // 缺 OpenConsole.exe 或没有 node-pty 时保持系统 ConPTY，而不是用残缺目录。
  assert.equal(resolveConptyDir({}, binary, { ...probe, exists: (p: string) => p.endsWith("conpty.dll") && files.has(p) }), null);
  assert.equal(resolveConptyDir({}, binary, { ...probe, resolveNodePty: () => { throw new Error("MODULE_NOT_FOUND"); } }), null);
});

test("harness-host env prepends the conpty dir to PATH, keeping the key's case", () => {
  assert.deepEqual(rustHostEnv({ Path: "sys", X: "1" }, "conpty"), { Path: "conpty;sys", X: "1" });
  assert.deepEqual(rustHostEnv({ X: "1" }, "conpty"), { X: "1", PATH: "conpty" });
  const env = { PATH: "a" };
  assert.equal(rustHostEnv(env, null), env);
});
