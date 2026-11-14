//! Workspace file index, fuzzy file-name search and streaming text search.
//!
//! The crate reproduces the `ZCode` TS workspace index
//! (`fileService.ensureWorkspaceFileIndex`) so that `searchWorkspaceFiles`,
//! `listWorkspaceFilesLength`/`Range` are drop-in compatible:
//!
//! * [`ignore_rules`]: `.zcodeignore` / `.gitignore` / built-in rule loading
//!   (gitignore syntax via the `ignore` crate, case-insensitive like the
//!   `ignore` npm package);
//! * [`filter`]: the default mention filter (hidden dirs, `.env`, binaries);
//! * [`index`]: a parallel walk with the `ignore` crate, ICU-collated sort and
//!   the columnar pack codec;
//! * [`fuzzy`]: a literal port of `filterWorkspaceFileSearchCandidates`
//!   computed on UTF-16 code units, like JS;
//! * [`cache`]: the 60 s / 4-entry LRU index cache with shared in-flight scans;
//! * [`text`]: cancellable, ordered, streaming content search with the
//!   ripgrep libraries.
//!
//! All entry points block; async callers must use `spawn_blocking`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod cache;
mod cancel;
pub mod codec;
mod error;
pub mod filter;
pub mod fuzzy;
pub mod ignore_rules;
pub mod index;
pub mod text;
pub mod utf16;

#[cfg(test)]
mod tests;

pub use cache::{IndexCache, INDEX_CACHE_MAX_ENTRIES, INDEX_CACHE_TTL};
pub use cancel::CancellationToken;
pub use error::SearchError;
pub use fuzzy::{search_entries, WORKSPACE_FILE_SEARCH_DISPLAY_CAP};
pub use index::{build_index, EntryType, IndexEntry, WorkspaceFileEntry, WorkspaceIndex};
pub use text::{search_text, TextMatch, TextSearchOptions, TextSearchSummary};
