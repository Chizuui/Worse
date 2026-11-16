//! Harness MCP memory server (spec §8.3): MCP over stdio, newline-delimited JSON-RPC.

use std::io::{BufRead, Write};
use std::path::Path;

use serde_json::{json, Value};

use crate::memory::{MemoryFiles, MemoryTarget};
use crate::session_notes;

/// MCP protocol version we answer with.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// Runs the server until stdin closes.
///
/// `home` is the Harness data dir (session notes); `project` and `session` are optional so a helper started
/// by hand still answers.
///
/// # Errors
///
/// I/O errors on stdout.
pub fn serve(
    memory: &MemoryFiles,
    home: &Path,
    project: Option<&Path>,
    session: Option<&str>,
    input: impl BufRead,
    mut output: impl Write,
) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            write_line(
                &mut output,
                &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}),
            )?;
            continue;
        };
        let Some(id) = message.get("id").cloned() else {
            continue; // notification (e.g. notifications/initialized)
        };
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let reply = match method {
            "initialize" => Ok(json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "harness-memory", "version": env!("CARGO_PKG_VERSION")}
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tool_list()})),
            "tools/call" => Ok(call_tool(memory, home, project, session, &params)),
            _ => Err(json!({"code": -32601, "message": format!("method not found: {method}")})),
        };
        let response = match reply {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(error) => json!({"jsonrpc":"2.0","id":id,"error":error}),
        };
        write_line(&mut output, &response)?;
    }
    Ok(())
}

fn write_line(output: &mut impl Write, value: &Value) -> std::io::Result<()> {
    output.write_all(value.to_string().as_bytes())?;
    output.write_all(b"\n")?;
    output.flush()
}

fn tool_list() -> Value {
    let scope = json!({"type":"string","enum":["global","project"],"description":"global = ~/.harness/MEMORY.md shared by all projects; project = <project>/.harness/MEMORY.md"});
    json!([
        {
            "name": "memory_read",
            "description": "Read the shared Harness MEMORY.md (shared by every agent: Claude Code, Codex, agy).",
            "inputSchema": {"type":"object","properties":{"scope":scope},"required":["scope"]}
        },
        {
            "name": "memory_write",
            "description": "Write the shared Harness MEMORY.md. mode=append adds a line, mode=replace overwrites.",
            "inputSchema": {"type":"object","properties":{
                "scope": scope,
                "content": {"type":"string"},
                "mode": {"type":"string","enum":["append","replace"],"default":"append"}
            },"required":["scope","content"]}
        },
        {
            "name": "session_notes_read",
            "description": SESSION_NOTES_READ,
            "inputSchema": {"type":"object","properties":{}}
        },
        {
            "name": "session_notes_write",
            "description": SESSION_NOTES_WRITE,
            "inputSchema": {"type":"object","properties":{
                "content": {
                    "type":"string",
                    "description": format!(
                        "The full notes text (markdown), replacing the previous notes. At most {} characters.",
                        crate::session_notes::MAX_NOTES_CHARS
                    )
                }
            },"required":["content"]}
        }
    ])
}

const SESSION_NOTES_READ: &str =
    "Read the working notes of this Harness session (files with line pointers, findings, decisions, next \
steps). Call it when your context was compacted, after another agent handed the session over, or when you \
are unsure what was already established. The same notes are injected into this prompt at the start of a \
fresh session.";

const SESSION_NOTES_WRITE: &str =
    "Replace the working notes of this Harness session. Call it after each meaningful discovery or \
decision while you work: relevant files with line pointers, root causes, decisions taken, what is already \
done, what is next. They are injected into the prompt when another agent takes the session over (quota \
failover) or a native session starts fresh, so a later agent can continue without re-reading everything. \
Write the complete notes every time (markdown, at most 16k characters: condense instead of growing \
without bound) and never put secrets (API keys, tokens, passwords) in them.";

fn tool_error(message: &str) -> Value {
    json!({"content":[{"type":"text","text":message}],"isError":true})
}

fn call_tool(
    memory: &MemoryFiles,
    home: &Path,
    project: Option<&Path>,
    session: Option<&str>,
    params: &Value,
) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    // 会话笔记工具不需要 scope 参数，先于 memory_read / memory_write 处理。
    if name.starts_with("session_notes_") {
        let Some(session) = session else {
            return tool_error(
                "no Harness session for this helper (no --session / HARNESS_SESSION_ID), \
so this session has no working notes",
            );
        };
        return match name {
            "session_notes_read" => {
                let text = session_notes::read(home, session);
                let text = if text.trim().is_empty() {
                    "(no notes yet)".to_owned()
                } else {
                    text
                };
                json!({"content":[{"type":"text","text":text}]})
            }
            "session_notes_write" => {
                let Some(content) = args.get("content").and_then(Value::as_str) else {
                    return tool_error("content is required (the full notes text)");
                };
                match session_notes::write(home, session, content) {
                    Ok(()) => json!({"content":[{"type":"text","text":"ok"}]}),
                    Err(e) => tool_error(&format!("write failed: {e}")),
                }
            }
            _ => tool_error(&format!("unknown tool: {name}")),
        };
    }
    let target = match args.get("scope").and_then(Value::as_str) {
        Some("global") => MemoryTarget::Global,
        Some("project") => match project {
            Some(root) => MemoryTarget::Project(root.to_path_buf()),
            None => return tool_error("no project configured for this session"),
        },
        _ => return tool_error("scope must be \"global\" or \"project\""),
    };
    match name {
        "memory_read" => match memory.read(&target) {
            Ok(text) => json!({"content":[{"type":"text","text":text}]}),
            Err(e) => tool_error(&format!("read failed: {e}")),
        },
        "memory_write" => {
            let Some(content) = args.get("content").and_then(Value::as_str) else {
                return tool_error("content is required");
            };
            let append = args.get("mode").and_then(Value::as_str) != Some("replace");
            match memory.write(&target, content, append) {
                Ok(()) => json!({"content":[{"type":"text","text":"ok"}]}),
                Err(e) => tool_error(&format!("write failed: {e}")),
            }
        }
        _ => tool_error(&format!("unknown tool: {name}")),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn serve_lines(home: &Path, session: Option<&str>, lines: &[&str]) -> Vec<Value> {
        let memory = MemoryFiles::new(home);
        let input = lines.join("\n");
        let mut output = Vec::new();
        serve(
            &memory,
            home,
            None,
            session,
            std::io::Cursor::new(input.into_bytes()),
            &mut output,
        )
        .unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect()
    }

    fn tool_text(reply: &Value) -> &str {
        reply["result"]["content"][0]["text"].as_str().unwrap()
    }

    #[test]
    fn notes_tools_round_trip_with_a_session() {
        let home = std::env::temp_dir().join(format!("harness-mcp-notes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let replies = serve_lines(
            &home,
            Some("ses-1"),
            &[
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"session_notes_read","arguments":{}}}"#,
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"session_notes_write","arguments":{"content":"files: mcp.rs:111"}}}"#,
                r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"session_notes_read","arguments":{}}}"#,
            ],
        );
        let names: Vec<&str> = replies[0]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "memory_read",
                "memory_write",
                "session_notes_read",
                "session_notes_write"
            ]
        );
        assert_eq!(tool_text(&replies[1]), "(no notes yet)");
        assert_eq!(tool_text(&replies[2]), "ok");
        assert_eq!(tool_text(&replies[3]), "files: mcp.rs:111\n");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn notes_tools_without_a_session_say_so() {
        let home =
            std::env::temp_dir().join(format!("harness-mcp-nosession-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let replies = serve_lines(
            &home,
            None,
            &[
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"session_notes_read","arguments":{}}}"#,
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"session_notes_write","arguments":{"content":"x"}}}"#,
            ],
        );
        for reply in &replies {
            assert_eq!(reply["result"]["isError"], true);
            assert!(tool_text(reply).contains("HARNESS_SESSION_ID"), "{reply}");
        }
        let _ = std::fs::remove_dir_all(&home);
    }
}
