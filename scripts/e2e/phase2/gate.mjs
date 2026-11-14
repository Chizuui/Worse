/** A failed gate step: `step` names the scenario, `command` the fix to run. */
export class GateError extends Error {
  constructor(step, message, command) {
    super(message);
    this.name = "GateError";
    this.step = step;
    this.command = command;
  }
}

export function fail(step, message, command) {
  throw new GateError(step, message, command);
}

export function delay(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Races `promise` against a step-tagged timeout. */
export async function withTimeout(promise, timeoutMs, step, label) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(() => {
          reject(new GateError(step, `timed out after ${timeoutMs}ms while waiting for ${label}`));
        }, timeoutMs);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

/** True while `pid` names a live process (EPERM still means it exists). */
export function isProcessAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error?.code === "EPERM";
  }
}
