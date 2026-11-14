//! Write plans: every change is computed in memory first, shown as a masked unified diff, then applied atomically
//! after a conflict check (spec §9.1.3 "Write protocol").

use std::fmt;
use std::path::{Path, PathBuf};

use harness_settings::fsx;
use harness_settings::json::{detect_indent, is_secret_key, mask_secret, masked, OValue};
use serde::Serialize;

use crate::error::{ConfigError, Result};

/// Keys whose map values are always masked in previews.
pub(crate) const SECRET_MAPS: &[&str] = &["env", "headers", "http_headers", "env_http_headers"];

/// What a change does to its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FileOp {
    Write,
    Delete,
    /// Removes a whole directory (skills). Guarded by the content of one file in it.
    RemoveDir,
}

/// How to mask a file for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Json,
    Toml,
    Text,
}

/// One file change. `before_preview`/`after_preview`/`diff` have secrets masked; the raw contents stay private
/// (and out of `Debug`).
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    pub path: PathBuf,
    pub op: FileOp,
    /// Unified diff (3 lines of context) of the masked previews; empty for a no-op.
    pub diff: String,
    pub before_preview: Option<String>,
    pub after_preview: Option<String>,
    /// Nothing to write (same content).
    pub no_op: bool,
    #[serde(skip)]
    expected: Option<String>,
    #[serde(skip)]
    content: Option<String>,
    #[serde(skip)]
    guard: Option<PathBuf>,
}

impl fmt::Debug for FileChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileChange")
            .field("path", &self.path)
            .field("op", &self.op)
            .field("no_op", &self.no_op)
            .finish_non_exhaustive()
    }
}

impl FileChange {
    /// Writes `after` to `path` (which held `before`).
    pub(crate) fn write(path: PathBuf, before: Option<String>, after: String, kind: Kind) -> Self {
        let no_op = before.as_deref() == Some(after.as_str());
        let before_preview = before.as_deref().map(|t| preview(t, kind));
        let after_preview = preview(&after, kind);
        let diff = if no_op {
            String::new()
        } else {
            unified_diff(
                &path.display().to_string(),
                before_preview.as_deref().unwrap_or(""),
                &after_preview,
            )
        };
        Self {
            path,
            op: FileOp::Write,
            diff,
            before_preview,
            after_preview: Some(after_preview),
            no_op,
            expected: before,
            content: Some(after),
            guard: None,
        }
    }

    /// Deletes `path` (which held `before`; `None` = already absent → no-op).
    pub(crate) fn delete(path: PathBuf, before: Option<String>, kind: Kind) -> Self {
        let before_preview = before.as_deref().map(|t| preview(t, kind));
        let diff = before_preview
            .as_deref()
            .map(|b| unified_diff(&path.display().to_string(), b, ""))
            .unwrap_or_default();
        Self {
            path,
            op: FileOp::Delete,
            diff,
            no_op: before.is_none(),
            before_preview,
            after_preview: None,
            expected: before,
            content: None,
            guard: None,
        }
    }

    /// Removes directory `dir`; `guard` is a file inside it whose content must still be `guard_before`.
    pub(crate) fn remove_dir(dir: PathBuf, guard: PathBuf, guard_before: Option<String>) -> Self {
        let before_preview = guard_before.clone();
        let diff = before_preview
            .as_deref()
            .map(|b| unified_diff(&guard.display().to_string(), b, ""))
            .unwrap_or_default();
        Self {
            path: dir,
            op: FileOp::RemoveDir,
            diff,
            before_preview,
            after_preview: None,
            no_op: false,
            expected: guard_before,
            content: None,
            guard: Some(guard),
        }
    }

    fn guard_path(&self) -> &Path {
        self.guard.as_deref().unwrap_or(&self.path)
    }
}

/// A set of file changes shown together and applied together.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WritePlan {
    pub changes: Vec<FileChange>,
}

impl WritePlan {
    pub(crate) fn single(change: FileChange) -> Self {
        Self {
            changes: vec![change],
        }
    }

    /// True when applying would write nothing.
    #[must_use]
    pub fn is_no_op(&self) -> bool {
        self.changes.iter().all(|c| c.no_op)
    }

    /// All diffs joined (for a single preview pane).
    #[must_use]
    pub fn diff(&self) -> String {
        self.changes
            .iter()
            .map(|c| c.diff.as_str())
            .filter(|d| !d.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Result of [`apply`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyOutcome {
    /// Paths actually written or removed (no-op changes are not listed).
    pub written: Vec<PathBuf>,
}

/// Applies `plan`: first checks that every file still has the content the plan was computed from
/// ([`ConfigError::Conflict`] otherwise, nothing written), then writes each change atomically.
///
/// # Errors
///
/// Conflict or I/O failure.
pub async fn apply(plan: &WritePlan) -> Result<ApplyOutcome> {
    for change in plan.changes.iter().filter(|c| !c.no_op) {
        let current = fsx::read_to_string_opt(change.guard_path()).await?;
        if current != change.expected {
            return Err(ConfigError::Conflict {
                path: change.guard_path().to_path_buf(),
            });
        }
    }
    let mut outcome = ApplyOutcome::default();
    for change in plan.changes.iter().filter(|c| !c.no_op) {
        match change.op {
            FileOp::Write => {
                let content = change.content.as_deref().unwrap_or_default();
                fsx::write_atomic(&change.path, content.as_bytes()).await?;
            }
            FileOp::Delete => fsx::remove_file_opt(&change.path).await?,
            FileOp::RemoveDir => match tokio::fs::remove_dir_all(&change.path).await {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            },
        }
        outcome.written.push(change.path.clone());
    }
    Ok(outcome)
}

/// Masked display text of a file.
pub(crate) fn preview(text: &str, kind: Kind) -> String {
    match kind {
        Kind::Text => text.to_owned(),
        Kind::Json => match OValue::parse(text) {
            Ok(value) => masked(&value, SECRET_MAPS).to_pretty(&detect_indent(text), true),
            Err(_) => "<unparseable JSON hidden>\n".into(),
        },
        Kind::Toml => match text.parse::<toml_edit::DocumentMut>() {
            Ok(mut doc) => {
                mask_toml_table(doc.as_table_mut(), false);
                doc.to_string()
            }
            Err(_) => "<unparseable TOML hidden>\n".into(),
        },
    }
}

fn mask_toml_table(table: &mut toml_edit::Table, secret_map: bool) {
    for (key, item) in table.iter_mut() {
        let child_secret = secret_map || SECRET_MAPS.contains(&key.get());
        let secret_key = is_secret_key(key.get());
        mask_toml_item(item, child_secret, secret_key);
    }
}

fn mask_toml_item(item: &mut toml_edit::Item, secret_map: bool, secret_key: bool) {
    match item {
        toml_edit::Item::Table(t) => mask_toml_table(t, secret_map),
        toml_edit::Item::ArrayOfTables(a) => {
            for t in a.iter_mut() {
                mask_toml_table(t, secret_map);
            }
        }
        toml_edit::Item::Value(v) => mask_toml_value(v, secret_map, secret_key),
        toml_edit::Item::None => {}
    }
}

fn mask_toml_value(value: &mut toml_edit::Value, secret_map: bool, secret_key: bool) {
    match value {
        toml_edit::Value::String(s) if secret_map || secret_key => {
            let decor = s.decor().clone();
            let mut replaced = toml_edit::Formatted::new(mask_secret(s.value()));
            *replaced.decor_mut() = decor;
            *s = replaced;
        }
        toml_edit::Value::InlineTable(t) => {
            for (key, v) in t.iter_mut() {
                let child_secret = secret_map || SECRET_MAPS.contains(&key.get());
                mask_toml_value(v, child_secret, is_secret_key(key.get()));
            }
        }
        toml_edit::Value::Array(a) => {
            for v in a.iter_mut() {
                mask_toml_value(v, secret_map, false);
            }
        }
        _ => {}
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tag {
    Equal,
    Delete,
    Insert,
}

#[derive(Clone, Copy)]
struct Op {
    tag: Tag,
    a: usize,
    b: usize,
}

/// Largest middle section (lines × lines) diffed with LCS; larger edits are shown as replace.
const LCS_LIMIT: usize = 4_000_000;

/// Unified diff of two texts with 3 lines of context.
#[must_use]
pub fn unified_diff(label: &str, before: &str, after: &str) -> String {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    let ops = diff_ops(&a, &b);
    if ops.iter().all(|o| o.tag == Tag::Equal) {
        return String::new();
    }
    let mut out = format!("--- a/{label}\n+++ b/{label}\n");
    let ctx = 3;
    let mut i = 0;
    let mut prev_end = 0;
    while i < ops.len() {
        if ops[i].tag == Tag::Equal {
            i += 1;
            continue;
        }
        let start = i.saturating_sub(ctx).max(prev_end);
        let mut last_change = i;
        let mut j = i;
        while j < ops.len() {
            if ops[j].tag != Tag::Equal {
                last_change = j;
            } else if j - last_change > 2 * ctx {
                break;
            }
            j += 1;
        }
        let end = (last_change + ctx + 1).min(ops.len());
        let hunk = &ops[start..end];
        let old_len = hunk.iter().filter(|o| o.tag != Tag::Insert).count();
        let new_len = hunk.iter().filter(|o| o.tag != Tag::Delete).count();
        let old_start = hunk[0].a + usize::from(old_len > 0);
        let new_start = hunk[0].b + usize::from(new_len > 0);
        out.push_str(&format!(
            "@@ -{old_start},{old_len} +{new_start},{new_len} @@\n"
        ));
        for op in hunk {
            let (sign, line) = match op.tag {
                Tag::Equal => (' ', a[op.a]),
                Tag::Delete => ('-', a[op.a]),
                Tag::Insert => ('+', b[op.b]),
            };
            out.push(sign);
            out.push_str(line);
            out.push('\n');
        }
        prev_end = end;
        i = end;
    }
    out
}

fn diff_ops(a: &[&str], b: &[&str]) -> Vec<Op> {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let max_suffix = a.len().min(b.len()) - prefix;
    let suffix = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take(max_suffix)
        .take_while(|(x, y)| x == y)
        .count();
    let (am, bm) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let mut ops: Vec<Op> = (0..prefix)
        .map(|i| Op {
            tag: Tag::Equal,
            a: i,
            b: i,
        })
        .collect();
    let (mut ai, mut bi) = (prefix, prefix);
    if am.len().saturating_mul(bm.len()) <= LCS_LIMIT {
        // LCS table over the middle section.
        let w = bm.len() + 1;
        let mut t = vec![0u32; (am.len() + 1) * w];
        for i in (0..am.len()).rev() {
            for j in (0..bm.len()).rev() {
                t[i * w + j] = if am[i] == bm[j] {
                    t[(i + 1) * w + j + 1] + 1
                } else {
                    t[(i + 1) * w + j].max(t[i * w + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < am.len() || j < bm.len() {
            if i < am.len() && j < bm.len() && am[i] == bm[j] {
                ops.push(Op {
                    tag: Tag::Equal,
                    a: ai,
                    b: bi,
                });
                (i, j, ai, bi) = (i + 1, j + 1, ai + 1, bi + 1);
            } else if j < bm.len() && (i == am.len() || t[i * w + j + 1] > t[(i + 1) * w + j]) {
                ops.push(Op {
                    tag: Tag::Insert,
                    a: ai,
                    b: bi,
                });
                (j, bi) = (j + 1, bi + 1);
            } else {
                ops.push(Op {
                    tag: Tag::Delete,
                    a: ai,
                    b: bi,
                });
                (i, ai) = (i + 1, ai + 1);
            }
        }
    } else {
        for _ in am {
            ops.push(Op {
                tag: Tag::Delete,
                a: ai,
                b: bi,
            });
            ai += 1;
        }
        for _ in bm {
            ops.push(Op {
                tag: Tag::Insert,
                a: ai,
                b: bi,
            });
            bi += 1;
        }
    }
    for k in 0..suffix {
        ops.push(Op {
            tag: Tag::Equal,
            a: ai + k,
            b: bi + k,
        });
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_has_hunks_with_context() {
        let before = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        let after = "a\nb\nc\nd\nE\nf\ng\nh\ni\nj\nk\n";
        let d = unified_diff("x", before, after);
        assert!(d.contains("@@ -2,9 +2,10 @@"), "{d}");
        assert!(d.contains("-e\n+E\n"));
        assert!(d.contains("+k\n"));
        assert_eq!(unified_diff("x", before, before), "");
        let created = unified_diff("x", "", "one\ntwo\n");
        assert!(created.contains("@@ -0,0 +1,2 @@"), "{created}");
    }

    #[test]
    fn toml_preview_masks_secrets_keeps_comments() {
        let text = "# keep me\n[mcp_servers.gh]\ncommand = \"gh\"\nbearer_token = \"TOK123\"\nenv = { GH = \"SEC456\" }\n[mcp_servers.gh.http_headers]\nX = \"HDR789\"\n";
        let p = preview(text, Kind::Toml);
        assert!(p.contains("# keep me"));
        for s in ["TOK123", "SEC456", "HDR789"] {
            assert!(!p.contains(s), "{p}");
        }
        assert!(p.contains("command = \"gh\""));
    }

    #[tokio::test]
    async fn apply_detects_conflicts_and_skips_no_ops() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("f.json");
        std::fs::write(&path, "{}")?;
        let plan = WritePlan::single(FileChange::write(
            path.clone(),
            Some("{}".into()),
            "{\"a\":1}".into(),
            Kind::Json,
        ));
        std::fs::write(&path, "{\"b\":2}")?;
        assert!(matches!(
            apply(&plan).await,
            Err(ConfigError::Conflict { .. })
        ));
        assert_eq!(std::fs::read_to_string(&path)?, "{\"b\":2}");
        let same = WritePlan::single(FileChange::write(
            path.clone(),
            Some("{\"b\":2}".into()),
            "{\"b\":2}".into(),
            Kind::Json,
        ));
        assert!(same.is_no_op());
        assert_eq!(apply(&same).await?.written, Vec::<PathBuf>::new());
        Ok(())
    }
}
