//! Structured diff (`harness_core_api::diff`) → plain view rows for the unified and side-by-side layouts.
//! Pure functions; runs on the `harness-panels` thread.

use harness_core_api::diff::{DiffLineKind, FileDiff, FileDiffStatus};

/// Rows beyond this are cut (the viewer is not a pager for generated files).
pub(crate) const MAX_ROWS: usize = 20_000;

pub(crate) const CONTEXT: i32 = 0;
pub(crate) const ADDED: i32 = 1;
pub(crate) const REMOVED: i32 = 2;
pub(crate) const HUNK: i32 = 3;
pub(crate) const NOTE: i32 = 4;
pub(crate) const EMPTY: i32 = -1;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Row {
    pub kind: i32,
    pub old_no: String,
    pub new_no: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Pair {
    pub hunk: bool,
    pub header: String,
    pub left_kind: i32,
    pub left_no: String,
    pub left_text: String,
    pub right_kind: i32,
    pub right_no: String,
    pub right_text: String,
}

/// View of one file: rows for both layouts, or a notice when there is nothing to draw.
#[derive(Debug, Clone, Default)]
pub(crate) struct FileView {
    pub rows: Vec<Row>,
    pub pairs: Vec<Pair>,
    pub notice: Option<&'static str>,
    pub cut: bool,
}

fn no(n: Option<u32>) -> String {
    n.map(|n| n.to_string()).unwrap_or_default()
}

pub(crate) fn status_code(status: FileDiffStatus) -> &'static str {
    match status {
        FileDiffStatus::Added => "A",
        FileDiffStatus::Deleted => "D",
        FileDiffStatus::Renamed => "R",
        FileDiffStatus::Modified => "M",
    }
}

/// Builds both layouts for `file`.
pub(crate) fn file_view(file: &FileDiff) -> FileView {
    if file.binary {
        return FileView {
            notice: Some("binary"),
            ..FileView::default()
        };
    }
    if file.hunks.is_empty() {
        return FileView {
            notice: Some("empty"),
            ..FileView::default()
        };
    }
    let mut view = FileView::default();
    for hunk in &file.hunks {
        if view.rows.len() >= MAX_ROWS {
            view.cut = true;
            break;
        }
        view.rows.push(Row {
            kind: HUNK,
            text: hunk.header.clone(),
            ..Row::default()
        });
        view.pairs.push(Pair {
            hunk: true,
            header: hunk.header.clone(),
            left_kind: EMPTY,
            right_kind: EMPTY,
            ..Pair::default()
        });
        // Side by side: a run of removed lines is paired with the following run of added lines.
        let mut removed: Vec<&harness_core_api::diff::DiffLine> = Vec::new();
        let mut added: Vec<&harness_core_api::diff::DiffLine> = Vec::new();
        let flush = |removed: &mut Vec<&harness_core_api::diff::DiffLine>,
                     added: &mut Vec<&harness_core_api::diff::DiffLine>,
                     pairs: &mut Vec<Pair>| {
            for i in 0..removed.len().max(added.len()) {
                let left = removed.get(i);
                let right = added.get(i);
                pairs.push(Pair {
                    hunk: false,
                    header: String::new(),
                    left_kind: left.map_or(EMPTY, |_| REMOVED),
                    left_no: left.map(|l| no(l.old_line)).unwrap_or_default(),
                    left_text: left.map(|l| l.text.clone()).unwrap_or_default(),
                    right_kind: right.map_or(EMPTY, |_| ADDED),
                    right_no: right.map(|l| no(l.new_line)).unwrap_or_default(),
                    right_text: right.map(|l| l.text.clone()).unwrap_or_default(),
                });
            }
            removed.clear();
            added.clear();
        };
        for line in &hunk.lines {
            let kind = match line.kind {
                DiffLineKind::Context => CONTEXT,
                DiffLineKind::Added => ADDED,
                DiffLineKind::Removed => REMOVED,
                DiffLineKind::NoNewline => NOTE,
            };
            view.rows.push(Row {
                kind,
                old_no: no(line.old_line),
                new_no: no(line.new_line),
                text: line.text.clone(),
            });
            match line.kind {
                DiffLineKind::Removed => {
                    if !added.is_empty() {
                        flush(&mut removed, &mut added, &mut view.pairs);
                    }
                    removed.push(line);
                }
                DiffLineKind::Added => added.push(line),
                DiffLineKind::Context => {
                    flush(&mut removed, &mut added, &mut view.pairs);
                    view.pairs.push(Pair {
                        hunk: false,
                        header: String::new(),
                        left_kind: CONTEXT,
                        left_no: no(line.old_line),
                        left_text: line.text.clone(),
                        right_kind: CONTEXT,
                        right_no: no(line.new_line),
                        right_text: line.text.clone(),
                    });
                }
                DiffLineKind::NoNewline => {}
            }
        }
        flush(&mut removed, &mut added, &mut view.pairs);
    }
    view
}
