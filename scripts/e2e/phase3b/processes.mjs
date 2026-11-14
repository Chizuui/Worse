// Process-tree tracking for the no-orphan checks (B8/B9/O1).
//
// 修复原因（ORPHANS_TS 偶发失败，见 Worse/.work/phase3b/fix.md）：旧实现只按 {pid, ppid, name} 建树，
// 关闭后固定等 1.5s 再按 pid+name 判定存活，有两处竞态：
// 1. Windows 的 ParentProcessId 在父进程退出后不会清零，pid 又会被很快复用。机器上早就存在的
//    cmd.exe / OpenConsole.exe / conhost.exe，如果它们早已退出的父进程 pid 恰好被本次测试的
//    pwsh / OpenConsole 复用，就会被当成测试进程的"子孙"记进快照，之后自然"没退出"。
// 2. 快照里的进程退出后 pid 被同名进程（每次控制台程序启动都会新建 conhost.exe）复用，也会被误判存活；
//    固定 1.5s 也不等 node-pty 的异步收尾（kill 时另起 agent 进程枚举并结束控制台进程）。
// 现在每个进程以 {pid, name, 创建时间} 为身份：子进程必须晚于父进程创建（过滤陈旧 ppid 链），
// 存活判定要求三者都相同；关闭后用 Process.WaitForExit 做有上限的事件式等待，而不是固定 sleep。
import { execFile, execFileSync } from "node:child_process";

const WINDOWS_LIST =
  "Get-CimInstance Win32_Process | ForEach-Object { [pscustomobject]@{ pid = $_.ProcessId; ppid = $_.ParentProcessId; name = $_.Name; " +
  "created = if ($_.CreationDate) { [long](($_.CreationDate.ToUniversalTime() - [datetime]'1970-01-01Z').TotalMilliseconds * 1000) } else { 0 } } } | ConvertTo-Json -Compress";

/** All processes as {pid, ppid, name, created}; `created` is the creation time in µs since the epoch (0 = unknown). */
export function listProcesses() {
  if (process.platform === "win32") {
    const json = execFileSync("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", WINDOWS_LIST], {
      encoding: "utf8",
      windowsHide: true,
      maxBuffer: 64 * 1024 * 1024,
    });
    return JSON.parse(json).map((row) => ({ pid: row.pid, ppid: row.ppid, name: row.name, created: Number(row.created) }));
  }
  const text = execFileSync("ps", ["-eo", "pid=,ppid=,etimes=,comm="], { encoding: "utf8" });
  const nowUs = Date.now() * 1000;
  return text
    .split("\n")
    .map((line) => line.trim().split(/\s+/))
    .filter((parts) => parts.length >= 4)
    .map(([pid, ppid, etimes, ...name]) => ({ pid: Number(pid), ppid: Number(ppid), name: name.join(" "), created: nowUs - Number(etimes) * 1_000_000 }));
}

/** Same process: pid, image name and creation time all match (pid reuse cannot alias). */
function sameProcess(a, b) {
  if (a.pid !== b.pid || a.name !== b.name) return false;
  // Unix 只有秒级的 etimes，两次采样间允许 1s 误差；Windows 用 WMI 的微秒级创建时间精确比较。
  return process.platform === "win32" ? a.created === b.created : Math.abs(a.created - b.created) <= 1_000_000;
}

/**
 * Descendants of `rootPid` (excluding it). A child is only linked when it was created no earlier
 * than its parent: a stale ParentProcessId that now names a reused pid does not make an unrelated,
 * older process part of the tree.
 */
export function descendants(rootPid, processes = listProcesses(), skipped = null) {
  const root = processes.find((row) => row.pid === rootPid);
  if (root === undefined) return [];
  const children = new Map();
  for (const row of processes) {
    const list = children.get(row.ppid) ?? [];
    list.push(row);
    children.set(row.ppid, list);
  }
  const found = [];
  const queue = [root];
  while (queue.length > 0) {
    const parent = queue.shift();
    for (const child of children.get(parent.pid) ?? []) {
      if (child.pid === parent.pid) continue;
      if (child.created < parent.created) {
        // 陈旧的 ppid 链接：旧实现会把它（连同它的子树）算进测试进程树。
        skipped?.push({ ...child, linkedTo: `${parent.name}#${parent.pid}` });
        continue;
      }
      found.push(child);
      queue.push(child);
    }
  }
  return found;
}

/** Which of the given processes are still alive (same pid, image name and creation time). */
export function survivors(snapshot, processes = listProcesses()) {
  return snapshot.filter((row) => processes.some((live) => sameProcess(live, row)));
}

/** Windows: waits on each process handle (WaitForExit) until a shared deadline; skips reused pids. */
const WINDOWS_WAIT =
  "$rows = ConvertFrom-Json $env:P3B_WAIT_ROWS; $deadline = [datetime]::UtcNow.AddMilliseconds([int]$env:P3B_WAIT_MS); " +
  "$sw = [Diagnostics.Stopwatch]::StartNew(); $alive = @(); " +
  "foreach ($row in @($rows)) { try { $p = [Diagnostics.Process]::GetProcessById([int]$row.pid) } catch { continue }; " +
  "try { $created = [long](($p.StartTime.ToUniversalTime() - [datetime]'1970-01-01Z').TotalMilliseconds * 1000) } catch { continue }; " +
  "if ([Math]::Abs($created - [long]$row.created) -gt 2000) { continue }; " +
  "$alive += $row.name + '#' + $row.pid; $left = [int][Math]::Max(0, ($deadline - [datetime]::UtcNow).TotalMilliseconds); [void]$p.WaitForExit($left) }; " +
  "ConvertTo-Json -Compress @{ alive = @($alive); waitedMs = $sw.Elapsed.TotalMilliseconds }";

/**
 * Bounded wait for the given processes to exit, then the exact survivor list. Windows blocks on
 * the process handles (event-based, returns as soon as the last one exits); elsewhere it re-checks
 * the process table every 100 ms. Returns `{ left, waitedMs, aliveAtStart }`: `waitedMs` is the time
 * spent blocked on processes that were still running (Windows), `aliveAtStart` their names.
 */
export async function waitForExit(rows, timeoutMs) {
  const startedAt = performance.now();
  if (rows.length === 0) return { left: [], waitedMs: 0, aliveAtStart: [] };
  if (process.platform === "win32") {
    const report = await new Promise((resolve) => {
      execFile(
        "powershell.exe",
        ["-NoProfile", "-NonInteractive", "-Command", WINDOWS_WAIT],
        {
          windowsHide: true,
          timeout: timeoutMs + 15_000,
          env: { ...process.env, P3B_WAIT_ROWS: JSON.stringify(rows.map(({ pid, created }) => ({ pid, created: String(created) }))), P3B_WAIT_MS: String(timeoutMs) },
        },
        (_error, stdout) => {
          try {
            const parsed = JSON.parse(String(stdout).trim());
            resolve({ waitedMs: Number(parsed.waitedMs), aliveAtStart: [].concat(parsed.alive ?? []) });
          } catch {
            resolve({ waitedMs: performance.now() - startedAt, aliveAtStart: ["<unknown>"] });
          }
        },
      );
    });
    return { left: survivors(rows), ...report };
  }
  const deadline = startedAt + timeoutMs;
  let left = survivors(rows);
  while (left.length > 0 && performance.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 100));
    left = survivors(left);
  }
  return { left, waitedMs: performance.now() - startedAt, aliveAtStart: [] };
}

export function killPid(pid) {
  try {
    process.kill(pid, "SIGKILL");
  } catch {
    // already gone
  }
}

/** Remembers every process seen in `rootPid`'s tree across snapshots (keyed by pid + creation time). */
export function createTreeTracker(rootPid) {
  const live = new Map();
  const stale = new Map();
  return {
    snapshot() {
      const skipped = [];
      for (const row of descendants(rootPid, listProcesses(), skipped)) live.set(`${row.pid}:${row.created}`, row);
      for (const row of skipped) stale.set(`${row.pid}:${row.created}`, row);
    },
    /** Older processes whose stale ParentProcessId pointed into the tree (excluded; evidence only). */
    staleLinks: () => [...stale.values()].map((row) => `${row.name}#${row.pid}->${row.linkedTo}`),
    rows: () => [...live.values()],
    names: () => [...new Set([...live.values()].map((row) => row.name))].sort(),
  };
}
