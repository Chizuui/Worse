// Cross-process build lock for Worse/scripts/package/build-installer.mjs (builds share Worse/target).
// Spec: Worse/docs/specs/harness/phase-6-packaging.md section 1 ("Build lock").
//
// 修复原因（Phase 6 fix round）：旧实现用 mkdir(build.lock) 再写 build.lock/owner.json，两步之间锁目录
// 可被另一进程删除：进程 C 读到旧 owner（pid 已退出）判为 stale 后 rm -r 锁目录，此时进程 B 恰好刚 mkdir
// 成功、正在写 owner.json → ENOENT（phase6-update gate 曾因此重试最多 3 次）。同时 release 无条件 rm -r，
// 会删掉别人的锁；stale 判断只看 pid，Windows 复用 pid 时会把死锁当成活锁。
// 修复依据与机制：
//   1. 获取是原子的：owner JSON 先写进唯一临时文件，再 link() 成锁文件。link 在目标存在时以 EEXIST 失败，
//      所以锁文件一出现就带完整内容，不存在“已创建但未写入”的窗口。
//   2. 释放先核对 token：只删除内容仍是自己 token 的锁文件。
//   3. stale = pid 不存在，或 pid 存在但进程创建时间与锁里记录的不同（pid 复用）。
//   4. 清除 stale 锁只能在持有 break 锁（wx 独占创建）时进行，并在其中重新读取锁文件、与判定 stale 时读到的
//      原文逐字节相同才删除；因此不会误删在判定之后由别的进程新建的锁。
// 剩余风险：持有 break 锁的进程恰在毫秒级的清除窗口内崩溃，break 锁本身变成 stale，需按同样的 pid+创建时间
// 规则清除（未再加第三层锁），见 spec。
import { execFile } from "node:child_process";
import { readFileSync, unlinkSync, watch } from "node:fs";
import { link, mkdir, readFile, unlink, writeFile } from "node:fs/promises";
import { randomUUID } from "node:crypto";
import path from "node:path";
import process from "node:process";

const DEFAULT_WAIT_MS = 30 * 60 * 1000;
/** A crashed owner never touches the directory, so liveness is re-checked at least this often. */
const LIVENESS_RECHECK_MS = 30_000;
const POWERSHELL = path.join(process.env.SystemRoot ?? "C:\\Windows", "System32\\WindowsPowerShell\\v1.0\\powershell.exe");

export class BuildLockTimeoutError extends Error {
  constructor(lockFile, owner) {
    super(`build lock ${lockFile} still held by pid ${owner?.pid ?? "?"}`);
    this.owner = owner;
  }
}

function execText(command, args) {
  return new Promise((resolve) => {
    execFile(command, args, { windowsHide: true, encoding: "utf8", timeout: 60_000 }, (error, stdout) =>
      resolve(error ? null : stdout.trim()),
    );
  });
}

/**
 * The OS creation time of `pid` as an opaque string, or null when no such process exists. Throws when
 * the platform query itself fails (the caller then treats the owner as alive: fail safe, never steal).
 */
export async function processStartTime(pid) {
  if (!Number.isInteger(pid) || pid <= 0) return null;
  if (process.platform === "win32") {
    // Win32_Process 不需要提升权限即可读到其他进程的 CreationDate（Get-Process.StartTime 对部分进程拒绝访问）。
    const script =
      `$p = Get-CimInstance Win32_Process -Filter 'ProcessId=${pid}'; ` +
      "if ($p) { $p.CreationDate.ToUniversalTime().ToString('o') } else { 'NONE' }";
    const out = await execText(POWERSHELL, ["-NoProfile", "-NonInteractive", "-Command", script]);
    if (out === null || out === "") throw new Error(`could not query the creation time of pid ${pid}`);
    return out === "NONE" ? null : `win:${out}`;
  }
  if (process.platform === "linux") {
    const stat = await readFile(`/proc/${pid}/stat`, "utf8").catch(() => null);
    if (stat === null) return null;
    // 第 22 个字段 starttime（开机后的 clock ticks）；comm 可能含空格，从最后一个 ')' 之后切分。
    const fields = stat.slice(stat.lastIndexOf(")") + 2).split(" ");
    return `linux:${fields[19]}`;
  }
  const out = await execText("ps", ["-o", "lstart=", "-p", String(pid)]);
  return out ? `ps:${out}` : null;
}

function pidExists(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error.code === "EPERM";
  }
}

/** True when the recorded owner process is still running (same pid and same creation time). */
export async function ownerAlive(owner) {
  if (!owner || !Number.isInteger(owner.pid)) return false;
  if (!pidExists(owner.pid)) return false;
  if (!owner.startedAt) return true;
  try {
    const current = await processStartTime(owner.pid);
    return current === owner.startedAt;
  } catch {
    return true;
  }
}

/** Reads a lock file: { raw, owner } (owner null when unparsable), or null when it does not exist. */
async function readLock(file) {
  let raw;
  try {
    raw = await readFile(file, "utf8");
  } catch (error) {
    if (error.code === "ENOENT") return null;
    throw error;
  }
  try {
    return { raw, owner: JSON.parse(raw) };
  } catch {
    return { raw, owner: null };
  }
}

/** Creates `file` with `content` atomically and exclusively; false when it already exists. */
async function createExclusive(file, content) {
  const temp = `${file}.${randomUUID()}.tmp`;
  await writeFile(temp, content, { flag: "wx" });
  try {
    await link(temp, file);
    return true;
  } catch (error) {
    if (error.code === "EEXIST") return false;
    throw error;
  } finally {
    await unlink(temp).catch(() => {});
  }
}

/** Waits for any change in `dir`, or `ms`, whichever comes first (event-driven, no polling loop). */
function waitForChange(dir, ms) {
  return new Promise((resolve) => {
    let watcher;
    const done = () => {
      clearTimeout(timer);
      watcher?.close();
      resolve();
    };
    const timer = setTimeout(done, ms);
    try {
      watcher = watch(dir, done);
    } catch {
      // 目录监听不可用时只靠超时重新检查（上限仍由 waitMs 控制）。
    }
  });
}

/**
 * Removes the stale lock whose content was `staleRaw`, under the exclusive break lock. Returns true when
 * the caller should retry at once (lock removed, or someone else is already handling it).
 */
async function breakStaleLock(lockFile, staleRaw, identity, log) {
  const breakFile = `${lockFile}.break`;
  const mine = JSON.stringify(identity);
  if (!(await createExclusive(breakFile, mine))) {
    const holder = await readLock(breakFile);
    if (holder && !(await ownerAlive(holder.owner))) {
      // break 锁的持有者在清除窗口内退出：按同样的 pid+创建时间规则清除，内容未变才删。
      const again = await readLock(breakFile);
      if (again?.raw === holder.raw) await unlink(breakFile).catch(() => {});
      return true;
    }
    return false;
  }
  try {
    const current = await readLock(lockFile);
    if (current?.raw === staleRaw) {
      await unlink(lockFile);
      log(`build lock: removed stale lock of pid ${current.owner?.pid ?? "?"} (process gone or pid reused)`);
    }
    return true;
  } finally {
    const held = await readLock(breakFile);
    if (held?.raw === mine) await unlink(breakFile).catch(() => {});
  }
}

/**
 * Acquires the lock file, waiting up to `waitMs` for a live owner. Resolves with
 * { release(), releaseSync(), identity }; release only removes the file while it still carries our token.
 */
export async function acquireBuildLock({ lockFile, waitMs = DEFAULT_WAIT_MS, log = () => {} }) {
  const dir = path.dirname(lockFile);
  await mkdir(dir, { recursive: true });
  const identity = {
    pid: process.pid,
    // 查询失败时只记录 pid（其他进程按 pid 判活，退化为旧行为，但不会阻止构建）。
    startedAt: await processStartTime(process.pid).catch(() => null),
    token: randomUUID(),
    acquiredAt: new Date().toISOString(),
  };
  const content = JSON.stringify(identity);
  const deadline = Date.now() + waitMs;
  let announced = false;
  for (;;) {
    if (await createExclusive(lockFile, content)) break;
    const current = await readLock(lockFile);
    if (current === null) continue;
    if (!(await ownerAlive(current.owner))) {
      if (await breakStaleLock(lockFile, current.raw, identity, log)) continue;
    } else if (!announced) {
      log(`build lock held by pid ${current.owner?.pid ?? "?"}; waiting`);
      announced = true;
    }
    const left = deadline - Date.now();
    if (left <= 0) throw new BuildLockTimeoutError(lockFile, current.owner);
    await waitForChange(dir, Math.min(left, LIVENESS_RECHECK_MS));
  }

  let released = false;
  const isMine = (raw) => {
    try {
      return JSON.parse(raw).token === identity.token;
    } catch {
      return false;
    }
  };
  const releaseSync = () => {
    if (released) return;
    released = true;
    process.off("exit", releaseSync);
    let raw;
    try {
      raw = readFileSync(lockFile, "utf8");
    } catch {
      return;
    }
    if (isMine(raw)) unlinkSync(lockFile);
    else log("build lock: the lock file no longer carries our token; left in place");
  };
  // 进程异常退出（未走 finally）时同步释放；被强杀时由下一个构建按 pid+创建时间判 stale。
  process.on("exit", releaseSync);
  return {
    identity,
    releaseSync,
    release: async () => {
      if (released) return;
      released = true;
      process.off("exit", releaseSync);
      const current = await readLock(lockFile);
      if (current === null) return;
      // 只有本进程（token 相同）能删除；破锁方只删与其判定时原文相同的 stale 内容，不会改动活锁。
      if (isMine(current.raw)) await unlink(lockFile);
      else log("build lock: the lock file no longer carries our token; left in place");
    },
  };
}
