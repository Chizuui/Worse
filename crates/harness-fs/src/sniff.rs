//! Binary-content heuristic shared with the TS `fileService.ts`.

/// Returns `true` when `buffer` looks binary: any NUL byte, or more than 30%
/// control characters other than TAB, LF, FF and CR. Identical to the TS
/// `isProbablyBinary` so text/binary decisions match the fallback.
#[must_use]
pub fn is_probably_binary(buffer: &[u8]) -> bool {
    if buffer.is_empty() {
        return false;
    }
    let mut suspicious: usize = 0;
    for &value in buffer {
        if value == 0 {
            return true;
        }
        let common_whitespace = matches!(value, 9 | 10 | 12 | 13);
        let control = matches!(value, 1..=8 | 14..=31 | 127);
        if !common_whitespace && control {
            suspicious += 1;
        }
    }
    // suspicious / len > 0.3  <=>  10 * suspicious > 3 * len（整数比较避免浮点误差）
    suspicious.saturating_mul(10) > buffer.len().saturating_mul(3)
}
