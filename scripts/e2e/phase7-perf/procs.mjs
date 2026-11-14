// Process-tree measurement for the Phase 7 performance gate (Windows only).
//
// Only trees rooted at PIDs this gate spawned are inspected or stopped. A child belongs to the tree
// when its parent is in the tree and it was created after that parent (guards against PID reuse).
import { spawn, spawnSync } from "node:child_process";
import { existsSync, lstatSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

const SYSTEM32 = path.join(process.env.SystemRoot ?? "C:\\Windows", "System32");
const POWERSHELL = path.join(SYSTEM32, "WindowsPowerShell", "v1.0", "powershell.exe");

function run(command, args, { env = process.env, input } = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { env, windowsHide: true, stdio: ["pipe", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8").on("data", (chunk) => (stdout += chunk));
    child.stderr.setEncoding("utf8").on("data", (chunk) => (stderr += chunk));
    child.once("error", reject);
    child.once("close", (code) => resolve({ code: code ?? 1, stdout, stderr }));
    child.stdin.end(input ?? "");
  });
}

// 修复原因：本机 WMI 负载高时 Get-CimInstance 会返回 "Quota violation" / "Call cancelled" 等瞬时错误，
// 导致启动时拿不到根进程身份、taskkill /T（同样依赖 WMI）静默失败，最终残留整棵 ZCode 进程树。
// 现在对这类瞬时错误重试；存活检查与兜底树遍历改用不依赖 WMI 的 Get-Process（GetProcessTimes / NtQueryInformationProcess）。
const WMI_TRANSIENT = /Quota violation|Call cancelled|general error|RPC server|0x8004106C|0x80041006|0x80041032/i;
const WMI_RETRIES = 6;

/** Windows PowerShell 5.1 with its own module path (PSModulePath from pwsh 7 breaks autoload). */
export async function ps(script, input) {
  const env = { ...process.env };
  delete env.PSModulePath;
  for (let attempt = 1; ; attempt += 1) {
    const result = await run(POWERSHELL, ["-NoProfile", "-NonInteractive", "-Command", script], { env, input });
    if (result.code === 0) return result.stdout.trim();
    const message = result.stderr.trim().slice(0, 800);
    if (attempt >= WMI_RETRIES || !WMI_TRANSIENT.test(message)) throw new Error(`powershell failed: ${message}`);
    await new Promise((resolve) => setTimeout(resolve, 500 * attempt));
  }
}

// MSI 安装位于 Program Files；Store 安装只在 WindowsApps 下提供执行别名。
const PWSH7 =
  [
    path.join(process.env.ProgramFiles ?? "C:\\Program Files", "PowerShell", "7", "pwsh.exe"),
    // 隔离 home 会改写 LOCALAPPDATA/USERPROFILE，这里用系统账户的 profile 目录定位执行别名（只解析路径）。
    path.join(os.userInfo().homedir, "AppData", "Local", "Microsoft", "WindowsApps", "pwsh.exe"),
  ].find((candidate) => {
    // 执行别名是 APPEXECLINK 重解析点，existsSync(stat) 返回 false，只能用 lstat 判断。
    try {
      return existsSync(candidate) || lstatSync(candidate).size > 0;
    } catch {
      return false;
    }
  }) ?? null;

// 不经 WMI 的进程树遍历（pwsh 7 的 Process.Parent），仅在 CIM 重试仍失败时使用；角色未知记为 "?"。
const TREE_SCRIPT_NO_WMI = String.raw`
$roots = [Console]::In.ReadToEnd().Trim() -split ','
$all = @(Get-Process | ForEach-Object { $s = $null; try { $s = $_.StartTime } catch {}; $pp = $null; try { $pp = $_.Parent } catch {}; if ($s) { [pscustomobject]@{ ProcessId = $_.Id; ParentProcessId = $(if ($pp) { $pp.Id } else { 0 }); Name = $_.ProcessName + '.exe'; Start = $s; Path = $(try { $_.Path } catch { $null }) } } })
$byParent = @{}; foreach ($p in $all) { $k = [int]$p.ParentProcessId; if (-not $byParent.ContainsKey($k)) { $byParent[$k] = New-Object System.Collections.ArrayList }; [void]$byParent[$k].Add($p) }
$byId = @{}; foreach ($p in $all) { $byId[[int]$p.ProcessId] = $p }
$out = New-Object System.Collections.ArrayList
foreach ($r in $roots) {
  if (-not $r) { continue }
  $root = $byId[[int]$r]; if (-not $root) { continue }
  $queue = New-Object System.Collections.Queue; $queue.Enqueue($root); $seen = @{}
  while ($queue.Count -gt 0) {
    $cur = $queue.Dequeue(); if ($seen.ContainsKey([int]$cur.ProcessId)) { continue }; $seen[[int]$cur.ProcessId] = $true
    [void]$out.Add([pscustomobject]@{ root = [int]$r; pid = [int]$cur.ProcessId; parent = [int]$cur.ParentProcessId; name = $cur.Name; role = '?'; created = $cur.Start.ToUniversalTime().ToString('o'); createdMs = ([DateTimeOffset]$cur.Start).ToUnixTimeMilliseconds(); path = $cur.Path })
    $kids = $byParent[[int]$cur.ProcessId]; if ($kids) { foreach ($k in $kids) { if ($k.Start -ge $cur.Start) { $queue.Enqueue($k) } } }
  }
}
@($out) | ConvertTo-Json -Compress -Depth 3
`;

// The tree walk runs inside PowerShell so one call returns a consistent snapshot.
const TREE_SCRIPT = String.raw`
$roots = [Console]::In.ReadToEnd().Trim() -split ','
$all = @(Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId, Name, CreationDate, ExecutablePath, CommandLine)
# 只从命令行推导进程角色（--type / utility 子类型 / 入口脚本名），不输出命令行本身。
function Role($p) {
  $c = [string]$p.CommandLine
  if ($c -match '--type=([\w-]+)') { $r = $Matches[1]; if ($c -match '--utility-sub-type=([\w.]+)') { $r += ':' + ($Matches[1] -replace '^.*\.', '') }; if ($c -match '--service-name=([\w-]+)') { $r += ':' + $Matches[1] }; return $r }
  if ($c -match 'zcode\.cjs') { return 'agent' }
  if ($c -match 'host-node[\\/]dist[\\/]main\.js') { return 'host-node' }
  if ($c -match 'out[\\/]host[\\/]index\.js') { return 'host' }
  return 'main'
}
$byParent = @{}
foreach ($p in $all) { $k = [int]$p.ParentProcessId; if (-not $byParent.ContainsKey($k)) { $byParent[$k] = New-Object System.Collections.ArrayList }; [void]$byParent[$k].Add($p) }
$byId = @{}; foreach ($p in $all) { $byId[[int]$p.ProcessId] = $p }
$out = New-Object System.Collections.ArrayList
foreach ($r in $roots) {
  if (-not $r) { continue }
  $root = $byId[[int]$r]
  if (-not $root) { continue }
  $queue = New-Object System.Collections.Queue
  $queue.Enqueue($root)
  $seen = @{}
  while ($queue.Count -gt 0) {
    $cur = $queue.Dequeue()
    if ($seen.ContainsKey([int]$cur.ProcessId)) { continue }
    $seen[[int]$cur.ProcessId] = $true
    [void]$out.Add([pscustomobject]@{ root = [int]$r; pid = [int]$cur.ProcessId; parent = [int]$cur.ParentProcessId; name = $cur.Name; role = (Role $cur); created = $cur.CreationDate.ToUniversalTime().ToString('o'); createdMs = ([DateTimeOffset]$cur.CreationDate).ToUnixTimeMilliseconds(); path = $cur.ExecutablePath })
    $kids = $byParent[[int]$cur.ProcessId]
    if ($kids) { foreach ($k in $kids) { if ($k.CreationDate -ge $cur.CreationDate) { $queue.Enqueue($k) } } }
  }
}
`;

/** Test hook: the WMI-free tree walk alone (same output shape, role "?"). */
export async function processTreeNoWmi(rootPids) {
  if (!PWSH7) throw new Error("pwsh 7 not found");
  const result = await run(PWSH7, ["-NoProfile", "-NonInteractive", "-Command", TREE_SCRIPT_NO_WMI], { input: rootPids.join(",") });
  if (result.code !== 0) throw new Error(result.stderr.trim().slice(0, 400));
  const out = result.stdout.trim();
  return out ? [].concat(JSON.parse(out)) : [];
}

/** All processes in the trees rooted at `rootPids` (pid, parent, name, created, path). */
export async function processTree(rootPids) {
  const roots = rootPids.filter((pid) => Number.isInteger(pid) && pid > 0);
  if (roots.length === 0) return [];
  let out;
  try {
    out = await ps(`${TREE_SCRIPT}\n@($out) | ConvertTo-Json -Compress -Depth 3`, roots.join(","));
  } catch (error) {
    if (!PWSH7) throw error;
    const result = await run(PWSH7, ["-NoProfile", "-NonInteractive", "-Command", TREE_SCRIPT_NO_WMI], { input: roots.join(",") });
    if (result.code !== 0) throw new Error(`${error.message}; no-WMI fallback failed: ${result.stderr.trim().slice(0, 400)}`);
    out = result.stdout.trim();
  }
  const tree = out ? [].concat(JSON.parse(out)) : [];
  // 修复原因：WMI 承压时 Win32_Process 枚举可能“成功”却漏掉刚启动的进程，导致树为空、后续不杀任何进程。
  // 只要有根没出现在结果里，就用不依赖 WMI 的遍历补齐。
  const found = new Set(tree.map((entry) => entry.pid));
  if (PWSH7 && roots.some((pid) => !found.has(pid))) {
    const fallback = await processTreeNoWmi(roots).catch(() => []);
    if (fallback.filter((entry) => roots.includes(entry.pid)).length > roots.filter((pid) => found.has(pid)).length) return fallback;
  }
  return tree;
}

/**
 * Memory and CPU snapshot of the trees rooted at `rootPids`. Private working set comes from the
 * perf counters (what Task Manager shows as "Memory"); CPU is cumulative processor time in ms.
 */
export async function sampleTree(rootPids) {
  const roots = rootPids.filter((pid) => Number.isInteger(pid) && pid > 0);
  if (roots.length === 0) return { at: Date.now(), processes: [] };
  const script = `${TREE_SCRIPT}
$ids = @{}; foreach ($p in $out) { $ids[[int]$p.pid] = $true }
$perf = @{}
foreach ($c in @(Get-CimInstance Win32_PerfFormattedData_PerfProc_Process)) { if ($ids.ContainsKey([int]$c.IDProcess)) { $perf[[int]$c.IDProcess] = $c } }
$res = foreach ($p in $out) {
  $gp = Get-Process -Id $p.pid -ErrorAction SilentlyContinue
  $c = $perf[[int]$p.pid]
  [pscustomobject]@{
    root = $p.root; pid = $p.pid; parent = $p.parent; name = $p.name; role = $p.role; created = $p.created; path = $p.path
    privateWorkingSet = if ($c) { [int64]$c.WorkingSetPrivate } else { $null }
    workingSet = if ($gp) { [int64]$gp.WorkingSet64 } else { $null }
    privateBytes = if ($gp) { [int64]$gp.PrivateMemorySize64 } else { $null }
    cpuMs = if ($gp -and $gp.TotalProcessorTime) { [double]$gp.TotalProcessorTime.TotalMilliseconds } else { $null }
  }
}
@($res) | ConvertTo-Json -Compress -Depth 3`;
  const at = Date.now();
  const out = await ps(script, roots.join(","));
  const parsed = out ? JSON.parse(out) : [];
  return { at, processes: Array.isArray(parsed) ? parsed : [parsed] };
}

export function summarizeSample(sample) {
  const sum = (key) => sample.processes.reduce((total, entry) => total + (entry[key] ?? 0), 0);
  const byName = {};
  for (const entry of sample.processes) {
    const name = `${entry.name.toLowerCase()} (${entry.role ?? "?"})`;
    byName[name] ??= { count: 0, privateWorkingSet: 0, workingSet: 0 };
    byName[name].count += 1;
    byName[name].privateWorkingSet += entry.privateWorkingSet ?? 0;
    byName[name].workingSet += entry.workingSet ?? 0;
  }
  return {
    processCount: sample.processes.length,
    privateWorkingSetBytes: sum("privateWorkingSet"),
    workingSetBytes: sum("workingSet"),
    privateBytes: sum("privateBytes"),
    byName,
  };
}

/** CPU used by the tree between two samples (processes present in both, plus new ones from 0). */
export function cpuBetween(first, second) {
  const before = new Map(first.processes.map((entry) => [`${entry.pid}@${entry.created}`, entry.cpuMs ?? 0]));
  let cpuMs = 0;
  for (const entry of second.processes) {
    const key = `${entry.pid}@${entry.created}`;
    cpuMs += Math.max(0, (entry.cpuMs ?? 0) - (before.get(key) ?? 0));
  }
  const wallMs = second.at - first.at;
  const cores = os.cpus().length;
  return {
    cpuMs: Math.round(cpuMs),
    wallMs,
    percentOfOneCore: wallMs > 0 ? Math.round((cpuMs / wallMs) * 10_000) / 100 : null,
    percentOfMachine: wallMs > 0 ? Math.round((cpuMs / wallMs / cores) * 10_000) / 100 : null,
    logicalCpus: cores,
  };
}

/** Kills only the given trees (our own spawns). Returns the PIDs that were still alive. */
export async function killTrees(rootPids) {
  const tree = await processTree(rootPids).catch(() => []);
  // 先在树快照上逐个 TerminateProcess（process.kill，不依赖 WMI），再用 taskkill /T 兜底未进入快照的子进程。
  const terminate = (pid) => {
    try {
      process.kill(pid, "SIGKILL");
    } catch {
      // 已退出或无权限：随后的存活检查会如实暴露。
    }
  };
  for (const entry of await stillAlive(tree).catch(() => [])) terminate(entry.pid);
  for (const pid of rootPids) {
    spawnSync(path.join(SYSTEM32, "taskkill.exe"), ["/PID", String(pid), "/T", "/F"], { stdio: "ignore", windowsHide: true });
  }
  // 只处理已确认属于本次启动的进程，并按 pid + 创建时间复核，复用的 PID 不会被误杀。
  for (const entry of await stillAlive(tree).catch(() => [])) {
    terminate(entry.pid);
    spawnSync(path.join(SYSTEM32, "taskkill.exe"), ["/PID", String(entry.pid), "/F"], { stdio: "ignore", windowsHide: true });
  }
  return tree.map((entry) => entry.pid);
}

const createdKey = (entry) => `${entry.pid}@${entry.createdMs ?? Date.parse(entry.created)}`;

/** Processes from `snapshot` (pid + creation time) that are still alive. Uses Get-Process, not WMI. */
export async function stillAlive(snapshot) {
  if (snapshot.length === 0) return [];
  const out = await ps(
    "@(Get-Process | ForEach-Object { $s = $null; try { $s = $_.StartTime } catch {}; if ($s) { [pscustomobject]@{ pid = [int]$_.Id; createdMs = ([DateTimeOffset]$s).ToUnixTimeMilliseconds() } } }) | ConvertTo-Json -Compress",
  );
  const live = new Set((out ? [].concat(JSON.parse(out)) : []).map(createdKey));
  return snapshot.filter((entry) => live.has(createdKey(entry)));
}

export async function machineInfo() {
  const out = await ps(
    "$c = Get-CimInstance Win32_Processor | Select-Object -First 1 Name; $m = Get-CimInstance Win32_ComputerSystem; $o = Get-CimInstance Win32_OperatingSystem; " +
      "[pscustomobject]@{ cpu = $c.Name; totalRamBytes = [int64]$m.TotalPhysicalMemory; freeRamBytes = [int64]$o.FreePhysicalMemory * 1024; os = $o.Caption + ' ' + $o.Version; processCount = @(Get-Process).Count } | ConvertTo-Json -Compress",
  );
  return { ...JSON.parse(out), logicalCpus: os.cpus().length, node: process.version };
}
