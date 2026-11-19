//! Reversible context folding for the API agent, after the design of billion-context
//! (<https://github.com/ranxianglei/billion-context>, MIT). The model folds finished message ranges into its
//! own summaries; the original messages stay in the session file, where `context_search` finds them and
//! `context_decompress` restores them. Folds are addressed by absolute message refs (`m12`) that are never
//! reused, so earlier requests keep a stable prefix for prompt caching.

use std::fmt::Write as _;

use serde_json::{json, Value};

use crate::llm;

/// Budget of the context sent to the model, in characters (~100k tokens).
pub const VIEW_CHARS: usize = 400_000;
/// Originals kept on disk before the oldest turns are dropped for good.
const ARCHIVE_CHARS: usize = 8_000_000;
/// Newest messages never folded automatically.
pub const KEEP_TAIL: usize = 8;
/// A listed compressible range is split at an assistant step once it holds this much.
const RANGE_SPLIT_CHARS: usize = 30_000;
const AUTO_SUMMARY_CHARS: usize = 6_000;
const DIGEST_CHARS: usize = 3_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fold {
    pub id: u32,
    /// First folded message (absolute ref).
    pub start: u64,
    /// One past the last folded message (absolute ref).
    pub end: u64,
    pub summary: String,
    /// Written by Harness when the context overflowed, not by the model.
    pub auto: bool,
}

#[derive(Debug, Default)]
#[allow(clippy::struct_field_names)]
pub struct Folds {
    /// Absolute ref of `messages[0]` (messages dropped from the front so far).
    pub base: u64,
    pub folds: Vec<Fold>,
    next_id: u32,
    /// Visible size when the model was last asked to fold.
    pub nudged_at: usize,
}

pub fn role(message: &Value) -> &str {
    message.get("role").and_then(Value::as_str).unwrap_or("")
}

/// Text of a message including the tool calls it makes.
pub fn text_of(message: &Value) -> String {
    let mut text = llm::content_text(message.get("content")).unwrap_or_default();
    for call in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let function = &call["function"];
        let _ = write!(
            text,
            "\n{}({})",
            function["name"].as_str().unwrap_or_default(),
            function["arguments"].as_str().unwrap_or_default()
        );
    }
    text
}

pub fn clip(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max).collect();
    out.push('…');
    out
}

fn abs(i: usize) -> u64 {
    u64::try_from(i).unwrap_or(u64::MAX)
}

impl Folds {
    pub fn to_json(&self) -> Value {
        json!({
            "base": self.base,
            "nextId": self.next_id,
            "nudgedAt": self.nudged_at,
            "folds": self.folds.iter().map(|f| json!({
                "id": f.id, "start": f.start, "end": f.end, "summary": f.summary, "auto": f.auto
            })).collect::<Vec<_>>()
        })
    }

    pub fn from_json(value: Option<&Value>) -> Self {
        let Some(value) = value else {
            return Self::default();
        };
        let num = |v: &Value, key: &str| v.get(key).and_then(Value::as_u64).unwrap_or(0);
        let folds = value
            .get("folds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|f| {
                Some(Fold {
                    id: u32::try_from(num(f, "id")).ok()?,
                    start: num(f, "start"),
                    end: num(f, "end"),
                    summary: f.get("summary").and_then(Value::as_str)?.to_owned(),
                    auto: f.get("auto").and_then(Value::as_bool).unwrap_or(false),
                })
            })
            .filter(|f| f.end > f.start)
            .collect();
        Self {
            base: num(value, "base"),
            folds,
            next_id: u32::try_from(num(value, "nextId")).unwrap_or(0),
            nudged_at: usize::try_from(num(value, "nudgedAt")).unwrap_or(0),
        }
    }

    fn index(&self, at: u64, len: usize) -> usize {
        at.checked_sub(self.base)
            .and_then(|i| usize::try_from(i).ok())
            .map_or(len, |i| i.min(len))
    }

    pub fn containing(&self, at: u64) -> Option<&Fold> {
        self.folds.iter().find(|f| f.start <= at && at < f.end)
    }

    pub fn carrier(fold: &Fold) -> Value {
        json!({"role": "user", "content": format!(
            "[Harness folded context f{id}: messages m{}-m{} were compressed into this summary; \
             context_decompress {{\"id\":\"f{id}\"}} restores them]\n{}",
            fold.start,
            fold.end - 1,
            fold.summary,
            id = fold.id
        )})
    }

    /// Calls `visit` with each visible item: an unfolded message index or a fold.
    fn walk<'a>(&'a self, messages: &[Value], mut visit: impl FnMut(Result<usize, &'a Fold>)) {
        let mut i = 0;
        while i < messages.len() {
            let at = self.base + abs(i);
            if let Some(fold) = self.folds.iter().find(|f| f.start == at) {
                visit(Err(fold));
                i = self.index(fold.end, messages.len()).max(i + 1);
            } else {
                visit(Ok(i));
                i += 1;
            }
        }
    }

    /// The history as the model sees it: each fold replaced by its summary.
    pub fn view(&self, messages: &[Value]) -> Vec<Value> {
        let mut out = Vec::with_capacity(messages.len());
        self.walk(messages, |item| match item {
            Ok(i) => out.push(messages[i].clone()),
            Err(fold) => out.push(Self::carrier(fold)),
        });
        out
    }

    pub fn view_size(&self, messages: &[Value]) -> usize {
        let mut size = 0;
        self.walk(messages, |item| {
            size += match item {
                Ok(i) => llm::message_size(&messages[i]),
                Err(fold) => llm::message_size(&Self::carrier(fold)),
            };
        });
        size
    }

    /// Folds `from..=to` (absolute refs) into `summary`; folds inside the range are merged into it.
    pub fn compress(
        &mut self,
        messages: &[Value],
        from: u64,
        to: u64,
        summary: &str,
        auto: bool,
    ) -> Result<u32, String> {
        let summary = summary.trim();
        if summary.is_empty() {
            return Err("summary is empty".into());
        }
        let last = self.base + abs(messages.len());
        if messages.is_empty() || from > to || from < self.base || to >= last {
            return Err(format!(
                "foldable messages are m{}-m{} (from <= to; the newest message cannot be folded)",
                self.base,
                last.saturating_sub(1)
            ));
        }
        let start = self.index(from, messages.len());
        let end = self.index(to + 1, messages.len());
        if role(&messages[start]) == "tool" {
            return Err(format!(
                "m{from} is a tool result: start at the assistant message that made the call, or at a user message"
            ));
        }
        if end < messages.len() && role(&messages[end]) == "tool" {
            return Err(format!(
                "m{} is a tool result of a call inside the range: end the range after the last result of that step",
                to + 1
            ));
        }
        if let Some(cut) = self
            .folds
            .iter()
            .find(|f| f.end > from && f.start <= to && !(f.start >= from && f.end <= to + 1))
        {
            return Err(format!(
                "the range cuts fold f{} (m{}-m{}): include all of it or none of it",
                cut.id,
                cut.start,
                cut.end - 1
            ));
        }
        self.folds.retain(|f| !(f.start >= from && f.end <= to + 1));
        self.next_id = self.next_id.max(1);
        let id = self.next_id;
        self.next_id += 1;
        self.folds.push(Fold {
            id,
            start: from,
            end: to + 1,
            summary: summary.to_owned(),
            auto,
        });
        self.folds.sort_by_key(|f| f.start);
        Ok(id)
    }

    /// Restores fold `id` unless that would overflow the context budget.
    pub fn decompress(&mut self, messages: &[Value], id: u32) -> Result<Fold, String> {
        let pos = self
            .folds
            .iter()
            .position(|f| f.id == id)
            .ok_or_else(|| format!("there is no fold f{id}; context_status lists the folds"))?;
        let fold = self.folds.remove(pos);
        if self.view_size(messages) > VIEW_CHARS {
            self.folds.insert(pos, fold);
            return Err(format!(
                "restoring f{id} would overflow the context budget: use context_search for the detail you need, \
                 or fold other ranges first"
            ));
        }
        Ok(fold)
    }

    /// Unfolded ranges `(start, end)` (indices, end exclusive) before index `limit`, split at user messages and
    /// at assistant steps once a range holds `RANGE_SPLIT_CHARS`. Every range is a valid `compress` argument.
    pub fn ranges(&self, messages: &[Value], limit: usize) -> Vec<(usize, usize)> {
        let limit = limit.min(messages.len());
        let mut out = Vec::new();
        let mut i = 0;
        while i < limit {
            if let Some(fold) = self.containing(self.base + abs(i)) {
                i = self.index(fold.end, messages.len()).max(i + 1);
                continue;
            }
            if role(&messages[i]) == "tool" {
                i += 1;
                continue;
            }
            let mut end = i + 1;
            let mut size = llm::message_size(&messages[i]);
            while end < limit && self.containing(self.base + abs(end)).is_none() {
                let r = role(&messages[end]);
                if r == "user" || (r == "assistant" && size >= RANGE_SPLIT_CHARS) {
                    break;
                }
                size += llm::message_size(&messages[end]);
                end += 1;
            }
            while end < messages.len() && role(&messages[end]) == "tool" {
                end += 1;
            }
            out.push((i, end));
            i = end;
        }
        out
    }

    /// Makes the history fit: folds the oldest ranges with a mechanical digest while the view is over budget,
    /// then drops whole oldest turns while the view or the archive is still too large.
    pub fn fit(&mut self, messages: &mut Vec<Value>) {
        while self.view_size(messages) > VIEW_CHARS {
            let limit = messages.len().saturating_sub(KEEP_TAIL);
            let Some(&(start, end)) = self.ranges(messages, limit).first() else {
                break;
            };
            if end >= messages.len() {
                break;
            }
            let summary = digest(&messages[start..end]);
            let from = self.base + abs(start);
            let to = self.base + abs(end) - 1;
            if let Some(prev) = self.folds.iter_mut().find(|f| f.end == from && f.auto) {
                prev.end = to + 1;
                prev.summary = join_capped(&prev.summary, &summary);
            } else if self.compress(messages, from, to, &summary, true).is_err() {
                break;
            }
        }
        let total = |m: &[Value]| m.iter().map(llm::message_size).sum::<usize>();
        while self.view_size(messages) > VIEW_CHARS || total(messages) > ARCHIVE_CHARS {
            let base = self.base;
            let Some(cut) = (1..messages.len()).find(|&i| {
                let at = base + abs(i);
                role(&messages[i]) == "user"
                    && !self.folds.iter().any(|f| f.start < at && at < f.end)
            }) else {
                break;
            };
            messages.drain(..cut);
            self.base += abs(cut);
            let base = self.base;
            self.folds.retain(|f| f.start >= base);
        }
    }
}

fn join_capped(old: &str, new: &str) -> String {
    let joined = format!("{old}\n{new}");
    let count = joined.chars().count();
    if count <= AUTO_SUMMARY_CHARS {
        return joined;
    }
    let tail: String = joined.chars().skip(count - AUTO_SUMMARY_CHARS).collect();
    format!("(older digest lines dropped; context_search still finds them)\n{tail}")
}

/// Mechanical summary of a range, used when the context overflowed before the model folded it.
pub fn digest(messages: &[Value]) -> String {
    let mut out = String::from(
        "(automatic digest written by Harness when the context filled up; context_search or \
         context_decompress recover the full messages)",
    );
    let mut omitted = 0usize;
    for message in messages {
        let content = llm::content_text(message.get("content")).unwrap_or_default();
        let mut lines = Vec::new();
        match role(message) {
            "user" => lines.push(format!("User: {}", clip(&content, 400))),
            "assistant" => {
                if !content.trim().is_empty() {
                    lines.push(format!("Assistant: {}", clip(&content, 400)));
                }
                for call in message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    lines.push(format!(
                        "  called {}({})",
                        call["function"]["name"].as_str().unwrap_or_default(),
                        clip(
                            call["function"]["arguments"].as_str().unwrap_or_default(),
                            160
                        )
                    ));
                }
            }
            "tool" => lines.push(format!("    result: {}", clip(&content, 200))),
            _ => {}
        }
        for line in lines {
            if out.len() + line.len() + 1 > DIGEST_CHARS {
                omitted += 1;
            } else {
                out.push('\n');
                out.push_str(&line);
            }
        }
    }
    if omitted > 0 {
        let _ = write!(out, "\n… {omitted} more lines omitted");
    }
    out
}

#[cfg(test)]
#[path = "compress_tests.rs"]
mod tests;
