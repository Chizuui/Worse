//! Streaming text search over the workspace index (`search.text`).
//!
//! Files are the index's file entries, in index order. Batches of files are
//! searched in parallel with the ripgrep libraries, and results are emitted in
//! index order so output is deterministic. Files above `max_file_bytes` and
//! binary files (TS sniff on the first 8 KiB) are skipped. One match is
//! reported per matching line (its first match).

use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use harness_fs::is_probably_binary;
use serde::{Deserialize, Serialize};

use crate::cancel::CancellationToken;
use crate::codec;
use crate::error::SearchError;
use crate::index::{EntryType, WorkspaceIndex};
use crate::utf16::{truncate_utf16, utf16_len};

/// Default cap on matching lines.
pub const DEFAULT_MAX_RESULTS: usize = 2000;
/// Default per-file size cap (1 MiB).
pub const DEFAULT_MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Bytes inspected by the binary sniff.
const SNIFF_BYTES: usize = 8 * 1024;
/// Maximum UTF-16 length of a reported line.
const MAX_LINE_UNITS: usize = 500;
/// Progress batching limits.
const PROGRESS_MAX_MATCHES: usize = 200;
const PROGRESS_MAX_DELAY: Duration = Duration::from_millis(50);
/// Files searched per ordered parallel batch.
const FILES_PER_BATCH: usize = 256;

/// Options of one text search.
#[derive(Debug, Clone)]
pub struct TextSearchOptions {
    /// Literal text or regex pattern.
    pub query: String,
    /// Interpret `query` as a Rust `regex` pattern.
    pub is_regex: bool,
    /// Case-sensitive matching.
    pub case_sensitive: bool,
    /// Stop after this many matching lines.
    pub max_results: usize,
    /// Skip files larger than this.
    pub max_file_bytes: u64,
}

/// One matching line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextMatch {
    /// Posix path relative to the root.
    pub relative_path: String,
    /// 1-based line number.
    pub line: u64,
    /// 1-based UTF-16 column of the first match.
    pub column: usize,
    /// Line text without terminator, at most 500 UTF-16 units.
    pub text: String,
}

/// Final result of a text search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextSearchSummary {
    /// Matching lines reported.
    pub match_count: usize,
    /// Files with at least one reported match.
    pub file_count: usize,
    /// Whether `max_results` stopped the search.
    pub truncated: bool,
    /// Always `false` in a returned summary (cancellation is an error).
    pub cancelled: bool,
}

fn build_matcher(options: &TextSearchOptions) -> Result<RegexMatcher, SearchError> {
    RegexMatcherBuilder::new()
        .case_insensitive(!options.case_sensitive)
        .fixed_strings(!options.is_regex)
        .line_terminator(Some(b'\n'))
        .build(&options.query)
        .map_err(|e| SearchError::InvalidPattern(e.to_string()))
}

struct LineSink<'a> {
    matcher: &'a RegexMatcher,
    relative_path: &'a str,
    limit: usize,
    out: Vec<TextMatch>,
}

impl Sink for LineSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        let first_line = mat.line_number().unwrap_or(1);
        for (line_number, raw) in (first_line..).zip(mat.bytes().split_inclusive(|b| *b == b'\n')) {
            if self.out.len() >= self.limit {
                return Ok(false);
            }
            let line = raw.strip_suffix(b"\n").unwrap_or(raw);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if let Ok(Some(found)) = self.matcher.find(line) {
                let prefix = String::from_utf8_lossy(&line[..found.start()]);
                self.out.push(TextMatch {
                    relative_path: self.relative_path.to_owned(),
                    line: line_number,
                    column: utf16_len(&prefix) + 1,
                    text: truncate_utf16(&String::from_utf8_lossy(line), MAX_LINE_UNITS),
                });
            }
        }
        Ok(self.out.len() < self.limit)
    }
}

fn search_file(
    matcher: &RegexMatcher,
    path: &str,
    relative_path: &str,
    max_file_bytes: u64,
    limit: usize,
) -> Vec<TextMatch> {
    let Ok(meta) = fs::metadata(path) else {
        return Vec::new();
    };
    if !meta.is_file() || meta.len() > max_file_bytes {
        return Vec::new();
    }
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    if is_probably_binary(&bytes[..bytes.len().min(SNIFF_BYTES)]) {
        return Vec::new();
    }
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::none())
        .build();
    let mut sink = LineSink {
        matcher,
        relative_path,
        limit,
        out: Vec::new(),
    };
    if let Err(error) = searcher.search_slice(matcher, &bytes, &mut sink) {
        tracing::debug!(%error, path, "text search failed for file");
    }
    sink.out
}

struct Emitter<'a> {
    pending: Vec<TextMatch>,
    last_flush: Instant,
    on_progress: &'a mut dyn FnMut(Vec<TextMatch>),
}

impl Emitter<'_> {
    fn push(&mut self, matches: Vec<TextMatch>) {
        self.pending.extend(matches);
        while self.pending.len() >= PROGRESS_MAX_MATCHES {
            let rest = self.pending.split_off(PROGRESS_MAX_MATCHES);
            let batch = std::mem::replace(&mut self.pending, rest);
            (self.on_progress)(batch);
            self.last_flush = Instant::now();
        }
        if !self.pending.is_empty() && self.last_flush.elapsed() >= PROGRESS_MAX_DELAY {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if !self.pending.is_empty() {
            (self.on_progress)(std::mem::take(&mut self.pending));
        }
        self.last_flush = Instant::now();
    }
}

/// Searches every indexed file. Progress batches go to `on_progress` in index
/// order, all before this function returns.
///
/// # Errors
/// [`SearchError::Cancelled`] on cancellation, [`SearchError::InvalidPattern`]
/// when the regex does not compile.
pub fn search_text(
    index: &WorkspaceIndex,
    root_path: &str,
    options: &TextSearchOptions,
    cancel: &CancellationToken,
    on_progress: &mut dyn FnMut(Vec<TextMatch>),
) -> Result<TextSearchSummary, SearchError> {
    let mut summary = TextSearchSummary {
        match_count: 0,
        file_count: 0,
        truncated: false,
        cancelled: false,
    };
    if options.query.is_empty() || options.max_results == 0 {
        return Ok(summary);
    }
    let matcher = build_matcher(options)?;
    let (prefix, separator) = codec::path_prefix(root_path);
    let files: Vec<(&str, String)> = index
        .entries
        .iter()
        .filter(|e| e.kind == EntryType::File)
        .map(|e| {
            (
                e.relative_path.as_str(),
                codec::join_relative(&prefix, separator, &e.relative_path),
            )
        })
        .collect();
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    let mut emitter = Emitter {
        pending: Vec::new(),
        last_flush: Instant::now(),
        on_progress,
    };
    for batch in files.chunks(FILES_PER_BATCH) {
        if cancel.is_cancelled() {
            return Err(SearchError::Cancelled);
        }
        let remaining = options.max_results - summary.match_count;
        let results = search_batch(
            &matcher,
            batch,
            options.max_file_bytes,
            remaining,
            workers,
            cancel,
        );
        for matches in results {
            if matches.is_empty() {
                continue;
            }
            let remaining = options.max_results - summary.match_count;
            let take = matches.len().min(remaining);
            let mut kept = matches;
            kept.truncate(take);
            summary.match_count += take;
            summary.file_count += 1;
            emitter.push(kept);
            if summary.match_count >= options.max_results {
                summary.truncated = true;
                break;
            }
        }
        if summary.truncated {
            break;
        }
    }
    if cancel.is_cancelled() {
        return Err(SearchError::Cancelled);
    }
    emitter.flush();
    Ok(summary)
}

fn search_batch(
    matcher: &RegexMatcher,
    batch: &[(&str, String)],
    max_file_bytes: u64,
    limit: usize,
    workers: usize,
    cancel: &CancellationToken,
) -> Vec<Vec<TextMatch>> {
    let slots: Vec<Mutex<Vec<TextMatch>>> = batch.iter().map(|_| Mutex::new(Vec::new())).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..workers.min(batch.len()) {
            scope.spawn(|| loop {
                if cancel.is_cancelled() {
                    return;
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some((relative_path, path)) = batch.get(i) else {
                    return;
                };
                let found = search_file(matcher, path, relative_path, max_file_bytes, limit);
                if let Some(slot) = slots.get(i) {
                    *slot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = found;
                }
            });
        }
    });
    slots
        .into_iter()
        .map(|m| {
            m.into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        })
        .collect()
}
