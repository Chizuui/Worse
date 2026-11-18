//! The model-facing context tools (`context_compress`, `context_decompress`, `context_search`,
//! `context_status`) and the nudge that asks the model to fold when the context grows.

use std::fmt::Write as _;

use serde_json::{json, Value};

use crate::compress::{clip, role, text_of, Folds, KEEP_TAIL, VIEW_CHARS};
use crate::llm::{self, ToolCall};
use crate::Agent;

pub const NAMES: [&str; 4] = [
    "context_compress",
    "context_decompress",
    "context_search",
    "context_status",
];
/// Visible size from which the model is asked to fold (~40k tokens).
const NUDGE_FROM: usize = 160_000;
/// Growth between two nudges (~30k tokens).
const NUDGE_STEP: usize = 120_000;
const SEARCH_HITS: usize = 12;
const LISTED_RANGES: usize = 30;

pub const COMPACT_PROMPT: &str = "[Harness] The user asked to compact the context. Call context_status, then fold \
every finished range you no longer need verbatim with context_compress (one call per range, each with a detailed \
summary as the tool describes). If a session_notes_write tool is available, refresh the session notes as well. \
Then reply with a short list of what was folded.";

pub fn handles(name: &str) -> bool {
    NAMES.contains(&name)
}

fn function(name: &str, description: &str, parameters: &Value) -> Value {
    json!({"type": "function", "function": {"name": name, "description": description, "parameters": parameters}})
}

pub fn definitions() -> Vec<Value> {
    let reference = json!({"type": "string", "description": "Message ref such as m12 (see context_status)"});
    vec![
        function(
            "context_compress",
            "Fold a finished range of this conversation (messages from..to, inclusive) into your summary to free \
             context. The originals are kept: context_search finds them and context_decompress restores them. \
             Write the summary for a reader who never saw the range, possibly another model taking over this \
             session: the goal, decisions and why, files/functions with paths and line numbers, commands run and \
             their results, errors and how they were fixed, what is done and what is next. Keep exact identifiers, \
             paths and numbers. Do not fold what you still need verbatim (code you are about to edit).",
            &json!({"type": "object", "properties": {
                "from": reference,
                "to": reference,
                "summary": {"type": "string", "description": "Detailed, self-contained summary of the range"}
            }, "required": ["from", "to", "summary"]}),
        ),
        function(
            "context_decompress",
            "Restore a folded range (id such as f3) into the context with its original messages.",
            &json!({"type": "object", "properties": {
                "id": {"type": "string", "description": "Fold id such as f3"}
            }, "required": ["id"]}),
        ),
        function(
            "context_search",
            "Search every message of this conversation, folded ones included, for words; returns message refs \
             and snippets. Use it to recover a detail from a folded range without restoring it.",
            &json!({"type": "object", "properties": {
                "query": {"type": "string", "description": "Words to look for (all of them, case-insensitive)"}
            }, "required": ["query"]}),
        ),
        function(
            "context_status",
            "Show the context size, the existing folds and the ranges that can be folded, with their refs.",
            &json!({"type": "object", "properties": {}}),
        ),
    ]
}

fn parse_ref(value: Option<&Value>, prefix: char) -> Option<u64> {
    let value = value?;
    value.as_u64().or_else(|| {
        let text = value.as_str()?.trim();
        text.strip_prefix(prefix)
            .unwrap_or(text)
            .parse()
            .ok()
    })
}

fn tokens(chars: usize) -> String {
    let t = chars / 4;
    if t >= 1000 {
        format!("{:.1}k", t as f64 / 1000.0)
    } else {
        t.to_string()
    }
}

/// Runs one context tool with ACP reporting; returns the text for the `tool` message.
/// `messages` ends with the assistant message that made the call; it is never folded.
pub fn run(
    agent: &Agent,
    session: &str,
    folds: &mut Folds,
    messages: &[Value],
    call: &ToolCall,
) -> String {
    let args: Value = serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({}));
    let closed = &messages[..messages.len().saturating_sub(1)];
    let result = match call.name.as_str() {
        "context_compress" => {
            let from = parse_ref(args.get("from"), 'm');
            let to = parse_ref(args.get("to"), 'm');
            let summary = args.get("summary").and_then(Value::as_str).unwrap_or("");
            match (from, to) {
                (Some(from), Some(to)) => {
                    let before = folds.view_size(messages);
                    folds.compress(closed, from, to, summary, false).map(|id| {
                        let after = folds.view_size(messages);
                        folds.nudged_at = after;
                        format!(
                            "Folded m{from}-m{to} as f{id}: the context went from ~{} to ~{} tokens.",
                            tokens(before),
                            tokens(after)
                        )
                    })
                }
                _ => Err("from and to must be message refs such as m12".into()),
            }
        }
        "context_decompress" => match parse_ref(args.get("id"), 'f').and_then(|id| u32::try_from(id).ok()) {
            Some(id) => folds.decompress(messages, id).map(|f| {
                format!(
                    "Restored f{id}: messages m{}-m{} are back in the context.",
                    f.start,
                    f.end - 1
                )
            }),
            None => Err("id must be a fold id such as f3".into()),
        },
        "context_search" => {
            let query = args.get("query").and_then(Value::as_str).unwrap_or("");
            search(folds, messages, query)
        }
        _ => Ok(status(folds, messages)),
    };
    let (ok, text) = match result {
        Ok(text) => (true, text),
        Err(e) => (false, format!("Error: {e}")),
    };
    let title = match call.name.as_str() {
        "context_compress" => "Compress context",
        "context_decompress" => "Restore context",
        "context_search" => "Search context",
        _ => "Context status",
    };
    agent.update(
        session,
        &json!({
            "sessionUpdate": "tool_call",
            "toolCallId": format!("{}-{}", call.id, uuid::Uuid::new_v4().simple()),
            "title": title,
            "kind": "think",
            "status": if ok { "completed" } else { "failed" },
            "rawInput": args,
            "content": [{"type": "content", "content": {"type": "text", "text": clip(&text, 2000)}}]
        }),
    );
    text
}

fn snippet(text: &str, lower: &str, term: &str) -> String {
    let at = lower.find(term).map_or(0, |pos| lower[..pos].chars().count());
    let start = at.saturating_sub(120);
    let piece: String = text.chars().skip(start).take(320).collect();
    let mut out = clip(&piece, 320);
    if start > 0 {
        out.insert(0, '…');
    }
    out
}

pub fn search(folds: &Folds, messages: &[Value], query: &str) -> Result<String, String> {
    let terms: Vec<String> = query.to_lowercase().split_whitespace().map(str::to_owned).collect();
    if terms.is_empty() {
        return Err("query is empty".into());
    }
    let mut hits: Vec<(usize, usize, String)> = Vec::new();
    for (i, message) in messages.iter().enumerate() {
        let text = text_of(message);
        let lower = text.to_lowercase();
        let found: Vec<&String> = terms.iter().filter(|t| lower.contains(t.as_str())).collect();
        if found.is_empty() {
            continue;
        }
        let at = folds.base + u64::try_from(i).unwrap_or(u64::MAX);
        let place = folds
            .containing(at)
            .map(|f| format!(", folded in f{}", f.id))
            .unwrap_or_default();
        hits.push((
            found.len(),
            i,
            format!("m{at} ({}{place}): {}", role(message), snippet(&text, &lower, found[0])),
        ));
    }
    for fold in &folds.folds {
        let lower = fold.summary.to_lowercase();
        let found = terms.iter().filter(|t| lower.contains(t.as_str())).count();
        if found > 0 {
            let first = terms.iter().find(|t| lower.contains(t.as_str())).map_or("", |t| t.as_str());
            hits.push((
                found,
                usize::MAX,
                format!("f{} summary: {}", fold.id, snippet(&fold.summary, &lower, first)),
            ));
        }
    }
    if hits.is_empty() {
        return Ok(format!("No message contains \"{query}\"."));
    }
    let best = hits.iter().map(|h| h.0).max().unwrap_or(0);
    hits.retain(|h| h.0 == best);
    hits.sort_by(|a, b| b.1.cmp(&a.1));
    let total = hits.len();
    let mut out = String::new();
    for hit in hits.iter().take(SEARCH_HITS) {
        let _ = writeln!(out, "{}", hit.2);
    }
    if total > SEARCH_HITS {
        let _ = writeln!(out, "… {} more matches; refine the query", total - SEARCH_HITS);
    }
    Ok(out)
}

fn range_lines(folds: &Folds, messages: &[Value], limit: usize) -> (Vec<String>, usize) {
    let ranges = folds.ranges(messages, limit);
    let total = ranges.len();
    let lines = ranges
        .into_iter()
        .filter(|&(_, end)| end < messages.len())
        .take(LISTED_RANGES)
        .map(|(start, end)| {
            let size: usize = messages[start..end].iter().map(llm::message_size).sum();
            let first = &messages[start];
            format!(
                "m{}-m{} ~{} tokens, {} messages: {} \"{}\"",
                folds.base + u64::try_from(start).unwrap_or(0),
                folds.base + u64::try_from(end).unwrap_or(0) - 1,
                tokens(size),
                end - start,
                role(first),
                clip(&text_of(first), 80)
            )
        })
        .collect();
    (lines, total)
}

pub fn status(folds: &Folds, messages: &[Value]) -> String {
    let view = folds.view_size(messages);
    let mut out = format!(
        "Context: ~{} tokens visible of a ~{} token budget; messages m{}-m{}.\n",
        tokens(view),
        tokens(VIEW_CHARS),
        folds.base,
        (folds.base + u64::try_from(messages.len()).unwrap_or(0)).saturating_sub(1)
    );
    if folds.folds.is_empty() {
        out.push_str("Folds: none\n");
    } else {
        out.push_str("Folds:\n");
        for fold in &folds.folds {
            let _ = writeln!(
                out,
                "  f{} m{}-m{}{}: {}",
                fold.id,
                fold.start,
                fold.end - 1,
                if fold.auto { " (automatic)" } else { "" },
                clip(&fold.summary, 100)
            );
        }
    }
    let (lines, total) = range_lines(folds, messages, messages.len().saturating_sub(1));
    out.push_str("Compressible ranges (oldest first):\n");
    for line in &lines {
        let _ = writeln!(out, "  {line}");
    }
    if total > lines.len() {
        let _ = writeln!(out, "  … {} more", total - lines.len());
    }
    out.push_str(
        "A range may span several listed ranges. `from` must not be a tool result, and the message after `to` \
         must not be one either (a call and its results fold together).",
    );
    out
}

/// A note asking the model to fold, when the visible context has grown enough since the last one.
pub fn nudge(folds: &mut Folds, messages: &[Value]) -> Option<String> {
    let size = folds.view_size(messages);
    if size < NUDGE_FROM || size < folds.nudged_at + NUDGE_STEP {
        return None;
    }
    folds.nudged_at = size;
    let (lines, _) = range_lines(folds, messages, messages.len().saturating_sub(KEEP_TAIL));
    if lines.is_empty() {
        return None;
    }
    let mut note = format!(
        "\n\n[Harness context] This conversation now takes ~{} tokens of context. Fold finished work you no \
         longer need verbatim with context_compress (originals stay retrievable with context_search and \
         context_decompress), then continue the task. Oldest compressible ranges:\n",
        tokens(size)
    );
    for line in lines.iter().take(8) {
        let _ = writeln!(note, "  {line}");
    }
    note.push_str(
        "If a session_notes_write tool is available, refresh the session notes too so another model can \
         continue this session.",
    );
    Some(note)
}

/// Appends `note` to a message's content (plain text or content parts).
pub fn append_note(message: &mut Value, note: &str) {
    match message.get_mut("content") {
        Some(Value::String(text)) => text.push_str(note),
        Some(Value::Array(parts)) => parts.push(json!({"type": "text", "text": note.trim_start()})),
        _ => message["content"] = json!(note.trim_start()),
    }
}
