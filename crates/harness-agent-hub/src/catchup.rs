//! Seed and catch-up context for agents that missed turns (spec §7).

use std::fmt::Write as _;

/// History budget in characters (~12k tokens).
pub const HISTORY_BUDGET_CHARS: usize = 48_000;
/// Per-field length of a compacted (older) turn line.
pub const COMPACT_FIELD_CHARS: usize = 160;

/// One earlier turn as context.
#[derive(Debug, Clone)]
pub struct ContextTurn {
    pub seq: u32,
    /// Display name of the agent that answered.
    pub agent: String,
    pub user: String,
    pub assistant: String,
}

fn full(turn: &ContextTurn) -> String {
    format!(
        "[turn {}] user:\n{}\n[turn {}] assistant ({}):\n{}\n",
        turn.seq, turn.user, turn.seq, turn.agent, turn.assistant
    )
}

fn clip(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let mut out: String = flat.chars().take(max).collect();
        out.push('…');
        out
    }
}

fn compact(turn: &ContextTurn) -> String {
    format!(
        "[turn {}, {}] user: {} / assistant: {}\n",
        turn.seq,
        turn.agent,
        clip(&turn.user, COMPACT_FIELD_CHARS),
        clip(&turn.assistant, COMPACT_FIELD_CHARS)
    )
}

/// Renders turns within `budget`: newest verbatim, older compacted, oldest dropped with a note.
#[must_use]
pub fn render_history(turns: &[ContextTurn], budget: usize) -> String {
    let mut verbatim: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut split = turns.len();
    for (index, turn) in turns.iter().enumerate().rev() {
        let text = full(turn);
        if used + text.len() > budget {
            break;
        }
        used += text.len();
        verbatim.push(text);
        split = index;
    }
    verbatim.reverse();
    let mut compacted: Vec<String> = Vec::new();
    let mut dropped = 0usize;
    for turn in turns[..split].iter().rev() {
        let line = compact(turn);
        if used + line.len() > budget {
            dropped = turns[..split].len() - compacted.len();
            break;
        }
        used += line.len();
        compacted.push(line);
    }
    compacted.reverse();
    let mut out = String::new();
    if dropped > 0 {
        let _ = writeln!(out, "({dropped} earlier turns omitted)");
    }
    for line in compacted {
        out.push_str(&line);
    }
    for text in verbatim {
        out.push_str(&text);
    }
    out
}

/// Seed for a new native session: every earlier turn of the Harness session.
#[must_use]
pub fn seed_block(turns: &[ContextTurn]) -> Option<String> {
    if turns.is_empty() {
        return None;
    }
    Some(format!(
        "<harness-conversation kind=\"seed\">\nThis Harness conversation was started before you joined; other agents may have answered earlier turns. Conversation so far:\n{}</harness-conversation>",
        render_history(turns, HISTORY_BUDGET_CHARS)
    ))
}

/// Catch-up for an existing native session: turns answered by other agents since this agent's last turn.
#[must_use]
pub fn catchup_block(turns: &[ContextTurn]) -> Option<String> {
    if turns.is_empty() {
        return None;
    }
    Some(format!(
        "<harness-conversation kind=\"catch-up\">\nWhile you were away, other agents handled these turns of the same conversation:\n{}</harness-conversation>",
        render_history(turns, HISTORY_BUDGET_CHARS)
    ))
}

/// Labelled partial output of an agent that hit its usage limit during this turn (spec §6.4).
#[must_use]
pub fn partial_block(agent: &str, partial_text: &str, tool_titles: &[String]) -> String {
    let tools = if tool_titles.is_empty() {
        String::from("(none)")
    } else {
        tool_titles.join("; ")
    };
    format!(
        "<harness-failover from=\"{agent}\">\nPartial output from {agent} before its usage limit was reached (it did not finish this turn; continue the user's request below):\n{partial_text}\nTool calls already made by {agent}: {tools}\n</harness-failover>"
    )
}
