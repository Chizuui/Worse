// zh-CN catalog check (spec phase-9-parity.md §6.7): every @tr("…") msgid in the Slint UI must have a non-empty
// msgstr in lang/zh_CN/LC_MESSAGES/harness-slint.po. Pure file reads; used by phase9-settings.mjs and runnable alone:
//   node Worse/scripts/e2e/phase9-settings/po-check.mjs        prints missing msgids (report only, always exit 0;
//   the user chose English-only UI strings on 2026-11-03, so missing zh-CN entries are counted, never fatal)
import { readdirSync, readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const app = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../apps/harness-slint");

function slintFiles(dir) {
  return readdirSync(dir, { withFileTypes: true }).flatMap((d) => {
    const full = path.join(dir, d.name);
    if (d.isDirectory()) return slintFiles(full);
    return d.name.endsWith(".slint") ? [full] : [];
  });
}

const unescape = (s) => s.replace(/\\(["\\n])/g, (_, c) => (c === "n" ? "\n" : c));

/** msgids used by @tr("…") (no context, no plural forms in this UI). */
export function usedMsgids() {
  const ids = new Map();
  const re = /@tr\(\s*"((?:[^"\\]|\\.)*)"/g;
  for (const file of slintFiles(path.join(app, "ui"))) {
    const text = readFileSync(file, "utf8");
    for (const m of text.matchAll(re)) {
      const id = unescape(m[1]);
      if (!ids.has(id)) ids.set(id, path.relative(app, file).replaceAll("\\", "/"));
    }
  }
  return ids;
}

/** msgid → msgstr of the zh-CN catalog (multi-line strings joined). */
export function catalog() {
  const text = readFileSync(path.join(app, "lang/zh_CN/LC_MESSAGES/harness-slint.po"), "utf8");
  const entries = new Map();
  let key = null;
  let current = null;
  let id = "";
  let str = "";
  const flush = () => {
    if (key !== null) entries.set(id, str);
    key = null;
    id = "";
    str = "";
  };
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim();
    if (line.startsWith("msgid ")) {
      flush();
      key = "id";
      current = "id";
      id = unescape(line.slice(7, -1));
    } else if (line.startsWith("msgstr ")) {
      current = "str";
      str = unescape(line.slice(8, -1));
    } else if (line.startsWith('"') && current) {
      const part = unescape(line.slice(1, -1));
      if (current === "id") id += part;
      else str += part;
    }
  }
  flush();
  return entries;
}

export function missingMsgids() {
  const po = catalog();
  return [...usedMsgids()].filter(([id]) => !po.get(id)).map(([id, file]) => ({ id, file }));
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const missing = missingMsgids();
  for (const m of missing) process.stdout.write(`${m.file}\t${JSON.stringify(m.id)}\n`);
  process.stdout.write(`${missing.length} missing of ${usedMsgids().size} (report only)\n`);
  // UI 只用英文（用户 2026-11-03）：缺少 zh-CN 译文不再算失败，只报告数量。
  process.exitCode = 0;
}
