//! Workspace file-system primitives for the Harness Rust Host.
//!
//! Every function in this crate is synchronous and may block on disk I/O.
//! Async callers (the `harness-host` dispatcher) must run them through
//! `tokio::task::spawn_blocking` or a dedicated thread.
//!
//! Result shapes mirror the `ZCode` TypeScript `IFileService` so that the Rust
//! Host can be a drop-in replacement: paths are echoed exactly as given,
//! child paths are built with Node `path.join` semantics, and binary sniffing
//! uses the same heuristic as `fileService.ts`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod atomic;
mod collate;
mod error;
pub mod filetype;
mod node_path;
mod ops;
mod path_identity;
mod policy;
mod sniff;

#[cfg(test)]
mod tests;

pub use atomic::{atomic_write, write_file};
pub use collate::locale_compare;
pub use error::{describe, FsError};
pub use node_path::{node_join, node_normalize};
pub use ops::{
    check_files_exist, clamp_js_length, js_offset, media_type_for_path, mkdir, read_bytes_limited,
    read_range, read_text, readdir, realpath, remove, rename, stat, DirEntry, EntryKind,
    FileExists, FileStat, TextSlice, DEFAULT_BINARY_READ_BYTES, DEFAULT_TEXT_READ_BYTES,
    FILE_EXISTENCE_BATCH_LIMIT, MAX_BINARY_READ_BYTES, MAX_TEXT_READ_BYTES,
};
pub use path_identity::{
    identity_key, is_within, normalize_lexical, strip_verbatim, CaseSensitivity,
};
pub use policy::{SymlinkPolicy, WorkspaceRoot};
pub use sniff::is_probably_binary;
