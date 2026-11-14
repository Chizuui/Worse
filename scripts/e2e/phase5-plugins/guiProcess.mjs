// P6 process helpers for the Phase 5 plugins gate: debug harness-app + vite dev server lifecycle, WebView2
// DevTools attach, and a local dead-cloud stand-in. Only processes started here are ever stopped.
import { spawnSync } from "node:child_process";
import http from "node:http";
import net from "node:net";
import path from "node:path";
import { repoRoot } from "../phase2/environment.mjs";
import { GateError, fail } from "../phase2/gate.mjs";

export const worseRoot = path.join(repoRoot, "Worse");
export const appDir = path.join(worseRoot, "apps/harness-app");
export const appExe = path.join(worseRoot, "target/debug/harness-app.exe");
export const evidenceDir = path.join(worseRoot, ".work/phase5");
export const DEV_URL = "http://localhost:1430";
export const started = [];

export function freePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      server.close(() => resolve(port));
    });
  });
}

export function portInUse(port) {
  return new Promise((resolve) => {
    const socket = net.connect({ port, host: "127.0.0.1" });
    socket.once("connect", () => {
      socket.destroy();
      resolve(true);
    });
    socket.once("error", () => resolve(false));
  });
}

/** Resolves when `child` prints a line matching `pattern` (event-driven). */
export function waitForOutput(child, pattern, timeoutMs, step) {
  return new Promise((resolve, reject) => {
    let buffer = "";
    const cleanup = () => {
      clearTimeout(timer);
      child.stdout?.off("data", onData);
      child.stderr?.off("data", onData);
      child.off("exit", onExit);
    };
    const onData = (chunk) => {
      buffer += chunk.toString();
      if (pattern.test(buffer)) {
        cleanup();
        resolve();
      }
    };
    const onExit = (code) => {
      cleanup();
      reject(new GateError(step, `process exited (${String(code)}) before ${String(pattern)}; tail: ${buffer.slice(-600)}`));
    };
    const timer = setTimeout(() => {
      cleanup();
      reject(new GateError(step, `timed out waiting for ${String(pattern)}; tail: ${buffer.slice(-600)}`));
    }, timeoutMs);
    child.stdout?.on("data", onData);
    child.stderr?.on("data", onData);
    child.once("exit", onExit);
  });
}

/** WebView2 opens its DevTools endpoint only after the browser process starts; retry the connect. */
export async function connectCdp(chromium, port, exe, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  let lastError;
  while (Date.now() < deadline) {
    if (exe.exitCode !== null) fail("P6_APP_START", `harness-app exited early (${String(exe.exitCode)})`);
    try {
      return await chromium.connectOverCDP(`http://127.0.0.1:${port}`, { timeout: 5_000 });
    } catch (error) {
      lastError = error;
      await new Promise((resolve) => setTimeout(resolve, 500));
    }
  }
  fail("P6_CDP_CONNECT", `could not attach to WebView2 on port ${port}: ${String(lastError?.message ?? lastError)}`);
}

export async function findAppPage(browser, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    for (const context of browser.contexts()) {
      for (const page of context.pages()) {
        if (page.url().startsWith(DEV_URL)) return page;
      }
    }
    const context = browser.contexts()[0];
    if (context) await context.waitForEvent("page", { timeout: 1_000 }).catch(() => {});
    else await new Promise((resolve) => setTimeout(resolve, 250));
  }
  fail("P6_APP_PAGE", `no webview page at ${DEV_URL}`);
}

/** Local stand-in for the ZCode cloud: every request gets 404, so the app never reaches a real service. */
export function startDeadCloud() {
  const paths = [];
  const server = http.createServer((request, response) => {
    paths.push(`${request.method} ${request.url?.split("?")[0]}`);
    response.writeHead(404, { "content-type": "application/json" });
    response.end('{"code":404,"msg":"phase5 local fixture"}');
  });
  server.on("upgrade", (_request, socket) => socket.destroy());
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve({ server, paths, port: server.address().port })));
}

export async function closeAppGracefully(page, exe) {
  if (exe.exitCode !== null) return { closed: true, exitCode: exe.exitCode };
  const exited = new Promise((resolve) => exe.once("exit", (code) => resolve(code)));
  // 通过 Tauri 窗口关闭请求让 Rust shell 正常收尾（不调用 destroy()）。
  await page.evaluate(() => window.__TAURI_INTERNALS__.invoke("plugin:window|close", { label: "main" })).catch(() => {});
  const code = await Promise.race([exited, new Promise((resolve) => setTimeout(() => resolve("timeout"), 30_000))]);
  return { closed: code !== "timeout", exitCode: code };
}

export function stopStarted() {
  for (const child of started.reverse()) {
    if (child.exitCode !== null || child.pid === undefined) continue;
    // 只结束本脚本启动的进程树。
    spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], { stdio: "ignore", windowsHide: true });
  }
}
