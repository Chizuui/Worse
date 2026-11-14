// Persisted-data snapshots for the Phase 7 parity gate (spec phase-7-parity.md §5 data shape, §6 rollback,
// §7 kill switch). Read-only: SQLite files are opened with readOnly, JSON files are parsed, nothing is written.
import { createHash } from "node:crypto";
import { existsSync } from "node:fs";
import { readdir, readFile, stat } from "node:fs/promises";
import path from "node:path";
import { dataPaths } from "./fixtures.mjs";

let DatabaseSync;
async function sqlite() {
  if (!DatabaseSync) {
    // node:sqlite 仍是实验特性，屏蔽一次性的 ExperimentalWarning，避免污染 gate 输出。
    const original = process.emitWarning;
    process.emitWarning = (warning, ...rest) => {
      if (String(warning).includes("SQLite")) return;
      original.call(process, warning, ...rest);
    };
    ({ DatabaseSync } = await import("node:sqlite"));
  }
  return DatabaseSync;
}

async function readJson(file) {
  if (!existsSync(file)) return { exists: false };
  const text = await readFile(file, "utf8");
  try {
    return { exists: true, valid: true, value: JSON.parse(text) };
  } catch (error) {
    return { exists: true, valid: false, error: String(error.message) };
  }
}

/** Tables, columns and row counts; plus id rows of session-like tables for identity comparison. */
export async function readDb(file) {
  if (!existsSync(file)) return { exists: false };
  const Database = await sqlite();
  const db = new Database(file, { readOnly: true });
  try {
    const integrity = db.prepare("PRAGMA quick_check").get();
    const tables = {};
    const names = db
      .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
      .all()
      .map((row) => row.name);
    for (const name of names) {
      const quoted = `"${name.replaceAll('"', '""')}"`;
      const columns = db.prepare(`PRAGMA table_info(${quoted})`).all().map((column) => column.name);
      const count = db.prepare(`SELECT COUNT(*) AS n FROM ${quoted}`).get().n;
      const entry = { columns, count };
      if (/session|task|message/i.test(name) && columns.includes("id") && count <= 500) {
        entry.ids = db
          .prepare(`SELECT id FROM ${quoted} ORDER BY id`)
          .all()
          .map((row) => String(row.id));
      }
      tables[name] = entry;
    }
    return { exists: true, integrity: Object.values(integrity ?? {})[0] ?? null, tables };
  } finally {
    db.close();
  }
}

export async function snapshot(home) {
  const paths = dataPaths(home);
  return {
    settings: await readJson(paths.settings),
    providerConfig: await readJson(paths.providerConfig),
    credentials: await readJson(paths.credentials),
    tasksIndex: await readDb(paths.tasksIndex),
    cliDb: await readDb(paths.cliDb),
  };
}

/** Top-level shape of a snapshot: key sets and table/column sets, no values. */
export function shapeOf(snap) {
  const keys = (json) => (json.valid && json.value && typeof json.value === "object" ? Object.keys(json.value).sort() : null);
  const tables = (db) =>
    db.exists ? Object.fromEntries(Object.entries(db.tables).map(([name, table]) => [name, [...table.columns].sort()])) : null;
  return {
    settingsKeys: keys(snap.settings),
    providerConfigKeys: keys(snap.providerConfig),
    tasksIndexTables: tables(snap.tasksIndex),
    cliDbTables: tables(snap.cliDb),
  };
}

/** Lists differences between two shapes as strings (empty = same shape). */
export function diffShapes(left, right, leftName, rightName) {
  const out = [];
  const keyDiff = (label, a, b) => {
    const sa = new Set(a ?? []);
    const sb = new Set(b ?? []);
    const onlyA = [...sa].filter((key) => !sb.has(key));
    const onlyB = [...sb].filter((key) => !sa.has(key));
    if (onlyA.length) out.push(`${label}: only in ${leftName}: ${onlyA.join(", ")}`);
    if (onlyB.length) out.push(`${label}: only in ${rightName}: ${onlyB.join(", ")}`);
  };
  keyDiff("setting.json keys", left.settingsKeys, right.settingsKeys);
  keyDiff("provider_config.json keys", left.providerConfigKeys, right.providerConfigKeys);
  for (const db of ["tasksIndexTables", "cliDbTables"]) {
    const a = left[db] ?? {};
    const b = right[db] ?? {};
    keyDiff(`${db} tables`, Object.keys(a), Object.keys(b));
    for (const table of Object.keys(a).filter((name) => name in b)) keyDiff(`${db}.${table} columns`, a[table], b[table]);
  }
  return out;
}

/** Ids present in `before` but missing in `after`, per session-like table (rollback: rows only grow). */
export function lostRows(before, after) {
  const lost = [];
  for (const db of ["tasksIndex", "cliDb"]) {
    if (!before[db].exists) continue;
    if (!after[db].exists) {
      lost.push(`${db}: database disappeared`);
      continue;
    }
    for (const [name, table] of Object.entries(before[db].tables)) {
      const next = after[db].tables[name];
      if (!next) {
        lost.push(`${db}.${name}: table disappeared`);
        continue;
      }
      if (next.count < table.count) lost.push(`${db}.${name}: rows ${table.count} -> ${next.count}`);
      if (table.ids && next.ids) {
        const have = new Set(next.ids);
        const missing = table.ids.filter((id) => !have.has(id));
        if (missing.length) lost.push(`${db}.${name}: missing ids ${missing.slice(0, 5).join(", ")}`);
      }
    }
  }
  return lost;
}

/** sha256 of every file under `root` (relative path -> hash); used to prove the kill switch wrote nothing. */
export async function hashTree(root) {
  const result = {};
  async function walk(dir) {
    let entries;
    try {
      entries = await readdir(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) await walk(full);
      else if (entry.isFile()) {
        const info = await stat(full);
        const hash = createHash("sha256").update(await readFile(full)).digest("hex");
        result[path.relative(root, full).replaceAll("\\", "/")] = `${info.size}:${hash}`;
      }
    }
  }
  await walk(root);
  return result;
}

export function diffTrees(before, after) {
  const changes = [];
  for (const [file, hash] of Object.entries(before)) {
    if (!(file in after)) changes.push(`removed ${file}`);
    else if (after[file] !== hash) changes.push(`changed ${file}`);
  }
  for (const file of Object.keys(after)) if (!(file in before)) changes.push(`added ${file}`);
  return changes;
}
