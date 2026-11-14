// Drives the installed shell through the WebView2 DevTools endpoint: Tauri IPC (`updater_call`) and
// updater events, event-driven (listeners registered before each call).
import { StepError, sleep } from "./system.mjs";

export async function connectCdp(chromium, port, alive, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  let lastError;
  while (Date.now() < deadline) {
    if (!alive()) throw new StepError("APP_START", "the app exited before its DevTools endpoint opened");
    try {
      return await chromium.connectOverCDP(`http://127.0.0.1:${port}`, { timeout: 5_000 });
    } catch (error) {
      lastError = error;
      await sleep(500);
    }
  }
  throw new StepError("CDP_CONNECT", `could not attach to WebView2 on ${port}: ${String(lastError?.message ?? lastError)}`);
}

export async function findAppPage(browser, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    for (const context of browser.contexts()) {
      for (const page of context.pages()) {
        const url = page.url();
        if (url.startsWith("http://tauri.localhost") || url.startsWith("tauri://") || url.startsWith("https://tauri.localhost")) {
          const ready = await page.evaluate(() => typeof window.__TAURI_INTERNALS__?.invoke === "function").catch(() => false);
          if (ready) return page;
        }
      }
    }
    const context = browser.contexts()[0];
    if (context) await context.waitForEvent("page", { timeout: 1_000 }).catch(() => {});
    else await sleep(250);
  }
  throw new StepError("APP_PAGE", "no Harness page with Tauri IPC in the WebView");
}

export function updater(page, method, params = {}) {
  return page.evaluate(([m, p]) => window.__TAURI_INTERNALS__.invoke("updater_call", { method: m, params: p }), [method, params]);
}

/**
 * Subscribes to the updater events, runs `method`, and resolves with the first matching event
 * (event-driven; listener registered before the call so nothing is missed).
 */
export function callAndAwaitEvent(page, method, params, waits, timeoutMs) {
  return page.evaluate(
    async ({ method, params, waits, timeoutMs }) => {
      const internals = window.__TAURI_INTERNALS__;
      let settle;
      const got = new Promise((resolve) => {
        settle = resolve;
      });
      const registered = [];
      for (const wait of waits) {
        const handler = internals.transformCallback((message) => {
          const payload = message.payload;
          const kind = typeof payload === "string" ? "version" : payload?.kind;
          if (wait.kinds.includes(kind)) settle({ event: wait.event, payload });
        });
        const eventId = await internals.invoke("plugin:event|listen", { event: wait.event, target: { kind: "Any" }, handler });
        registered.push({ event: wait.event, eventId });
      }
      try {
        const call = internals.invoke("updater_call", { method, params }).then(
          () => new Promise(() => {}),
          (error) => ({ event: "call-error", payload: String(error?.message ?? JSON.stringify(error)) }),
        );
        const timeout = new Promise((resolve) => setTimeout(() => resolve({ event: "timeout" }), timeoutMs));
        return await Promise.race([got, call, timeout]);
      } finally {
        for (const entry of registered) {
          await internals.invoke("plugin:event|unlisten", { event: entry.event, eventId: entry.eventId }).catch(() => {});
        }
      }
    },
    { method, params, waits, timeoutMs },
  );
}

export async function diagnostics(page) {
  return updater(page, "getDiagnostics");
}

export async function closeGracefully(page, isAlive, waitExit) {
  if (!isAlive()) return "already exited";
  await page.evaluate(() => window.__TAURI_INTERNALS__.invoke("plugin:window|close", { label: "main" })).catch(() => {});
  const result = await Promise.race([waitExit().then(() => "closed"), sleep(30_000).then(() => "timeout")]);
  return result;
}
