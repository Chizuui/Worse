// Phase 3c store gate fixture: seeds a large task-index database with node:sqlite while NO Host
// is running (the TS Host stays the only writer whenever a Host is up). The schema itself was
// created and migrated by the real TS Host beforehand.
import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";

export const TASK_COUNT = 3000;
export const PROJECT_COUNT = 40;

/** Deterministic PRNG so every run seeds the same database. */
function rng(seed) {
  let state = seed >>> 0;
  return () => {
    state = (state * 1664525 + 1013904223) >>> 0;
    return state / 2 ** 32;
  };
}

const UNICODE = ["naïve café", "日本語のタイトル", "emoji \u{1F680} rocket", "tab\tnew\nline", 'quote " and \\ backslash', "  sep"];

function metaFor(index, random) {
  // 只放 TS 能读的 meta：null / "" 会让 TS 的整个 readTree 抛错（见 breakTaskMeta 的错误一致性场景）。
  if (index % 97 === 0) return { text: "42", hasTrace: false };
  if (index % 89 === 0) return { text: " true ", hasTrace: false };
  if (index % 83 === 0) return { text: "{}", hasTrace: false };
  const meta = {};
  if (index % 7 === 0) meta["10"] = "integer-like key";
  meta.summary = `${UNICODE[index % UNICODE.length]} ${"x".repeat(600 + Math.floor(random() * 400))}`;
  const hasTrace = index % 3 !== 0;
  if (hasTrace) meta.traceId = `trace-${index}`;
  if (index % 5 === 0) meta.mode = index % 10 === 0 ? "plan" : null;
  if (index % 11 === 0) meta.title = "meta title is overwritten";
  if (index % 13 === 0) meta.nested = { a: [1, 2.5, -0.125, null, true], b: { c: "d" }, big: 9007199254740991 };
  if (index % 17 === 0) meta.float = 1e21;
  if (index % 19 === 0) meta.status = "running";
  if (index % 23 === 0) meta.workspaceIdentity = "from-meta";
  meta.model = `model-${index % 4}`;
  let text = JSON.stringify(meta);
  // 重复键与非规范数字只可能来自旧数据：JSON.parse 取最后一个值、保留第一次出现的位置。
  if (index % 29 === 0) text = `${text.slice(0, -1)},"model":"duplicate-last","exp":1.50E+2,"zero":-0.0}`;
  if (index % 31 === 0) text = `${text.slice(0, -1)},"__proto__":{"polluted":1}}`;
  return { text, hasTrace };
}

/**
 * Seeds projects, roots (existing / missing / file), tasks with edge cases and groups.
 * Returns the task ids whose meta has no traceId (TS generates a random one per read).
 */
export async function seedFixture(dbPath, rootsDir) {
  await mkdir(rootsDir, { recursive: true });
  const filePath = path.join(rootsDir, "not-a-directory.txt");
  await writeFile(filePath, "x");
  const db = new DatabaseSync(dbPath, { enableForeignKeyConstraints: false });
  const random = rng(7);
  const randomTrace = new Set();
  try {
    db.exec("BEGIN IMMEDIATE");
    db.prepare("INSERT OR IGNORE INTO hosts(host_id, kind, position) VALUES ('local','local',0)").run();
    db.prepare("INSERT OR IGNORE INTO hosts(host_id, kind, position) VALUES ('remote:server:fixture','server',1)").run();
    const insertProject = db.prepare(
      "INSERT INTO projects(project_id,host_id,name,name_overridden,position,pinned,pinned_at,deleted,deleted_at,created_at,updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?)"
    );
    const insertRoot = db.prepare("INSERT INTO project_roots(project_id,root_path,position,is_primary) VALUES (?,?,?,?)");
    const projectIds = [];
    for (let i = 0; i < PROJECT_COUNT; i += 1) {
      const id = `fixture-project-${String(i).padStart(3, "0")}`;
      projectIds.push(id);
      const host = i === 3 ? "remote:server:fixture" : "local";
      const deleted = i % 9 === 4 ? 1 : 0;
      const position = i % 6 === 0 ? 5 : i; // 相同 position 用 project_id 决定顺序
      insertProject.run(id, host, `Project ${UNICODE[i % UNICODE.length]} ${i}`, i % 4 === 0 ? 1 : 0, position, i % 5 === 0 ? 1 : 0, i % 5 === 0 ? 1000 + i : null, deleted, deleted ? 2000 : null, 1_700_000_000_000 + i, 1_700_000_100_000 + i);
      const existing = path.join(rootsDir, `p${i}`);
      await mkdir(existing, { recursive: true });
      if (i % 8 === 1) {
        insertRoot.run(id, path.join(rootsDir, `missing-${i}`), 0, 1);
      } else if (i % 8 === 2) {
        insertRoot.run(id, filePath, 0, 1);
      } else if (i % 8 === 3) {
        insertRoot.run(id, existing, 0, 0); // 无主要目录：不派生状态
      } else if (i % 8 === 5) {
        insertRoot.run(id, path.join(rootsDir, `secondary-${i}`), 0, 0);
        insertRoot.run(id, existing, 1, 1);
        insertRoot.run(id, path.join(filePath, "under-file"), 2, 0);
      } else if (i % 8 !== 7) {
        insertRoot.run(id, existing, 0, 1);
      }
    }
    const insertTask = db.prepare(
      `INSERT INTO tasks(workspace_key,workspace_path,workspace_identity,task_id,title,provider,created_at,updated_at,unread_at,pinned,archived,deleted,
        title_overridden,meta_json,host_id,root_path,project_id,cli_title,cli_title_source,cli_created_at,cli_updated_at,cli_last_outcome,cli_deleted)
       VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`
    );
    for (let i = 0; i < TASK_COUNT; i += 1) {
      const taskId = `fixture-task-${String(i).padStart(5, "0")}`;
      const workspace = path.join(rootsDir, `p${i % PROJECT_COUNT}`);
      let key = workspace;
      let identity = workspace;
      if (i % 41 === 0) {
        key = `remote:ssh:host.example:22:user:/home/u/w${i}`;
        identity = i % 82 === 0 ? key : null;
      } else if (i % 43 === 0) {
        identity = null;
      } else if (i % 47 === 0) {
        identity = "something-else";
      }
      const { text, hasTrace } = metaFor(i, random);
      const provider = i % 53 === 0 ? "other" : "glm";
      const deleted = i % 59 === 0 ? 1 : 0;
      const cliDeleted = i % 61 === 0 ? 1 : i % 67 === 0 ? 0 : null;
      const cliCreated = i % 71 === 0 ? null : 1_700_000_000_000 + i;
      const cliUpdated = 1_700_001_000_000 + (i % 500) * 1000 + (i % 37 === 0 ? 0.5 : 0);
      const outcomes = [null, "error", "completed", "interrupted", ""];
      const host = i % 73 === 0 ? "remote:server:fixture" : "local";
      const listed = provider === "glm" && !deleted && cliDeleted !== 1 && cliCreated !== null && host === "local";
      if (listed && !hasTrace) randomTrace.add(taskId);
      insertTask.run(
        key, workspace, identity, taskId, i % 9 === 0 ? `Overridden ${i}` : "", provider, 1_600_000_000_000 + i, 1_600_000_000_000 + i,
        i % 6 === 0 ? 1_700_002_000_000 + i : null, i % 15 === 0 ? 1 : 0, i % 16 === 0 ? 1 : 0, deleted,
        i % 9 === 0 ? 1 : 0, text, host, workspace, i % 4 === 0 ? null : projectIds[i % PROJECT_COUNT],
        i % 25 === 0 ? null : `CLI title ${i} ${UNICODE[i % UNICODE.length]}`, i % 18 === 0 ? "custom" : i % 2 ? "auto" : null,
        cliCreated, cliUpdated, outcomes[i % outcomes.length], cliDeleted
      );
    }
    const insertGroup = db.prepare("INSERT INTO task_groups(group_id,title,color,created_at,updated_at) VALUES (?,?,?,?,?)");
    const insertMember = db.prepare(
      "INSERT INTO task_group_members(group_id,workspace_key,workspace_path,workspace_identity,task_id,sort_order,added_at,created_at,updated_at) VALUES (?,?,?,?,?,?,?,?,?)"
    );
    const insertOrder = db.prepare("INSERT INTO task_group_view_node_orders(node_type,node_key,sort_order,created_at,updated_at) VALUES (?,?,?,?,?)");
    for (let g = 0; g < 12; g += 1) {
      const groupId = `fixture-group-${g}`;
      insertGroup.run(groupId, `Group ${UNICODE[g % UNICODE.length]}`, ["gray", "red", "blue"][g % 3], 1_700_000_000_000 + (g % 4), 1_700_000_000_500 + g);
      insertOrder.run("group", groupId, g % 5, 1, 2);
    }
    for (let m = 0; m < 400; m += 1) {
      const workspace = path.join(rootsDir, `p${m % PROJECT_COUNT}`);
      insertMember.run(`fixture-group-${m % 12}`, workspace, workspace, null, `fixture-task-${String(m * 7).padStart(5, "0")}`, m % 3 === 0 ? null : m % 50, 1_700_000_000_000 + m, 1, 2);
    }
    insertOrder.run("task", "z-task", -3, 1, 2);
    db.exec("COMMIT");
  } finally {
    db.close();
  }
  return randomTrace;
}

/** Adds a migration id this build does not know (TS ignores it; Rust must fall back). */
export function addFutureMigration(dbPath) {
  const db = new DatabaseSync(dbPath);
  try {
    db.prepare("INSERT INTO schema_migration(id, checksum, app_version, applied_at) VALUES (?,?,?,?)").run("0010_future_migration", "f".repeat(64), "99.0.0", Date.now());
  } finally {
    db.close();
  }
}

/** Makes one listed task's meta `null`: TS readTree throws (`null.traceId`), Rust must fall back. */
export function breakTaskMeta(dbPath) {
  const db = new DatabaseSync(dbPath);
  try {
    db.prepare("UPDATE tasks SET meta_json = 'null' WHERE task_id = 'fixture-task-00001'").run();
  } finally {
    db.close();
  }
}
