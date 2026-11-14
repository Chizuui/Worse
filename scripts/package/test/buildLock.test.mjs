// Engineering regression test for scripts/package/buildLock.mjs (build-installer.mjs cross-process lock).
//
//   node --test Worse/scripts/package/test/buildLock.test.mjs
//
// Covers the Phase 6 race (a stale-lock breaker deleting a lock another build had just created), release
// ownership, stale detection by pid + creation time (pid reuse), and many processes contending at once.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { after, before, test } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import { BuildLockTimeoutError, acquireBuildLock, ownerAlive, processStartTime } from "../buildLock.mjs";

const moduleUrl = pathToFileURL(path.join(path.dirname(fileURLToPath(import.meta.url)), "../buildLock.mjs")).href;
let root;

before(async () => {
  root = await mkdtemp(path.join(os.tmpdir(), "harness-build-lock-test-"));
});

after(async () => {
  await rm(root, { recursive: true, force: true });
});

function runNode(source, args = []) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, ["--input-type=module", "-e", source, ...args], { windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => (stdout += chunk));
    child.stderr.on("data", (chunk) => (stderr += chunk));
    child.once("error", reject);
    child.once("close", (code) => resolve({ code, stdout, stderr, pid: child.pid }));
  });
}

/** A pid that existed and has exited (its creation time is gone with it). */
async function deadPid() {
  const { pid } = await runNode("");
  return pid;
}

test("acquire creates a complete lock file and release removes only our own lock", async () => {
  const lockFile = path.join(root, "own", "build-lock.json");
  const lock = await acquireBuildLock({ lockFile, waitMs: 1_000 });
  const owner = JSON.parse(await readFile(lockFile, "utf8"));
  assert.equal(owner.pid, process.pid);
  assert.equal(owner.token, lock.identity.token);
  assert.equal(owner.startedAt, await processStartTime(process.pid));
  await lock.release();
  assert.equal(existsSync(lockFile), false);
  // 无残留的临时文件或 break 锁。
  assert.deepEqual(await readdir(path.dirname(lockFile)), []);

  const second = await acquireBuildLock({ lockFile, waitMs: 1_000 });
  // 模拟锁已被他人接管：release 不得删除不带自己 token 的锁文件。
  const foreign = JSON.stringify({ pid: process.pid, startedAt: second.identity.startedAt, token: "someone-else" });
  await writeFile(lockFile, foreign);
  await second.release();
  assert.equal(await readFile(lockFile, "utf8"), foreign);
  await rm(lockFile);
});

test("a live owner blocks until the timeout", async () => {
  const lockFile = path.join(root, "live", "build-lock.json");
  const held = await acquireBuildLock({ lockFile, waitMs: 1_000 });
  await assert.rejects(acquireBuildLock({ lockFile, waitMs: 1_500 }), BuildLockTimeoutError);
  assert.equal(JSON.parse(await readFile(lockFile, "utf8")).token, held.identity.token);
  await held.release();
});

test("a lock of an exited pid is stale and is taken over", async () => {
  const lockFile = path.join(root, "dead", "build-lock.json");
  const pid = await deadPid();
  assert.equal(await ownerAlive({ pid, startedAt: "win:2000-01-01T00:00:00.0000000Z" }), false);
  await acquireBuildLock({ lockFile, waitMs: 1 }).then((lock) => lock.release());
  await writeFile(lockFile, JSON.stringify({ pid, startedAt: null, token: "dead" }));
  const lines = [];
  const lock = await acquireBuildLock({ lockFile, waitMs: 5_000, log: (line) => lines.push(line) });
  assert.match(lines.join("\n"), /removed stale lock of pid/);
  await lock.release();
});

test("a reused pid (same pid, different creation time) is stale", async () => {
  const lockFile = path.join(root, "reused", "build-lock.json");
  await acquireBuildLock({ lockFile, waitMs: 1 }).then((lock) => lock.release());
  // 本进程的 pid 活着，但记录的创建时间不是本进程的：等价于 pid 被复用。
  const reused = { pid: process.pid, startedAt: "win:2000-01-01T00:00:00.0000000Z", token: "old" };
  assert.equal(await ownerAlive(reused), false);
  await writeFile(lockFile, JSON.stringify(reused));
  const lock = await acquireBuildLock({ lockFile, waitMs: 5_000 });
  assert.equal(JSON.parse(await readFile(lockFile, "utf8")).token, lock.identity.token);
  await lock.release();
});

test("many processes contending (starting from a stale lock) never overlap", async () => {
  const dir = path.join(root, "contend");
  const lockFile = path.join(dir, "build-lock.json");
  const inside = path.join(dir, "inside.marker");
  await acquireBuildLock({ lockFile, waitMs: 1 }).then((lock) => lock.release());
  await writeFile(lockFile, JSON.stringify({ pid: await deadPid(), startedAt: null, token: "stale" }));
  const worker = `
    import { writeFile, rm } from "node:fs/promises";
    const { acquireBuildLock } = await import(process.argv[1]);
    const [lockFile, inside] = process.argv.slice(2);
    for (let round = 0; round < 3; round += 1) {
      const lock = await acquireBuildLock({ lockFile, waitMs: 120000 });
      // 临界区内独占创建标记文件：若另一个进程也在临界区，wx 会以 EEXIST 失败。
      await writeFile(inside, String(process.pid), { flag: "wx" });
      await new Promise((resolve) => setTimeout(resolve, 30));
      await rm(inside);
      await lock.release();
    }
    process.stdout.write("WORKER_OK");
  `;
  const results = await Promise.all(Array.from({ length: 6 }, () => runNode(worker, [moduleUrl, lockFile, inside])));
  for (const result of results) {
    assert.equal(result.code, 0, result.stderr);
    assert.match(result.stdout, /WORKER_OK/);
  }
  assert.deepEqual(await readdir(dir), []);
});
