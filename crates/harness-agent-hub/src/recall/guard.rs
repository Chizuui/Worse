//! Keeps credentials out of the memory store. Memories are put into prompts of other agents and sessions,
//! so a key saved once would travel far. Heuristics, not proof: known token prefixes, `password = …` style
//! assignments, private key blocks, and invisible characters (which can hide text from the user).

/// Prefixes of well-known credential formats and the shortest length that counts.
const TOKEN_PREFIXES: &[(&str, usize)] = &[
    ("sk-", 20),
    ("sk_live_", 16),
    ("sk_test_", 16),
    ("ghp_", 20),
    ("gho_", 20),
    ("ghu_", 20),
    ("ghs_", 20),
    ("github_pat_", 20),
    ("glpat-", 20),
    ("xoxb-", 20),
    ("xoxp-", 20),
    ("xoxa-", 20),
    ("AKIA", 20),
    ("AIza", 30),
    ("hf_", 20),
    ("eyJ", 40),
];

const KEYWORDS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "api_key",
    "api-key",
    "apikey",
    "access_token",
    "auth_token",
    "token",
];

fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{FEFF}')
}

fn is_delimiter(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '"' | '\''
                | '`'
                | '='
                | ':'
                | ','
                | ';'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
        )
}

/// Byte ranges of `text` that look like credentials, with what they look like.
fn spans(text: &str) -> Vec<(usize, usize, &'static str)> {
    let mut out = Vec::new();
    if let (Some(start), Some(end)) = (text.find("-----BEGIN"), text.rfind("PRIVATE KEY-----")) {
        if end > start {
            out.push((
                start,
                end + "PRIVATE KEY-----".len(),
                "contains a private key",
            ));
        }
    }
    // Words that carry a known token prefix.
    let mut word_start = None;
    for (i, c) in text.char_indices().chain([(text.len(), ' ')]) {
        if is_delimiter(c) {
            if let Some(start) = word_start.take() {
                let word = &text[start..i];
                if TOKEN_PREFIXES
                    .iter()
                    .any(|(prefix, min)| word.starts_with(prefix) && word.len() >= *min)
                {
                    out.push((start, i, "contains what looks like an API key or token"));
                }
            }
        } else if word_start.is_none() {
            word_start = Some(i);
        }
    }
    // `password = hunter2xyz`, `"token": "abc123def"`.
    let lower = text.to_ascii_lowercase();
    for keyword in KEYWORDS {
        let mut from = 0;
        while let Some(found) = lower[from..].find(keyword) {
            let after = from + found + keyword.len();
            from = after;
            let rest = &text[after..];
            let trimmed = rest.trim_start_matches(['"', '\'', ' ', '\t']);
            let Some(value) = trimmed.strip_prefix([':', '=']) else {
                continue;
            };
            let value = value.trim_start_matches(['"', '\'', ' ', '\t']);
            let len = value
                .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ';'))
                .unwrap_or(value.len());
            let word = &value[..len];
            let credential_like = word.len() >= 8
                && word.chars().any(|c| c.is_ascii_digit())
                && word.chars().any(|c| c.is_ascii_alphabetic());
            if credential_like {
                let start = text.len() - value.len();
                out.push((
                    start,
                    start + len,
                    "contains what looks like a password or token",
                ));
            }
        }
    }
    out
}

/// Why `text` must not be stored, if it looks like it holds a credential or hidden characters.
#[must_use]
pub fn secret_reason(text: &str) -> Option<&'static str> {
    if text.chars().any(is_invisible) {
        return Some("contains invisible characters");
    }
    spans(text).first().map(|(_, _, reason)| *reason)
}

/// `text` with everything that looks like a credential replaced by `[redacted]`, and invisible characters
/// removed.
#[must_use]
pub fn redact(text: &str) -> String {
    let mut spans = spans(text);
    spans.sort_by_key(|(start, _, _)| *start);
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (start, end, _) in spans {
        if start < at {
            continue;
        }
        out.push_str(&text[at..start]);
        out.push_str("[redacted]");
        at = end;
    }
    out.push_str(&text[at..]);
    out.chars().filter(|c| !is_invisible(*c)).collect()
}
