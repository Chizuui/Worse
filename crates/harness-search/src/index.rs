//! The workspace file index (TS `ensureWorkspaceFileIndex` scan step).
//!
//! The walk uses the `ignore` crate's parallel walker with every standard
//! filter disabled; the TS rules are applied explicitly per entry so that the
//! result set is identical: root `.zcodeignore` skipped, ignore rules tested
//! with directory semantics, then the default mention filter decides
//! include/traverse. Symlinked directories are listed (typed by their target)
//! but never descended into.

use std::fs;
use std::path::Path;
use std::sync::mpsc;
use std::sync::OnceLock;

use harness_fs::locale_compare;
use ignore::{WalkBuilder, WalkState};
use serde::{Deserialize, Serialize};

use crate::cancel::CancellationToken;
use crate::codec;
use crate::error::SearchError;
use crate::filter;
use crate::fuzzy::Candidate;
use crate::ignore_rules::{load_rules, IgnoreRules, WORKSPACE_FILE_SEARCH_IGNORE_FILE_NAME};

/// Scan concurrency (TS `WORKSPACE_FILE_LIST_SCAN_CONCURRENCY`).
const SCAN_THREADS: usize = 8;

/// Entry type, as TS `WorkspaceFileEntry["type"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryType {
    /// A file (or a symlink to a non-directory).
    File,
    /// A directory (or a symlink to one).
    Directory,
}

impl EntryType {
    /// Wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
        }
    }
}

/// One index row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    /// Entry type.
    pub kind: EntryType,
    /// Posix path relative to the root.
    pub relative_path: String,
}

/// TS `WorkspaceFileEntry` as returned over the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFileEntry {
    /// Last path segment.
    pub name: String,
    /// Absolute path rebuilt from the root.
    pub path: String,
    /// Posix relative path.
    pub relative_path: String,
    /// Entry type.
    #[serde(rename = "type")]
    pub kind: EntryType,
}

/// A built index: sorted entries plus the packed UTF-16 string.
#[derive(Debug)]
pub struct WorkspaceIndex {
    /// Root as given by the caller.
    pub root_path: String,
    /// Entries in TS order (directories first, then `localeCompare`).
    pub entries: Vec<IndexEntry>,
    packed_utf16: Vec<u16>,
    candidates: OnceLock<Vec<Candidate>>,
}

impl WorkspaceIndex {
    /// Builds the index value from already sorted entries.
    #[must_use]
    pub fn from_sorted(root_path: &str, entries: Vec<IndexEntry>) -> Self {
        let packed = codec::pack(entries.iter().map(|e| (e.kind, e.relative_path.as_str())));
        Self {
            root_path: root_path.to_owned(),
            entries,
            packed_utf16: packed.encode_utf16().collect(),
            candidates: OnceLock::new(),
        }
    }

    /// Packed length in UTF-16 code units (`listWorkspaceFilesLength`).
    #[must_use]
    pub fn packed_len(&self) -> usize {
        self.packed_utf16.len()
    }

    /// `listWorkspaceFilesRange` slice, lossy at split surrogate pairs.
    #[must_use]
    pub fn packed_range(&self, offset: usize, length: usize) -> String {
        if offset >= self.packed_utf16.len() {
            return String::new();
        }
        crate::utf16::slice_lossy(&self.packed_utf16, offset, offset.saturating_add(length))
    }

    /// Full packed string.
    #[must_use]
    pub fn packed(&self) -> String {
        String::from_utf16_lossy(&self.packed_utf16)
    }

    /// Search candidates for `root_path` (built once, lazily).
    pub(crate) fn candidates(&self, root_path: &str) -> &[Candidate] {
        self.candidates.get_or_init(|| {
            let (prefix, separator) = codec::path_prefix(root_path);
            self.entries
                .iter()
                .map(|e| Candidate::new(e, &prefix, separator))
                .collect()
        })
    }
}

fn entry_type(entry: &ignore::DirEntry) -> Option<(EntryType, bool)> {
    let file_type = entry.file_type()?;
    if file_type.is_dir() {
        return Some((EntryType::Directory, false));
    }
    if file_type.is_symlink() {
        // 软链接按目标类型分类（与 TS resolveReaddirEntryType 一致），但永不进入遍历。
        let is_dir = fs::metadata(entry.path()).is_ok_and(|m| m.is_dir());
        let kind = if is_dir {
            EntryType::Directory
        } else {
            EntryType::File
        };
        return Some((kind, true));
    }
    Some((EntryType::File, false))
}

fn relative_posix(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    Some(parts.join("/"))
}

/// Walks `root_path` and returns the sorted index.
///
/// # Errors
/// [`SearchError::Cancelled`] when `cancel` fires during the scan.
pub fn build_index(
    root_path: &str,
    cancel: &CancellationToken,
) -> Result<WorkspaceIndex, SearchError> {
    let root = Path::new(root_path);
    let rules = load_rules(root);
    let mut entries = walk(root, &rules, cancel);
    if cancel.is_cancelled() {
        return Err(SearchError::Cancelled);
    }
    entries.sort_by(|a, b| {
        if a.kind != b.kind {
            return if a.kind == EntryType::Directory {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        locale_compare(&a.relative_path, &b.relative_path)
    });
    Ok(WorkspaceIndex::from_sorted(root_path, entries))
}

fn walk(root: &Path, rules: &IgnoreRules, cancel: &CancellationToken) -> Vec<IndexEntry> {
    let (tx, rx) = mpsc::channel::<Vec<IndexEntry>>();
    let walker = WalkBuilder::new(root)
        .standard_filters(false)
        .follow_links(false)
        .threads(SCAN_THREADS)
        .build_parallel();
    walker.run(|| {
        let tx = tx.clone();
        let mut batch = BatchSender {
            tx,
            buffer: Vec::new(),
        };
        Box::new(move |result| {
            if cancel.is_cancelled() {
                return WalkState::Quit;
            }
            let entry = match result {
                Ok(entry) => entry,
                Err(error) => {
                    // TS 对 EACCES/EPERM/ENOENT 的目录跳过；这里对任意读取错误都跳过该目录并记录。
                    tracing::debug!(%error, "workspace index: skipping unreadable entry");
                    return WalkState::Continue;
                }
            };
            if entry.depth() == 0 {
                return WalkState::Continue;
            }
            visit(root, rules, &entry, &mut batch)
        })
    });
    drop(tx);
    rx.into_iter().flatten().collect()
}

struct BatchSender {
    tx: mpsc::Sender<Vec<IndexEntry>>,
    buffer: Vec<IndexEntry>,
}

impl BatchSender {
    fn push(&mut self, entry: IndexEntry) {
        self.buffer.push(entry);
        if self.buffer.len() >= 1024 {
            let _ = self.tx.send(std::mem::take(&mut self.buffer));
        }
    }
}

impl Drop for BatchSender {
    fn drop(&mut self) {
        if !self.buffer.is_empty() {
            let _ = self.tx.send(std::mem::take(&mut self.buffer));
        }
    }
}

fn visit(
    root: &Path,
    rules: &IgnoreRules,
    entry: &ignore::DirEntry,
    out: &mut BatchSender,
) -> WalkState {
    let Some(relative_path) = relative_posix(root, entry.path()) else {
        return WalkState::Continue;
    };
    let Some((kind, is_symlink)) = entry_type(entry) else {
        return WalkState::Continue;
    };
    let real_dir = kind == EntryType::Directory && !is_symlink;
    let skip_or_continue = if real_dir {
        WalkState::Skip
    } else {
        WalkState::Continue
    };
    if relative_path == WORKSPACE_FILE_SEARCH_IGNORE_FILE_NAME {
        return skip_or_continue;
    }
    let is_dir = kind == EntryType::Directory;
    if rules.is_ignored(&relative_path, is_dir) {
        return skip_or_continue;
    }
    let name = entry.file_name().to_string_lossy();
    let decision = filter::evaluate(&name, &relative_path, is_dir, true);
    let traverse = real_dir && decision.traverse;
    if decision.include {
        out.push(IndexEntry {
            kind,
            relative_path,
        });
    }
    if traverse || !real_dir {
        WalkState::Continue
    } else {
        WalkState::Skip
    }
}
