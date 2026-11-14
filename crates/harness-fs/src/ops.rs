//! Core file operations with TS-compatible result shapes.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::collate::locale_compare;
use crate::error::FsError;
use crate::node_path::node_join;
use crate::policy::canonicalize;
use crate::sniff::is_probably_binary;

/// Default `readTextFile` window (128 KiB), as in the TS service.
pub const DEFAULT_TEXT_READ_BYTES: u64 = 128 * 1024;
/// Hard cap of a single `readTextFile` window (256 KiB).
pub const MAX_TEXT_READ_BYTES: u64 = 256 * 1024;
/// Default `readFileRange` window (256 KiB).
pub const DEFAULT_BINARY_READ_BYTES: u64 = 256 * 1024;
/// Hard cap of a single `readFileRange` window (1 MiB).
pub const MAX_BINARY_READ_BYTES: u64 = 1024 * 1024;

/// Entry type, as the TS `FileEntry["type"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    /// Regular file (and anything that is not a directory).
    File,
    /// Directory (symlinks to directories included).
    Directory,
}

/// Result of [`stat`]; `size`/`mtimeMs` are only present for files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileStat {
    /// The path exactly as requested.
    pub path: String,
    /// File or directory (symlinks are followed).
    #[serde(rename = "type")]
    pub kind: EntryKind,
    /// Size in bytes (files only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Modification time in fractional milliseconds since the epoch (files only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<f64>,
}

/// One directory entry, as the TS `FileEntry`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirEntry {
    /// Entry name.
    pub name: String,
    /// `path.join(dir, name)`.
    pub path: String,
    /// Kind, following symlinks.
    #[serde(rename = "type")]
    pub kind: EntryKind,
    /// Whether the entry itself is a symlink.
    pub is_symbolic_link: bool,
}

/// A bounded text window, as the TS `FileTextSlice`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextSlice {
    /// The path exactly as requested.
    pub path: String,
    /// UTF-8 (lossy) content; empty when binary.
    pub content: String,
    /// Effective start offset in bytes.
    pub offset: u64,
    /// Bytes actually read.
    pub bytes_read: u64,
    /// Total file size.
    pub total_bytes: u64,
    /// Whether bytes remain after this window.
    pub truncated: bool,
    /// Whether the window looks binary.
    pub is_binary: bool,
}

/// One `checkFilesExist` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileExists {
    /// The path exactly as requested.
    pub path: String,
    /// Whether the path exists.
    pub exists: bool,
}

fn mtime_ms(meta: &fs::Metadata) -> Option<f64> {
    let modified = meta.modified().ok()?;
    let since = modified.duration_since(UNIX_EPOCH).ok()?;
    #[allow(clippy::cast_precision_loss)]
    let ms = since.as_secs() as f64 * 1000.0 + f64::from(since.subsec_nanos()) / 1_000_000.0;
    Some(ms)
}

fn metadata(path: &Path, syscall: &'static str) -> Result<fs::Metadata, FsError> {
    fs::metadata(path).map_err(|e| FsError::from_io(&e, syscall, path))
}

/// `stat` following symlinks.
///
/// # Errors
/// Any OS error (e.g. `ENOENT`).
pub fn stat(path: &str) -> Result<FileStat, FsError> {
    let meta = metadata(Path::new(path), "stat")?;
    let is_dir = meta.is_dir();
    Ok(FileStat {
        path: path.to_owned(),
        kind: if is_dir {
            EntryKind::Directory
        } else {
            EntryKind::File
        },
        size: (!is_dir).then_some(meta.len()),
        mtime_ms: if is_dir { None } else { mtime_ms(&meta) },
    })
}

/// Lists `dir`, hiding dot-entries unless `include_hidden`. Directories come
/// first, then entries ordered by `localeCompare` on the name.
///
/// # Errors
/// Any OS error opening the directory.
pub fn readdir(dir: &str, include_hidden: bool) -> Result<Vec<DirEntry>, FsError> {
    let dir_path = Path::new(dir);
    let read = fs::read_dir(dir_path).map_err(|e| FsError::from_io(&e, "scandir", dir_path))?;
    let mut entries = Vec::new();
    for item in read {
        let item = item.map_err(|e| FsError::from_io(&e, "scandir", dir_path))?;
        let name = item.file_name().to_string_lossy().into_owned();
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        let path = node_join(dir, &name);
        let file_type = item.file_type().ok();
        let is_symbolic_link = file_type.is_some_and(|t| t.is_symlink());
        let is_dir = file_type.is_some_and(|t| t.is_dir());
        let kind = if is_dir {
            EntryKind::Directory
        } else if is_symbolic_link {
            // 软链接目录在 Dirent 上只报告 isSymbolicLink，需要跟随目标重新分类（与 TS 一致）。
            match fs::metadata(item.path()) {
                Ok(meta) if meta.is_dir() => EntryKind::Directory,
                _ => EntryKind::File,
            }
        } else {
            EntryKind::File
        };
        entries.push(DirEntry {
            name,
            path,
            kind,
            is_symbolic_link,
        });
    }
    entries.sort_by(|a, b| {
        if a.kind != b.kind {
            return if a.kind == EntryKind::Directory {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        locale_compare(&a.name, &b.name)
    });
    Ok(entries)
}

/// JS `Math.min(Math.max(trunc(length ?? default), 1), max)` with a
/// non-finite or absent value mapped to `default`.
#[must_use]
pub fn clamp_js_length(length: Option<f64>, default: u64, max: u64) -> u64 {
    let value = match length {
        Some(v) if v.is_finite() => v.trunc(),
        _ => return default.clamp(1, max),
    };
    #[allow(clippy::cast_precision_loss)]
    let max_f = max as f64;
    if value <= 1.0 {
        1
    } else if value >= max_f {
        max
    } else {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let v = value as u64;
        v
    }
}

/// JS `Math.max(0, Math.trunc(offset ?? 0))` as an unsigned byte offset.
#[must_use]
pub fn js_offset(offset: Option<f64>) -> u64 {
    match offset {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some(v) if v.is_finite() && v > 0.0 => v.trunc() as u64,
        Some(v) if v.is_infinite() && v > 0.0 => u64::MAX,
        _ => 0,
    }
}

fn open_regular(path: &Path) -> Result<(fs::File, u64), FsError> {
    let meta = metadata(path, "stat")?;
    if !meta.is_file() {
        return Err(FsError::not_a_file(&path.to_string_lossy()));
    }
    let file = fs::File::open(path).map_err(|e| FsError::from_io(&e, "open", path))?;
    Ok((file, meta.len()))
}

fn read_window(
    file: &mut fs::File,
    path: &Path,
    offset: u64,
    len: u64,
) -> Result<Vec<u8>, FsError> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| FsError::from_io(&e, "read", path))?;
    let capacity = usize::try_from(len).unwrap_or(usize::MAX);
    let mut buffer = Vec::with_capacity(capacity);
    file.take(len)
        .read_to_end(&mut buffer)
        .map_err(|e| FsError::from_io(&e, "read", path))?;
    Ok(buffer)
}

/// Reads a bounded text window (TS `readTextFile`). `offset`/`length` are
/// JS numbers: `length` defaults to 128 KiB and is clamped to `[1, 256 KiB]`.
///
/// # Errors
/// `Path is not a file` when the path is not a regular file, or any OS error.
pub fn read_text(
    path: &str,
    offset: Option<f64>,
    length: Option<f64>,
) -> Result<TextSlice, FsError> {
    let p = Path::new(path);
    let (mut file, total) = open_regular(p)?;
    let offset = js_offset(offset);
    if offset >= total {
        return Ok(TextSlice {
            path: path.to_owned(),
            content: String::new(),
            offset,
            bytes_read: 0,
            total_bytes: total,
            truncated: false,
            is_binary: false,
        });
    }
    let want =
        clamp_js_length(length, DEFAULT_TEXT_READ_BYTES, MAX_TEXT_READ_BYTES).min(total - offset);
    let chunk = read_window(&mut file, p, offset, want)?;
    let bytes_read = chunk.len() as u64;
    let is_binary = is_probably_binary(&chunk);
    Ok(TextSlice {
        path: path.to_owned(),
        content: if is_binary {
            String::new()
        } else {
            String::from_utf8_lossy(&chunk).into_owned()
        },
        offset,
        bytes_read,
        total_bytes: total,
        truncated: offset + bytes_read < total,
        is_binary,
    })
}

/// Reads raw bytes `[offset, offset+length)` (TS `readFileRange`). `length`
/// is clamped to `[1, 1 MiB]`; returns an empty vector at EOF.
///
/// # Errors
/// `Path is not a file` when the path is not a regular file, or any OS error.
pub fn read_range(
    path: &str,
    offset: Option<f64>,
    length: Option<f64>,
) -> Result<Vec<u8>, FsError> {
    let p = Path::new(path);
    let (mut file, total) = open_regular(p)?;
    let offset = js_offset(offset);
    if offset >= total {
        return Ok(Vec::new());
    }
    let want = clamp_js_length(length, DEFAULT_BINARY_READ_BYTES, MAX_BINARY_READ_BYTES)
        .min(total - offset);
    read_window(&mut file, p, offset, want)
}

/// Reads a whole file if it is at most `max_bytes` (media/binary previews).
///
/// Returns the bytes and the file size.
///
/// # Errors
/// `File is too large to preview: <path>` when the file exceeds `max_bytes`.
pub fn read_bytes_limited(path: &str, max_bytes: u64) -> Result<(Vec<u8>, u64), FsError> {
    let p = Path::new(path);
    let (mut file, total) = open_regular(p)?;
    if total > max_bytes {
        return Err(FsError::app(format!(
            "File is too large to preview: {path}"
        )));
    }
    Ok((read_window(&mut file, p, 0, total)?, total))
}

/// Maximum number of paths per [`check_files_exist`] call (TS limit).
pub const FILE_EXISTENCE_BATCH_LIMIT: usize = 15;

/// Existence check for several paths, preserving input order. `exists` is
/// `stat(path).isFile()` and `false` on any error, as in the TS service.
///
/// # Errors
/// More than [`FILE_EXISTENCE_BATCH_LIMIT`] paths.
pub fn check_files_exist(paths: &[String]) -> Result<Vec<FileExists>, FsError> {
    if paths.len() > FILE_EXISTENCE_BATCH_LIMIT {
        return Err(FsError::app(format!(
            "File existence check supports at most {FILE_EXISTENCE_BATCH_LIMIT} paths."
        )));
    }
    Ok(paths
        .iter()
        .map(|path| FileExists {
            path: path.clone(),
            exists: fs::metadata(path).is_ok_and(|m| m.is_file()),
        })
        .collect())
}

/// `realpath` without the Windows verbatim prefix.
///
/// # Errors
/// Any OS error (e.g. `ENOENT`).
pub fn realpath(path: &str) -> Result<String, FsError> {
    Ok(canonicalize(Path::new(path))?
        .to_string_lossy()
        .into_owned())
}

/// Creates a directory. Returns `true` when something was created.
///
/// # Errors
/// Any OS error; with `recursive`, an existing directory is not an error.
pub fn mkdir(path: &str, recursive: bool) -> Result<bool, FsError> {
    let p = Path::new(path);
    if recursive {
        if p.is_dir() {
            return Ok(false);
        }
        fs::create_dir_all(p).map_err(|e| FsError::from_io(&e, "mkdir", p))?;
        return Ok(true);
    }
    fs::create_dir(p).map_err(|e| FsError::from_io(&e, "mkdir", p))?;
    Ok(true)
}

/// Removes a file, symlink or directory. Missing paths succeed when `force`.
///
/// # Errors
/// Any OS error; `ENOTEMPTY` for a non-empty directory without `recursive`.
pub fn remove(path: &str, recursive: bool, force: bool) -> Result<(), FsError> {
    let p = Path::new(path);
    let meta = match fs::symlink_metadata(p) {
        Ok(meta) => meta,
        Err(e) if force && e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(FsError::from_io(&e, "unlink", p)),
    };
    let result = if meta.is_dir() {
        if recursive {
            fs::remove_dir_all(p)
        } else {
            fs::remove_dir(p)
        }
    } else {
        fs::remove_file(p).or_else(|e| {
            // Windows 目录软链接需要 remove_dir 删除。
            if cfg!(windows) && meta.file_type().is_symlink() {
                fs::remove_dir(p)
            } else {
                Err(e)
            }
        })
    };
    result.map_err(|e| FsError::from_io(&e, "unlink", p))
}

/// Renames `from` to `to`, replacing an existing file target.
///
/// # Errors
/// Any OS error.
pub fn rename(from: &str, to: &str) -> Result<(), FsError> {
    fs::rename(from, to).map_err(|e| FsError::from_io(&e, "rename", Path::new(from)))
}

/// Node `path.extname` on the last path segment (either separator).
fn node_extname(path: &str) -> &str {
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match base.rfind('.') {
        Some(index) if index > 0 && !base[..index].chars().all(|c| c == '.') => &base[index..],
        _ => "",
    }
}

/// Media type for a preview: the TS image table, then the shared
/// `MEDIA_PREVIEW_FORMATS` (audio/video), else `application/octet-stream`.
#[must_use]
pub fn media_type_for_path(path: &str) -> &'static str {
    const FORMATS: [(&str, &str); 11] = [
        (".mp4", "video/mp4"),
        (".mov", "video/quicktime"),
        (".webm", "video/webm"),
        (".m4v", "video/x-m4v"),
        (".mp3", "audio/mpeg"),
        (".wav", "audio/wav"),
        (".m4a", "audio/mp4"),
        (".ogg", "audio/ogg"),
        (".opus", "audio/opus"),
        (".flac", "audio/flac"),
        (".weba", "audio/webm"),
    ];
    let ext = node_extname(path).to_lowercase();
    let image = match ext.as_str() {
        ".apng" => Some("image/apng"),
        ".avif" => Some("image/avif"),
        ".bmp" => Some("image/bmp"),
        ".gif" => Some("image/gif"),
        ".ico" => Some("image/x-icon"),
        ".jpeg" | ".jpg" => Some("image/jpeg"),
        ".png" => Some("image/png"),
        ".svg" => Some("image/svg+xml"),
        ".webp" => Some("image/webp"),
        _ => None,
    };
    if let Some(media) = image {
        return media;
    }
    let normalized = path.replace('\\', "/").to_lowercase();
    FORMATS
        .iter()
        .find(|(extension, _)| normalized.ends_with(extension))
        .map_or("application/octet-stream", |(_, media)| media)
}
