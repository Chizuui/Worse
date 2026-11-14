//! Literal port of `packages/shared/src/workspaceFileSearch.ts`.
//!
//! Scores are computed on UTF-16 code units (JS `indexOf`/`startsWith`
//! semantics) so that rankings and tie-breaks are identical to the TS
//! implementation, including the stable tie-break by original index.

use std::cmp::Ordering;

use crate::codec;
use crate::index::{EntryType, IndexEntry, WorkspaceFileEntry, WorkspaceIndex};
use crate::utf16::{index_of, js_trim, normalized_utf16};

/// TS `WORKSPACE_FILE_SEARCH_DISPLAY_CAP`.
pub const WORKSPACE_FILE_SEARCH_DISPLAY_CAP: usize = 1000;

/// TS `WorkspaceFileSearchCandidate` (precomputed lowercase forms).
#[derive(Debug, Clone)]
pub struct Candidate {
    name: String,
    path: String,
    relative_path: String,
    kind: EntryType,
    lc_name: Vec<u16>,
    lc_relative_path: Vec<u16>,
    lc_path: Vec<u16>,
}

impl Candidate {
    pub(crate) fn new(entry: &IndexEntry, prefix: &str, separator: char) -> Self {
        let name = entry
            .relative_path
            .rsplit_once('/')
            .map_or(entry.relative_path.as_str(), |(_, n)| n)
            .to_owned();
        let path = codec::join_relative(prefix, separator, &entry.relative_path);
        Self {
            lc_name: normalized_utf16(&name),
            lc_relative_path: normalized_utf16(&entry.relative_path),
            lc_path: normalized_utf16(&path),
            name,
            path,
            relative_path: entry.relative_path.clone(),
            kind: entry.kind,
        }
    }

    fn to_entry(&self) -> WorkspaceFileEntry {
        WorkspaceFileEntry {
            name: self.name.clone(),
            path: self.path.clone(),
            relative_path: self.relative_path.clone(),
            kind: self.kind,
        }
    }
}

/// `scoreNormalizedFuzzyMatch` on normalized UTF-16 input.
fn score_normalized(text: &[u16], query: &[u16], query_chars: &[Vec<u16>]) -> Option<i64> {
    if text.is_empty() {
        return None;
    }
    let len_diff = len_i64(text.len()) - len_i64(query.len());
    if text.starts_with(query) {
        return Some(len_diff);
    }
    if let Some(index) = index_of(text, query, 0) {
        return Some(100 + len_i64(index));
    }
    let mut score: i64 = 200;
    let mut search_start = 0usize;
    for ch in query_chars {
        let found = index_of(text, ch, search_start)?;
        score += len_i64(found) - len_i64(search_start);
        search_start = found + 1;
    }
    Some(score + len_diff)
}

fn len_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// `getWorkspaceFileSearchCandidateScore`.
fn candidate_score(candidate: &Candidate, query: &[u16], query_chars: &[Vec<u16>]) -> Option<i64> {
    let name = score_normalized(&candidate.lc_name, query, query_chars);
    let relative = score_normalized(&candidate.lc_relative_path, query, query_chars);
    let path = score_normalized(&candidate.lc_path, query, query_chars);
    let keyword = [relative.map(|s| s + 300), path.map(|s| s + 300)]
        .into_iter()
        .flatten()
        .min();
    [name, relative.map(|s| s + 25), keyword]
        .into_iter()
        .flatten()
        .min()
}

struct Scored {
    index: usize,
    score: i64,
}

fn compare(a: &Scored, b: &Scored) -> Ordering {
    // (score, index) 严格全序；TS 的 name localeCompare 兜底分支在 index 唯一时不可达。
    a.score.cmp(&b.score).then(a.index.cmp(&b.index))
}

/// `filterWorkspaceFileSearchCandidates(candidates, query, { limit })`
/// followed by the `{name, path, relativePath, type}` projection.
fn filter_candidates(
    candidates: &[Candidate],
    query: &str,
    limit: usize,
) -> Vec<WorkspaceFileEntry> {
    let trimmed = js_trim(query);
    if trimmed.is_empty() {
        // 无 query：文件优先（priority 0）、目录在后，组内保持原序。
        let files = candidates.iter().filter(|c| c.kind == EntryType::File);
        let dirs = candidates.iter().filter(|c| c.kind == EntryType::Directory);
        return files
            .chain(dirs)
            .take(limit)
            .map(Candidate::to_entry)
            .collect();
    }
    let lowered = trimmed.to_lowercase();
    let normalized: Vec<u16> = lowered.encode_utf16().collect();
    let query_chars: Vec<Vec<u16>> = lowered
        .chars()
        .map(|c| c.encode_utf16(&mut [0; 2]).to_vec())
        .collect();
    let mut best: Vec<Scored> = Vec::new();
    for (index, candidate) in candidates.iter().enumerate() {
        let Some(score) = candidate_score(candidate, &normalized, &query_chars) else {
            continue;
        };
        let scored = Scored { index, score };
        if best.len() >= limit {
            match best.last() {
                Some(worst) if compare(&scored, worst) != Ordering::Less => continue,
                None => continue,
                _ => {}
            }
        }
        let at = best.partition_point(|item| compare(item, &scored) != Ordering::Greater);
        best.insert(at, scored);
        if best.len() > limit {
            best.pop();
        }
    }
    best.iter()
        .map(|s| candidates[s.index].to_entry())
        .collect()
}

/// `searchWorkspaceFiles` ranking over a built index. `limit` must already be
/// clamped by the caller (`min(1000, max(0, trunc(limit ?? 1000)))`).
#[must_use]
pub fn search_entries(
    index: &WorkspaceIndex,
    root_path: &str,
    query: &str,
    limit: usize,
) -> Vec<WorkspaceFileEntry> {
    if limit == 0 {
        return Vec::new();
    }
    filter_candidates(index.candidates(root_path), query, limit)
}
