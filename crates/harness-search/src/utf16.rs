//! UTF-16 helpers so that offsets and scores match JS string semantics.

/// JS `String.prototype.trim` whitespace: `WhiteSpace` + `LineTerminator`.
/// Unlike `char::is_whitespace`, it includes U+FEFF and excludes U+0085.
#[must_use]
pub fn is_js_whitespace(c: char) -> bool {
    c == '\u{feff}' || (c.is_whitespace() && c != '\u{85}')
}

/// JS `s.trim()`.
#[must_use]
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

/// JS `s.trim().toLowerCase()` as UTF-16 code units.
#[must_use]
pub fn normalized_utf16(s: &str) -> Vec<u16> {
    js_trim(s).to_lowercase().encode_utf16().collect()
}

/// JS `haystack.indexOf(needle, from)` on code units.
#[must_use]
pub fn index_of(haystack: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(haystack.len()));
    }
    if needle.len() > haystack.len() || from > haystack.len() - needle.len() {
        return None;
    }
    let first = needle[0];
    let last_start = haystack.len() - needle.len();
    let mut i = from;
    while i <= last_start {
        if haystack[i] == first && haystack[i..i + needle.len()] == *needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// JS `s.slice(start, end)` on code units, decoded lossily: a surrogate half
/// cut by the slice becomes U+FFFD (what the TS RPC string encoder emits).
#[must_use]
pub fn slice_lossy(units: &[u16], start: usize, end: usize) -> String {
    let end = end.min(units.len());
    if start >= end {
        return String::new();
    }
    String::from_utf16_lossy(&units[start..end])
}

/// Truncates `s` to at most `max` UTF-16 code units (JS `s.slice(0, max)`).
#[must_use]
pub fn truncate_utf16(s: &str, max: usize) -> String {
    let mut units = 0usize;
    for (byte_index, c) in s.char_indices() {
        let width = c.len_utf16();
        if units + width > max {
            if units < max {
                // JS 会在代理对中间截断，留下孤立代理项；RPC 编码后为 U+FFFD。
                let mut out = s[..byte_index].to_owned();
                out.push('\u{fffd}');
                return out;
            }
            return s[..byte_index].to_owned();
        }
        units += width;
    }
    s.to_owned()
}

/// Number of UTF-16 code units in `s`.
#[must_use]
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}
