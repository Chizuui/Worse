//! Claim model of the `fact_check` tool (docs/specs/harness/deep-research.md, section "Fact check"): atomic
//! claim extraction with the relevance filter, verdict recording and the support score.

use std::collections::BTreeSet;

use serde_json::Value;

pub const CLAIM_MAX: usize = 40;
pub const QUERY_MAX: usize = 3;

const VERDICTS: [&str; 3] = ["supported", "notSupported", "unclear"];
const DROP_STARTS: [&str; 12] = [
    "we ",
    "our ",
    "this report",
    "this paper",
    "this section",
    "in this",
    "the method",
    "we used",
    "as described",
    "note that",
    "according to our",
    "the authors of this",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Claim {
    pub text: String,
    pub location: String,
    pub verdict: String,
    pub reason: String,
    pub sources: Vec<(String, String)>,
    pub queries: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Summary {
    pub total: usize,
    pub supported: usize,
    pub not_supported: usize,
    pub unclear: usize,
}

fn claim(text: &str, location: &str) -> Claim {
    Claim {
        text: text.trim().to_owned(),
        location: location.trim().to_owned(),
        verdict: "unclear".to_owned(),
        reason: String::new(),
        sources: Vec::new(),
        queries: Vec::new(),
    }
}

/// Sentences of the report as atomic claims, method statements and very short fragments dropped.
pub fn extract(report: &str, question: &str) -> Vec<Claim> {
    let mut out = Vec::new();
    let question_words = words(question);
    for (section, body) in sections(report) {
        for (index, sentence) in sentences(&body).into_iter().enumerate() {
            let location = format!("{section}, sentence {}", index + 1);
            if let Some(text) = usable(&sentence, &question_words) {
                out.push(claim(&text, &location));
            }
            if out.len() >= CLAIM_MAX {
                return out;
            }
        }
    }
    out
}

/// The claim text when the sentence is an atomic claim, `None` when the relevance filter drops it.
fn usable(sentence: &str, question_words: &BTreeSet<String>) -> Option<String> {
    let text = clean(sentence);
    let length = text.chars().count();
    if !(25..=600).contains(&length) {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    if DROP_STARTS.iter().any(|start| lower.starts_with(start)) {
        return None;
    }
    if lower.contains("http://") || lower.contains("https://") {
        return None;
    }
    let checkable = text.chars().any(|c| c.is_ascii_digit()) || lower.contains(" not ");
    let related = words(&lower).iter().any(|w| question_words.contains(w));
    if !checkable && !related {
        return None;
    }
    Some(text)
}

fn clean(sentence: &str) -> String {
    let mut text = sentence
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c: char| c == '-' || c == '"' || c == '\'' || c.is_whitespace())
        .trim_end_matches(['.', '!', '?'])
        .to_owned();
    text.push('.');
    text
}

fn words(text: &str) -> BTreeSet<String> {
    const STOP: [&str; 16] = [
        "the", "and", "for", "with", "that", "this", "from", "was", "were", "are", "has", "have",
        "its", "their", "what", "which",
    ];
    text.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.chars().count() > 3 && !STOP.contains(w))
        .map(str::to_owned)
        .collect()
}

/// `(section title, body)` pairs; a report without `\section` or `#` headings is one section.
fn sections(report: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut title = "Report".to_owned();
    let mut body = String::new();
    for line in report.lines() {
        let trimmed = line.trim();
        let heading = trimmed
            .strip_prefix("\\section{")
            .and_then(|rest| rest.split('}').next())
            .or_else(|| trimmed.strip_prefix("## "))
            .or_else(|| trimmed.strip_prefix("# "));
        if let Some(found) = heading {
            if !body.trim().is_empty() {
                out.push((title.clone(), std::mem::take(&mut body)));
            }
            found.trim().clone_into(&mut title);
        } else {
            body.push_str(line);
            body.push('\n');
        }
    }
    if !body.trim().is_empty() {
        out.push((title, body));
    }
    out
}

fn sentences(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\n' {
            current.push(' ');
            continue;
        }
        current.push(c);
        let ends = matches!(c, '!' | '?')
            || (c == '.' && !chars.peek().is_some_and(char::is_ascii_graphic));
        if ends {
            out.push(std::mem::take(&mut current));
        }
    }
    if current.trim().chars().count() > 20 {
        out.push(current);
    }
    out
}

/// Verdicts from the model, deduplicated by claim text and normalised.
pub fn record(value: Option<&Value>) -> Vec<Claim> {
    let mut out: Vec<Claim> = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in value.and_then(Value::as_array).into_iter().flatten() {
        let text = entry
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        let key = normalise_query(text);
        if key.is_empty() || !seen.insert(key) {
            continue;
        }
        let verdict = entry
            .get("verdict")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| VERDICTS.contains(v))
            .unwrap_or("unclear");
        let sources = entry
            .get("sources")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|source| {
                let url = source.get("url").and_then(Value::as_str)?.trim();
                if url.is_empty() {
                    return None;
                }
                let title = source
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                Some((url.to_owned(), title.trim().to_owned()))
            })
            .collect::<Vec<_>>();
        let queries = entry
            .get("queries")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .take(QUERY_MAX)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let mut claim = claim(
            text,
            entry
                .get("location")
                .and_then(Value::as_str)
                .unwrap_or("Report"),
        );
        entry
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .clone_into(&mut claim.reason);
        claim.verdict = if verdict == "supported" && sources.is_empty() {
            "unclear".to_owned()
        } else {
            verdict.to_owned()
        };
        claim.sources = sources;
        claim.queries = queries;
        out.push(claim);
        if out.len() >= CLAIM_MAX {
            break;
        }
    }
    out
}

/// Claim text as a dedupe and query-cache key: lowercase words, no punctuation, single spaces.
pub fn normalise_query(text: &str) -> String {
    text.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn score(claims: &[Claim]) -> Summary {
    let mut summary = Summary {
        total: claims.len(),
        ..Summary::default()
    };
    for claim in claims {
        match claim.verdict.as_str() {
            "supported" => summary.supported += 1,
            "notSupported" => summary.not_supported += 1,
            _ => summary.unclear += 1,
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use serde_json::json;

    const REPORT: &str = "\\section{Introduction}\nWe searched OpenAlex for studies. Harness ships a fact check \
                          step that scores 41 of 45 claims in 2026. This is a short one.\n\n\\section{Results}\n\
                          Rust builds finished 2.5 times faster than the C++ baseline in the same harness run.\n";

    #[test]
    fn extract_keeps_numbers_and_drops_method_sentences() {
        let claims = extract(REPORT, "does rust build faster than C++");
        let texts = claims.iter().map(|c| c.text.as_str()).collect::<Vec<_>>();
        assert_eq!(texts.len(), 2, "{texts:?}");
        assert!(texts[0].contains("41 of 45"));
        assert!(texts[1].contains("2.5 times faster"));
        assert!(claims.iter().all(|c| !c.text.starts_with("We ")));
        assert_eq!(claims[0].location, "Introduction, sentence 2");
        assert_eq!(claims[1].location, "Results, sentence 1");
    }

    #[test]
    fn extract_drops_claims_unrelated_to_the_question() {
        let report = "The cafeteria opened a new wing last spring. Cache misses fell by 30 percent with the new \
                      index.";
        let claims = extract(report, "how does the index change cache misses");
        assert_eq!(claims.len(), 1, "{claims:?}");
        assert!(claims[0].text.contains("30 percent"));
    }

    #[test]
    fn extract_caps_the_claim_list() {
        let line = "The measured value grew by 4 percent in every one of the sampled runs. ";
        assert_eq!(extract(&line.repeat(60), "value growth").len(), CLAIM_MAX);
    }

    #[test]
    fn record_dedupes_claims_and_requires_a_source_for_support() {
        let value = json!([
            {"text": "Cache misses fell by 30 percent.", "location": "Results", "verdict": "supported",
             "sources": [{"url": "https://example.org/a", "title": "A"}],
             "queries": ["cache misses 30 percent", "cache misses 30 percent", "cache", "extra"]},
            {"text": "cache misses fell by 30 percent", "location": "Results", "verdict": "notSupported"},
            {"text": "The sky is green today.", "verdict": "supported"},
            {"text": "No verdict here.", "verdict": "made up"}
        ]);
        let claims = record(Some(&value));
        assert_eq!(claims.len(), 3);
        assert_eq!(claims[0].queries.len(), QUERY_MAX);
        assert!(claims[1].text.starts_with("The sky is green"));
        assert_eq!(claims[1].verdict, "unclear");
        assert_eq!(claims[2].verdict, "unclear");
        let summary = score(&claims);
        assert_eq!(
            (
                summary.total,
                summary.supported,
                summary.not_supported,
                summary.unclear
            ),
            (3, 1, 0, 2)
        );
    }

    #[test]
    fn normalise_query_ignores_case_and_punctuation() {
        assert_eq!(
            normalise_query("Cache  misses, fell!"),
            normalise_query("cache misses fell")
        );
    }
}
