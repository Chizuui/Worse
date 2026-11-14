//! Structured unified diffs shared by git diffs and turn diffs (spec phase-8-core.md §15.3).
//!
//! [`parse_unified`] is a pure function: it turns `git diff` output (one or more files) into [`FileDiff`]s
//! with per-line old/new line numbers computed from the hunk headers.

use serde::{Deserialize, Serialize};

/// What happened to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FileDiffStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

/// Kind of one diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
    /// `\ No newline at end of file` marker.
    NoNewline,
}

/// One line of a hunk. `old_line` / `new_line` are 1-based; `None` where the line does not exist on that side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    /// Line text without the leading marker and without the line terminator.
    pub text: String,
}

/// One `@@ -a,b +c,d @@` hunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffHunk {
    /// The full header line (`@@ -1,3 +1,4 @@ fn main`).
    pub header: String,
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub lines: Vec<DiffLine>,
}

/// Diff of one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDiff {
    /// New path (old path for deletions), relative to the repository / workspace with `/`.
    pub path: String,
    /// Previous path of a rename.
    pub old_path: Option<String>,
    pub status: FileDiffStatus,
    pub binary: bool,
    pub added: u32,
    pub removed: u32,
    pub hunks: Vec<DiffHunk>,
}

impl FileDiff {
    fn new(path: String) -> Self {
        Self {
            path,
            old_path: None,
            status: FileDiffStatus::Modified,
            binary: false,
            added: 0,
            removed: 0,
            hunks: Vec::new(),
        }
    }
}

/// Strips git's `a/` / `b/` prefix and C-style quoting from a header path; `/dev/null` → `None`.
fn header_path(raw: &str) -> Option<String> {
    let raw = raw.trim_end_matches('\t').trim();
    if raw == "/dev/null" {
        return None;
    }
    let unquoted = if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        unquote(&raw[1..raw.len() - 1])
    } else {
        raw.to_owned()
    };
    let stripped = unquoted
        .strip_prefix("a/")
        .or_else(|| unquoted.strip_prefix("b/"))
        .map_or_else(|| unquoted.clone(), str::to_owned);
    Some(stripped)
}

/// Undoes git's C-style path quoting (`\"`, `\\`, `\t`, `\n`, octal UTF-8 bytes).
fn unquote(text: &str) -> String {
    let mut bytes = Vec::with_capacity(text.len());
    let mut chars = text.bytes().peekable();
    while let Some(b) = chars.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match chars.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(d @ b'0'..=b'7') => {
                let mut value = u32::from(d - b'0');
                for _ in 0..2 {
                    match chars.peek() {
                        Some(&n @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(n - b'0');
                            chars.next();
                        }
                        _ => break,
                    }
                }
                bytes.push(u8::try_from(value & 0xff).unwrap_or(b'?'));
            }
            Some(other) => bytes.push(other),
            None => bytes.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Parses `-a,b +c,d` of a hunk header. A missing count means 1.
fn parse_range(text: &str) -> Option<(u32, u32)> {
    let (start, count) = match text.split_once(',') {
        Some((s, c)) => (s, c),
        None => (text, "1"),
    };
    Some((start.parse().ok()?, count.parse().ok()?))
}

fn parse_hunk_header(line: &str) -> Option<DiffHunk> {
    let rest = line.strip_prefix("@@ ")?;
    let end = rest.find(" @@")?;
    let mut parts = rest[..end].split_whitespace();
    let (old_start, old_count) = parse_range(parts.next()?.strip_prefix('-')?)?;
    let (new_start, new_count) = parse_range(parts.next()?.strip_prefix('+')?)?;
    Some(DiffHunk {
        header: line.to_owned(),
        old_start,
        old_count,
        new_start,
        new_count,
        lines: Vec::new(),
    })
}

/// Path from `diff --git a/x b/y` when the `---`/`+++` lines are absent (binary, mode-only, pure rename).
fn git_header_path(line: &str) -> Option<String> {
    let rest = line.strip_prefix("diff --git ")?;
    if let Some(index) = rest.rfind(" b/") {
        return header_path(&rest[index + 1..]);
    }
    rest.split_whitespace().last().and_then(header_path)
}

/// Line-number state of the hunk being read.
#[derive(Default)]
struct HunkCursor {
    old_no: u32,
    new_no: u32,
    old_left: u32,
    new_left: u32,
    open: bool,
}

impl HunkCursor {
    /// Adds a body line to the last hunk of `file`; `false` when `line` is not a body line.
    fn push(&mut self, file: &mut FileDiff, line: &str) -> bool {
        let Some(hunk) = file.hunks.last_mut() else {
            return false;
        };
        let (kind, body) = if let Some(body) = line.strip_prefix('+') {
            (DiffLineKind::Added, body)
        } else if let Some(body) = line.strip_prefix('-') {
            (DiffLineKind::Removed, body)
        } else if let Some(body) = line.strip_prefix(' ') {
            (DiffLineKind::Context, body)
        } else {
            return false;
        };
        let has_old = kind != DiffLineKind::Added;
        let has_new = kind != DiffLineKind::Removed;
        hunk.lines.push(DiffLine {
            kind,
            old_line: has_old.then_some(self.old_no),
            new_line: has_new.then_some(self.new_no),
            text: body.to_owned(),
        });
        if has_old {
            self.old_no += 1;
            self.old_left = self.old_left.saturating_sub(1);
        }
        if has_new {
            self.new_no += 1;
            self.new_left = self.new_left.saturating_sub(1);
        }
        match kind {
            DiffLineKind::Added => file.added += 1,
            DiffLineKind::Removed => file.removed += 1,
            _ => {}
        }
        true
    }
}

/// Applies a file header line (`---`, `+++`, mode, rename, binary) to `file`.
fn apply_header(file: &mut FileDiff, line: &str) {
    if let Some(rest) = line.strip_prefix("--- ") {
        match header_path(rest) {
            Some(old) if file.path.is_empty() => file.path = old,
            Some(_) => {}
            None => file.status = FileDiffStatus::Added,
        }
    } else if let Some(rest) = line.strip_prefix("+++ ") {
        match header_path(rest) {
            Some(new) => file.path = new,
            None => file.status = FileDiffStatus::Deleted,
        }
    } else if line.starts_with("new file mode") {
        file.status = FileDiffStatus::Added;
    } else if line.starts_with("deleted file mode") {
        file.status = FileDiffStatus::Deleted;
    } else if let Some(from) = line.strip_prefix("rename from ") {
        file.status = FileDiffStatus::Renamed;
        file.old_path = Some(unquote_plain(from));
    } else if let Some(to) = line.strip_prefix("rename to ") {
        file.status = FileDiffStatus::Renamed;
        file.path = unquote_plain(to);
    } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
        file.binary = true;
    }
}

/// Parses unified diff text (`git diff` output, possibly several files) into structured file diffs.
#[must_use]
pub fn parse_unified(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut cursor = HunkCursor::default();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.starts_with("diff --git ") {
            files.push(FileDiff::new(git_header_path(line).unwrap_or_default()));
            cursor.open = false;
            continue;
        }
        if files.is_empty() && (line.starts_with("--- ") || line.starts_with("@@ ")) {
            // Plain unified diff without a `diff --git` header.
            files.push(FileDiff::new(String::new()));
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if line.starts_with('\\') {
            if let Some(hunk) = file.hunks.last_mut() {
                hunk.lines.push(DiffLine {
                    kind: DiffLineKind::NoNewline,
                    old_line: None,
                    new_line: None,
                    text: line.trim_start_matches('\\').trim().to_owned(),
                });
            }
            continue;
        }
        if cursor.open && cursor.old_left == 0 && cursor.new_left == 0 {
            // The hunk header's counts say where it ends; a following `--- ` line then starts a new file
            // (plain multi-file patches have no `diff --git` separator).
            cursor.open = false;
        }
        if cursor.open {
            if cursor.push(file, line) || line.is_empty() {
                // A bare empty line inside a hunk only appears as the trailing split piece.
                continue;
            }
            cursor.open = false;
        }
        if let Some(hunk) = parse_hunk_header(line) {
            cursor = HunkCursor {
                old_no: hunk.old_start,
                new_no: hunk.new_start,
                old_left: hunk.old_count,
                new_left: hunk.new_count,
                open: true,
            };
            file.hunks.push(hunk);
        } else {
            apply_header(file, line);
        }
    }
    files
}
fn unquote_plain(text: &str) -> String {
    let text = text.trim();
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        unquote(&text[1..text.len() - 1])
    } else {
        text.to_owned()
    }
}
