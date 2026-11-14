// Small Windows helpers for the Phase 6 updater gate: sync tools, PowerShell JSON, registry, pids, waits.
import { spawnSync } from "node:child_process";
import net from "node:net";
import path from "node:path";
import process from "node:process";

export class StepError extends Error {
  constructor(step, message) {
    super(message);
    this.step = step;
  }
}

export const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

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

/** Runs a short tool synchronously without a console window. */
export function runSync(command, args, options = {}) {
  const result = spawnSync(command, args, { encoding: "utf8", windowsHide: true, ...options });
  return { code: result.status ?? 1, stdout: result.stdout ?? "", stderr: result.stderr ?? "" };
}

export const POWERSHELL = path.join(process.env.SystemRoot ?? "C:\\Windows", "System32\\WindowsPowerShell\\v1.0\\powershell.exe");

export function powershellJson(script, env = {}) {
  const result = runSync(POWERSHELL, ["-NoProfile", "-NonInteractive", "-Command", script], {
    env: { ...process.env, ...env },
  });
  if (result.code !== 0) throw new Error(`powershell failed: ${result.stderr.trim().slice(0, 400)}`);
  const text = result.stdout.trim();
  if (!text) return [];
  const value = JSON.parse(text);
  return Array.isArray(value) ? value : [value];
}

export function pidAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error.code === "EPERM";
  }
}

export function regExists(key) {
  return runSync("reg", ["query", key]).code === 0;
}

export function regValues(key) {
  const result = runSync("reg", ["query", key]);
  if (result.code !== 0) return null;
  const values = {};
  for (const line of result.stdout.split(/\r?\n/)) {
    const match = line.match(/^\s{4}(.+?)\s{4}(REG_\w+)\s{4}?(.*)$/);
    if (match) values[match[1]] = match[3];
  }
  return values;
}

export function regDelete(key) {
  if (regExists(key)) runSync("reg", ["delete", key, "/f"]);
}

export async function waitUntil(predicate, timeoutMs, step, message) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await predicate()) return;
    await sleep(500);
  }
  throw new StepError(step, message);
}
