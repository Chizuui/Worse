//! Subsequence fuzzy match for the command center and the composer popups (case-insensitive).

/// Score of `query` against `text`, `None` when the query is not a subsequence. An empty query matches with 0.
/// Bonuses: contiguous runs, matches at word starts (after `/ _ - . space` or a lower→upper change), a prefix hit.
pub fn score(query: &str, text: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let q: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    let t: Vec<char> = text.chars().collect();
    let mut qi = 0;
    let mut total = 0;
    let mut run = 0;
    let mut prev: Option<char> = None;
    for (i, &c) in t.iter().enumerate() {
        if qi == q.len() {
            break;
        }
        let lower = c.to_lowercase().next().unwrap_or(c);
        if lower == q[qi] {
            let word_start = prev.map_or(true, |p| {
                matches!(p, '/' | '\\' | '_' | '-' | '.' | ' ')
                    || (p.is_lowercase() && c.is_uppercase())
            });
            run += 1;
            total += 1 + run * 2 + if word_start { 6 } else { 0 } + if i == 0 { 4 } else { 0 };
            qi += 1;
        } else {
            run = 0;
        }
        prev = Some(c);
    }
    if qi < q.len() {
        return None;
    }
    // 短文本优先：同样的命中，越短越相关。
    let len = i32::try_from(t.len()).unwrap_or(i32::MAX);
    Some(total * 4 - len.min(200) / 4)
}
