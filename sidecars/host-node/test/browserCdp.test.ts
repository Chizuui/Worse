import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { test } from "node:test";
import { CdpConnection, chromiumLaunchArgs, resolveBrowserExecutable } from "../src/browserCdp.ts";
import { composeBrokerHooks, createBrowserOwner } from "../src/browserOwner.ts";

test("CdpConnection correlates replies, routes events with their session and fails pending calls on close", async () => {
  const written: string[] = [];
  const connection = new CdpConnection((frame) => written.push(frame));
  const events: Array<[string, string | undefined]> = [];
  connection.onEvent((method, _params, sessionId) => events.push([method, sessionId]));

  const ok = connection.send("Page.navigate", { url: "about:blank" }, "S1");
  const failed = connection.send("Runtime.evaluate", {});
  const sent = written.map((frame) => JSON.parse(frame) as { id: number; sessionId?: string });
  assert.equal(sent[0]?.sessionId, "S1");
  assert.equal(sent[1]?.sessionId, undefined);

  connection.dispatch(JSON.stringify({ id: sent[0]?.id, result: { frameId: "F" } }));
  connection.dispatch(JSON.stringify({ id: sent[1]?.id, error: { message: "boom", code: -32000 } }));
  connection.dispatch(JSON.stringify({ method: "Page.loadEventFired", params: {}, sessionId: "S1" }));
  connection.dispatch("not json");

  assert.deepEqual(await ok, { frameId: "F" });
  await assert.rejects(failed, /Runtime\.evaluate: boom/u);
  assert.deepEqual(events, [["Page.loadEventFired", "S1"]]);

  const pending = connection.send("Browser.getVersion");
  connection.close("test");
  await assert.rejects(pending, /closed: test/u);
  await assert.rejects(connection.send("Browser.getVersion"), /closed: test/u);
});

test("resolveBrowserExecutable honours an explicit executable and reads Windows env names case-insensitively", () => {
  const dir = mkdtempSync(path.join(os.tmpdir(), "harness-browser-exe-"));
  try {
    const exe = path.join(dir, "chrome.exe");
    writeFileSync(exe, "");
    assert.equal(resolveBrowserExecutable({ HARNESS_BROWSER_EXECUTABLE: exe }, "win32").path, exe);
    const programFiles = path.join(dir, "pf");
    const chrome = path.join(programFiles, "Google", "Chrome", "Application", "chrome.exe");
    rmSync(programFiles, { recursive: true, force: true });
    mkdirSync(path.dirname(chrome), { recursive: true });
    writeFileSync(chrome, "");
    // 复制出来的 env 对象保留 Windows 原始大小写（ProgramFiles），查找必须大小写无关。
    assert.equal(resolveBrowserExecutable({ ProgramFiles: programFiles }, "win32").path, chrome);
    assert.equal(resolveBrowserExecutable({ HARNESS_BROWSER_EXECUTABLE: "relative/chrome" }, "win32").path, null);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a disabled owner advertises no backend and answers backend_unavailable", async () => {
  const owner = createBrowserOwner({ env: { HARNESS_BROWSER: "off" } });
  await owner.ready;
  assert.equal(owner.backendIdentity(), null);
  const initLocal = { type: "init-local" };
  assert.equal(owner.brokerHooks.prepareHostMessage?.(initLocal), initLocal);
  const result = await owner.execute({ requestId: "r1", sessionId: "s", command: { method: "navigate", url: "about:blank" } });
  assert.equal(result.ok, false);
  assert.equal(result.error?.code, "backend_unavailable");
  await owner.dispose();
});

test("composeBrokerHooks chains message preparation and fans out ownership and dispatch", () => {
  const dispatched: string[] = [];
  const hooks = composeBrokerHooks(
    { ownsAttachment: (id) => id === "a", prepareHostMessage: (message) => ({ ...message, first: true }), onDispatched: (id) => dispatched.push(`1:${id}`) },
    { prepareHostMessage: (message) => ({ ...message, second: message.first === true }), onDispatched: (id) => dispatched.push(`2:${id}`) }
  );
  assert.equal(hooks.ownsAttachment?.("a"), true);
  assert.equal(hooks.ownsAttachment?.("b"), false);
  assert.deepEqual(hooks.prepareHostMessage?.({ type: "init-local" }), { type: "init-local", first: true, second: true });
  hooks.onDispatched?.("x", {});
  assert.deepEqual(dispatched, ["1:x", "2:x"]);
});

test("chromiumLaunchArgs disables the cold GPU process only for Windows headless (first-input latency fix)", () => {
  const has = (args: string[]) => args.includes("--disable-gpu");
  const base = { userDataDir: "P", env: {} };
  const winHeadless = chromiumLaunchArgs({ ...base, headless: true, platform: "win32" });
  assert.ok(has(winHeadless));
  assert.ok(winHeadless.includes("--headless=new"));
  assert.equal(winHeadless.at(-1), "about:blank");
  assert.ok(winHeadless.includes("--disable-renderer-backgrounding"));
  assert.ok(!has(chromiumLaunchArgs({ ...base, headless: false, platform: "win32" })));
  assert.ok(!has(chromiumLaunchArgs({ ...base, headless: true, platform: "linux" })));
  assert.ok(!has(chromiumLaunchArgs({ ...base, headless: true, platform: "darwin" })));
  assert.ok(!has(chromiumLaunchArgs({ userDataDir: "P", headless: true, platform: "win32", env: { HARNESS_BROWSER_GPU: "1" } })));
});