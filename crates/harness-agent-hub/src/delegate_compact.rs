//! Compact `harness-delegate` tool list for agy (phase-9-parity.md §9.8.1): agy declares only small tool
//! definitions up front and loads the rest on demand through a lookup it does not offer over ACP, so every
//! definition stays well under ~260 bytes and the details move into the server instructions.

use serde_json::{json, Value};

pub(crate) const DETAILS: &str = "Tool details. delegate: runs task as one turn of a subagent session in the same project folder; the user watches it in Harness. The subagent does not see this conversation, so put every needed detail (files, goal, constraints, expected output) in task. agent is an id from list_agents (omit it to let Harness pick). Without background it waits and returns the answer; with background true it returns the subagent's session id at once and the answer arrives later as a new message, so do not poll. Up to 4 subagents run at once; give them different files. Pass session_id to continue an earlier subagent. subagent_message(session_id, text, interrupt): queue more instructions; interrupt true stops its current turn first. subagent_wait(session_ids, timeout_s): only when you need a result inside the current turn. claim_paths(paths, note) before editing files, release_paths(paths) when done. notify_parent(text): a short note to the agent that delegated your task.";

fn tool(name: &str, description: &str, properties: &Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {"type": "object", "properties": properties, "required": required}
    })
}

/// Tools of a parent (`child == false`) or subagent session.
pub(crate) fn tools(child: bool) -> Value {
    let s = json!({"type": "string"});
    let paths = json!({"type": "array", "items": {"type": "string"}});
    let claim = tool(
        "claim_paths",
        "Claim files before editing.",
        &json!({"paths": paths, "note": s}),
        &[],
    );
    let release = tool(
        "release_paths",
        "Release claimed paths.",
        &json!({"paths": paths}),
        &[],
    );
    if child {
        return json!([
            tool(
                "notify_parent",
                "Send a note to your parent agent.",
                &json!({"text": s}),
                &["text"]
            ),
            claim,
            release
        ]);
    }
    json!([
        tool("list_agents", "List Harness agents.", &json!({}), &[]),
        tool(
            "delegate",
            "Start a subagent.",
            &json!({"task": s, "agent": s, "background": {"type": "boolean"}, "session_id": s}),
            &["task"]
        ),
        tool(
            "subagent_message",
            "Message a subagent.",
            &json!({"session_id": s, "text": s, "interrupt": {"type": "boolean"}}),
            &["session_id", "text"]
        ),
        tool(
            "subagent_wait",
            "Wait for subagents.",
            &json!({"session_ids": paths, "timeout_s": {"type": "integer"}}),
            &[]
        ),
        claim,
        release
    ])
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_definition_is_small_enough_for_agy_to_load_up_front() {
        for child in [false, true] {
            let names = |v: &serde_json::Value| -> Vec<String> {
                v.as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|t| t["name"].as_str().map(str::to_owned))
                    .collect()
            };
            let tools = super::tools(child);
            assert_eq!(names(&tools), names(&crate::delegate_mcp::tool_list(child)));
            for t in tools.as_array().into_iter().flatten() {
                let stored = serde_json::json!({
                    "name": t["name"], "description": t["description"], "parameters": t["inputSchema"]
                });
                assert!(stored.to_string().len() < 240, "{stored}");
            }
        }
    }
}
