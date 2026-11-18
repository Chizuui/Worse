//! Search without an embedding model: BM25 over words, fused with recency.
//!
//! Words are lower-cased runs of letters and digits; `camelCase` identifiers also count by their parts, so
//! "pairing" finds `TokenPairing`. The lexical order and the newest-first order of the matches are fused by
//! reciprocal rank (k = 60, as memvid does) instead of adding raw scores of different scales, with the
//! lexical list weighted double: recency breaks ties, it does not outvote relevance.

use std::collections::{HashMap, HashSet};

use super::{Kind, Memory};

const K1: f64 = 1.2;
const B: f64 = 0.75;
const RRF_K: f64 = 60.0;
const RECENCY_WEIGHT: f64 = 0.5;

/// Words too common to tell memories apart (English and Indonesian).
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "that", "this", "with", "from", "are", "was", "were", "not", "but", "you",
    "your", "have", "has", "had", "can", "will", "would", "should", "could", "what", "when",
    "where", "which", "how", "why", "who", "into", "out", "its", "it", "is", "in", "on", "of",
    "to", "a", "an", "as", "at", "be", "by", "do", "if", "or", "so", "we", "me", "my", "no", "up",
    "use", "all", "any", "please", "then", "than", "yang", "dan", "di", "ke", "dari", "ini", "itu",
    "untuk", "dengan", "ada", "aja", "saja", "juga", "atau", "tidak", "ga", "gak", "nggak", "bisa",
    "biar", "jadi", "kalau", "kalo", "sama", "buat", "lagi", "udah", "sudah", "dah", "nya", "gua",
    "gue", "lu", "lo", "kita", "apa", "gimana", "karena", "tapi", "mau", "dong", "sih", "deh",
    "tolong",
];

fn keep(word: &str) -> bool {
    word.chars().count() >= 2 && !STOPWORDS.contains(&word)
}

/// A trailing plural `s` does not make a different word ("tokens" = "token").
fn stem(word: String) -> String {
    if word.len() > 3 && word.ends_with('s') && !word.ends_with("ss") && word.is_ascii() {
        word[..word.len() - 1].to_owned()
    } else {
        word
    }
}

/// Search words of `text`, in order, with repeats.
#[must_use]
pub fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut Vec<String>| {
        if word.is_empty() {
            return;
        }
        // camelCase / PascalCase parts, next to the whole identifier.
        let chars: Vec<char> = word.chars().collect();
        let mut parts: Vec<String> = Vec::new();
        let mut part = String::new();
        for (i, c) in chars.iter().enumerate() {
            let boundary = i > 0
                && c.is_uppercase()
                && (chars[i - 1].is_lowercase()
                    || chars.get(i + 1).is_some_and(|next| next.is_lowercase()));
            if boundary && !part.is_empty() {
                parts.push(std::mem::take(&mut part));
            }
            part.extend(c.to_lowercase());
        }
        parts.push(part);
        let whole = word.to_lowercase();
        if parts.len() > 1 {
            for part in parts {
                if keep(&part) {
                    out.push(stem(part));
                }
            }
        }
        if keep(&whole) {
            out.push(stem(whole));
        }
        word.clear();
    };
    for c in text.chars() {
        if c.is_alphanumeric() {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// A search.
#[derive(Debug, Clone)]
pub struct Query<'a> {
    pub text: &'a str,
    pub limit: usize,
    /// How many different query words a memory must contain. Automatic recall asks for two when the request
    /// has enough words, so one shared common word does not pull a memory in.
    pub need: usize,
    /// Only this kind.
    pub kind: Option<Kind>,
}

/// One result.
#[derive(Debug, Clone)]
pub struct Hit<'a> {
    pub memory: &'a Memory,
    /// Fused score; only its order means something.
    pub score: f64,
    /// Different query words found in the memory.
    pub matched: usize,
}

fn searchable(memory: &Memory) -> Vec<String> {
    let mut words = tokens(&memory.text);
    if let Some(title) = &memory.title {
        words.extend(tokens(title));
    }
    words
}

/// The memories that match `query`, best first.
#[must_use]
pub fn search<'a>(memories: &[&'a Memory], query: &Query<'_>) -> Vec<Hit<'a>> {
    let wanted: Vec<String> = {
        let mut seen = HashSet::new();
        tokens(query.text)
            .into_iter()
            .filter(|w| seen.insert(w.clone()))
            .collect()
    };
    if wanted.is_empty() || query.limit == 0 {
        return Vec::new();
    }
    let docs: Vec<(&'a Memory, Vec<String>)> = memories
        .iter()
        .filter(|m| query.kind.map_or(true, |k| m.kind == k))
        .map(|m| (*m, searchable(m)))
        .collect();
    if docs.is_empty() {
        return Vec::new();
    }
    #[allow(clippy::cast_precision_loss)]
    let count = docs.len() as f64;
    #[allow(clippy::cast_precision_loss)]
    let average = docs.iter().map(|(_, words)| words.len()).sum::<usize>() as f64 / count;
    let mut frequency: HashMap<&str, f64> = HashMap::new();
    for word in &wanted {
        let containing = docs
            .iter()
            .filter(|(_, words)| words.iter().any(|w| w == word))
            .count();
        #[allow(clippy::cast_precision_loss)]
        frequency.insert(word.as_str(), containing as f64);
    }
    let need = query.need.clamp(1, wanted.len());
    let mut lexical: Vec<(usize, f64, usize)> = Vec::new();
    for (index, (_, words)) in docs.iter().enumerate() {
        let mut score = 0.0;
        let mut matched = 0;
        #[allow(clippy::cast_precision_loss)]
        let length = words.len() as f64;
        for word in &wanted {
            #[allow(clippy::cast_precision_loss)]
            let tf = words.iter().filter(|w| *w == word).count() as f64;
            if tf == 0.0 {
                continue;
            }
            matched += 1;
            let df = frequency.get(word.as_str()).copied().unwrap_or(0.0);
            let idf = (1.0 + (count - df + 0.5) / (df + 0.5)).ln();
            score += idf * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * length / average.max(1.0)));
        }
        if matched >= need {
            lexical.push((index, score, matched));
        }
    }
    // Equal lexical scores: the newer memory first.
    lexical.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then_with(|| docs[b.0].0.at.cmp(&docs[a.0].0.at))
    });
    let mut newest: Vec<usize> = lexical.iter().map(|(index, _, _)| *index).collect();
    newest.sort_by_key(|index| std::cmp::Reverse(docs[*index].0.at));
    let mut hits: Vec<Hit<'a>> = lexical
        .iter()
        .enumerate()
        .map(|(rank, (index, _, matched))| {
            let recency = newest.iter().position(|i| i == index).unwrap_or(rank);
            #[allow(clippy::cast_precision_loss)]
            let score =
                1.0 / (RRF_K + rank as f64 + 1.0) + RECENCY_WEIGHT / (RRF_K + recency as f64 + 1.0);
            Hit {
                memory: docs[*index].0,
                score,
                matched: *matched,
            }
        })
        .collect();
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    hits.truncate(query.limit);
    hits
}
