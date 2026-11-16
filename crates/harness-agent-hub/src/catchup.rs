//! Seed and catch-up context for agents that missed turns (spec §7).

use std::fmt::Write as _;

/// History budget in characters (~30k tokens).
pub const HISTORY_BUDGET_CHARS: usize = 120_000;
/// Per-field length of a compacted (older) turn line.
pub const COMPACT_FIELD_CHARS: usize = 160;
/// Output of one tool call in a failover or verbatim turn block.
pub const TOOL_OUTPUT_CHARS: usize = 1_500;
/// Input (arguments) of one tool call: flattened to one line.
pub const TOOL_INPUT_CHARS: usize = 300;
/// Total budget of one tool digest list (failover block, or all tools of a turn).
pub const TOOL_BLOCK_BUDGET_CHARS: usize = 24_000;
/// Total budget of the `tools:` section of one verbatim turn.
pub const TURN_TOOL_BUDGET_CHARS: usize = 4_000;
/// Output of one tool call inside a verbatim turn.
pub const TURN_TOOL_OUTPUT_CHARS: usize = 400;

/// What an interrupted (or previous) agent already ran or read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolDigest {
    /// Display title of the call (`Read src/lib.rs`).
    pub title: String,
    /// Arguments as text (JSON or plain).
    pub input: String,
    /// Output the agent already saw.
    pub output: String,
}

/// One earlier turn as context.
#[derive(Debug, Clone, Default)]
pub struct ContextTurn {
    pub seq: u32,
    /// Display name of the agent that answered.
    pub agent: String,
    pub user: String,
    pub assistant: String,
    /// Tool calls of the turn (digested).
    pub tools: Vec<ToolDigest>,
}

fn full(turn: &ContextTurn) -> String {
    let mut out = format!(
        "[turn {}] user:\n{}\n[turn {}] assistant ({}):\n{}\n",
        turn.seq, turn.user, turn.seq, turn.agent, turn.assistant
    );
    if !turn.tools.is_empty() {
        let _ = write!(
            out,
            "[turn {}] tools:\n{}",
            turn.seq,
            render_tools(&turn.tools, TURN_TOOL_BUDGET_CHARS, TURN_TOOL_OUTPUT_CHARS)
        );
    }
    out
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

/// Clips `text` to `max` characters, keeping its head and its tail.
#[must_use]
pub fn clip_head_tail(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    let head_len = max / 2;
    let tail_len = max - head_len;
    let head: String = text.chars().take(head_len).collect();
    let tail: String = text.chars().skip(count - tail_len).collect();
    let omitted = count - head_len - tail_len;
    format!("{head}\n… ({omitted} chars omitted) …\n{tail}")
}

fn title_of(tool: &ToolDigest) -> &str {
    let title = tool.title.trim();
    if title.is_empty() {
        "tool call"
    } else {
        title
    }
}

fn render_tool(tool: &ToolDigest, output_chars: usize) -> String {
    let mut out = format!("- {}", title_of(tool));
    let input = clip(&tool.input, TOOL_INPUT_CHARS);
    if !input.is_empty() {
        let _ = write!(out, "\n  input: {input}");
    }
    let output = tool.output.trim();
    if !output.is_empty() {
        let _ = write!(out, "\n  output:\n{}", clip_head_tail(output, output_chars));
    }
    out.push('\n');
    out
}

fn render_tool_title(tool: &ToolDigest) -> String {
    format!("- {}\n", title_of(tool))
}

/// Renders tool digests within `budget`: the newest calls keep their output, older ones fall back
/// to the title only. The output keeps the original call order.
#[must_use]
pub fn render_tools(tools: &[ToolDigest], budget: usize, output_chars: usize) -> String {
    let mut rendered: Vec<String> = Vec::with_capacity(tools.len());
    let mut used = 0usize;
    for tool in tools.iter().rev() {
        let detailed = render_tool(tool, output_chars);
        if used + detailed.len() <= budget {
            used += detailed.len();
            rendered.push(detailed);
            continue;
        }
        let line = render_tool_title(tool);
        if used + line.len() > budget {
            break;
        }
        used += line.len();
        rendered.push(line);
    }
    rendered.reverse();
    rendered.concat()
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
pub fn partial_block(agent: &str, partial_text: &str, tools: &[ToolDigest]) -> String {
    let tools = if tools.is_empty() {
        String::from("(none)")
    } else {
        render_tools(tools, TOOL_BLOCK_BUDGET_CHARS, TOOL_OUTPUT_CHARS)
    };
    format!(
        "<harness-failover from=\"{agent}\">\nPartial output from {agent} before its usage limit was reached (it did not finish this turn; continue the user's request below):\n{partial_text}\nTool calls already made by {agent}:\n{tools}The results above are already known: do not run those tool calls again unless their output may be stale.\n</harness-failover>"
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn digest(title: &str, output: &str) -> ToolDigest {
        ToolDigest {
            title: title.to_owned(),
            input: String::from("{}"),
            output: output.to_owned(),
        }
    }

    #[test]
    fn head_and_tail_survive_multibyte_clipping() {
        let text = "汉".repeat(3_000);
        let clipped = clip_head_tail(&text, TOOL_OUTPUT_CHARS);
        assert!(clipped.contains("(1500 chars omitted)"), "{clipped}");
        assert!(clipped.starts_with('汉'));
        assert!(clipped.ends_with('汉'));
        let kept = clipped.matches('汉').count();
        assert_eq!(kept, TOOL_OUTPUT_CHARS);
        // Short text is untouched.
        assert_eq!(clip_head_tail("abc", TOOL_OUTPUT_CHARS), "abc");
    }

    #[test]
    fn total_budget_keeps_newest_full_and_older_as_titles() {
        let body = "x".repeat(2_000);
        let tools: Vec<ToolDigest> = (0..10)
            .map(|i| digest(&format!("call-{i}"), &body))
            .collect();
        let rendered = render_tools(&tools, 5_000, TOOL_OUTPUT_CHARS);
        assert!(rendered.contains("call-9"));
        assert!(rendered.contains("output:"));
        assert!(rendered.contains("- call-0\n"), "{rendered}");
        assert!(!rendered.contains("call-0\n  input:"), "{rendered}");
        let first = rendered.find("call-0").unwrap();
        let last = rendered.find("call-9").unwrap();
        assert!(first < last);
        assert!(rendered.len() <= 5_000, "{}", rendered.len());
    }

    #[test]
    fn partial_block_carries_tool_output() {
        let tools = vec![digest("Read src/lib.rs", "pub fn main()")];
        let block = partial_block("Codex", "half an answer", &tools);
        assert!(block.contains("Partial output from Codex"));
        assert!(block.contains("Tool calls already made by Codex"));
        assert!(block.contains("Read src/lib.rs"));
        assert!(block.contains("pub fn main()"));
        assert!(block.contains("do not run those tool calls again"));
        // No tools at all keeps the old wording.
        assert!(partial_block("agy", "x", &[]).contains("(none)"));
    }

    #[test]
    fn history_with_tools_stays_within_budget() {
        let body = "y".repeat(20_000);
        let turns: Vec<ContextTurn> = (0..8)
            .map(|seq| ContextTurn {
                seq,
                agent: String::from("Codex"),
                user: String::from("do it"),
                assistant: String::from("done"),
                tools: vec![digest(&format!("shell-{seq}"), &body)],
            })
            .collect();
        let budget = 60_000;
        let text = render_history(&turns, budget);
        assert!(text.len() <= budget, "{}", text.len());
        assert!(text.contains("tools:"));
        assert!(text.contains("chars omitted)"));
        assert!(text.contains("earlier turns omitted") || text.contains("[turn 0]"));
    }
}
