// Identity of processes a gate saw (AGENT_PID lines). Windows reuses PIDs quickly, so "the PID is alive" is not
// proof that the process is ours: a gate records the executable and creation time when it first sees a PID, and at
// the end only reports or kills a live PID whose identity is unchanged.
import { spawn } from "node:child_process";
import { readFileSync, readlinkSync } from "node:fs";
import process from "node:process";

export function pidAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error?.code === "EPERM";
  }
}

/** `"<executable>|<creation time>"` of a live process, or `""` when it is gone or unreadable. */
export function identify(pid) {
  const n = Number(pid);
  if (process.platform !== "win32") {
    try {
      // Field 22 of /proc/<pid>/stat is the start time; the command name (field 2) may contain spaces.
      const stat = readFileSync(`/proc/${n}/stat`, "utf8");
      const start = stat.slice(stat.lastIndexOf(")") + 2).split(" ")[19] ?? "";
      return Promise.resolve(`${readlinkSync(`/proc/${n}/exe`)}|${start}`);
    } catch {
      return Promise.resolve("");
    }
  }
  return new Promise((resolve) => {
    const script = `$p = Get-CimInstance Win32_Process -Filter "ProcessId=${n}"; if ($p) { "$($p.ExecutablePath)|$($p.CreationDate.ToFileTimeUtc())" }`;
    const child = spawn("powershell", ["-NoProfile", "-NonInteractive", "-Command", script], {
      stdio: ["ignore", "pipe", "ignore"],
      windowsHide: true,
    });
    let out = "";
    child.stdout.on("data", (chunk) => {
      out += chunk.toString("utf8");
    });
    child.on("error", () => resolve(""));
    child.on("exit", () => resolve(out.trim()));
  });
}

/**
 * `seen` maps each PID to the identity promise taken when it was first seen. Returns the PIDs whose process is still
 * the same one (survivors) and the live PIDs that now belong to some other process (reused: never touched).
 */
export async function classifySeen(seen) {
  const survivors = [];
  const reused = [];
  for (const [pid, first] of seen) {
    if (!pidAlive(pid)) continue;
    const before = await first;
    const now = await identify(pid);
    (before !== "" && now === before ? survivors : reused).push(pid);
  }
  return { survivors, reused };
}
