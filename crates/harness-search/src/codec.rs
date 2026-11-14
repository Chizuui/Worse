//! Port of `packages/shared/src/workspaceFileEntriesCodec.ts`.
//!
//! Each line is `type\trelativePath`; `\\`, `\t` and `\n` in the path are
//! escaped. `name` and `path` are rebuilt from the root on unpack.

use crate::index::{EntryType, WorkspaceFileEntry};

fn escape_field(value: &str, out: &mut String) {
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
}

fn unescape_field(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some(other) => out.push(other),
            // JS 正则 /\\(.)/ 不匹配末尾孤立反斜杠，原样保留。
            None => out.push('\\'),
        }
    }
    out
}

/// `packWorkspaceFileEntries`.
#[must_use]
pub fn pack<'a>(entries: impl IntoIterator<Item = (EntryType, &'a str)>) -> String {
    let mut out = String::new();
    for (index, (kind, relative_path)) in entries.into_iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(kind.as_str());
        out.push('\t');
        escape_field(relative_path, &mut out);
    }
    out
}

/// Prefix and separator used to rebuild absolute paths from `root_path`
/// (`\` if the root contains one, `/` otherwise).
#[must_use]
pub fn path_prefix(root_path: &str) -> (String, char) {
    let separator = if root_path.contains('\\') { '\\' } else { '/' };
    let prefix = if root_path.ends_with('/') || root_path.ends_with('\\') {
        root_path.to_owned()
    } else {
        format!("{root_path}{separator}")
    };
    (prefix, separator)
}

/// Rebuilds the absolute path of `relative_path` under `prefix`.
#[must_use]
pub fn join_relative(prefix: &str, separator: char, relative_path: &str) -> String {
    if separator == '/' {
        format!("{prefix}{relative_path}")
    } else {
        format!("{prefix}{}", relative_path.replace('/', "\\"))
    }
}

/// `unpackWorkspaceFileEntries`.
#[must_use]
pub fn unpack(packed: &str, root_path: &str) -> Vec<WorkspaceFileEntry> {
    if packed.is_empty() {
        return Vec::new();
    }
    let (prefix, separator) = path_prefix(root_path);
    let mut entries = Vec::new();
    for line in packed.split('\n') {
        let Some((kind, rest)) = line.split_once('\t') else {
            continue;
        };
        let relative_path = unescape_field(rest);
        let name = relative_path
            .rsplit_once('/')
            .map_or(relative_path.as_str(), |(_, n)| n)
            .to_owned();
        entries.push(WorkspaceFileEntry {
            name,
            path: join_relative(&prefix, separator, &relative_path),
            kind: if kind == "directory" {
                EntryType::Directory
            } else {
                EntryType::File
            },
            relative_path,
        });
    }
    entries
}
