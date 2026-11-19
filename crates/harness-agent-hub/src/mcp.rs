//! Harness MCP memory server (spec §8.3): MCP over stdio, newline-delimited JSON-RPC.

use std::io::{BufRead, Write};
use std::path::Path;

use serde_json::{json, Value};

use crate::memory::{MemoryFiles, MemoryTarget};
use crate::recall;
use crate::session_notes;

/// MCP protocol version we answer with.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// Runs the server until stdin closes.
///
/// `home` is the Harness data dir (session notes, the recall store); `project`, `session` and `agent` (the
/// display name memories are attributed to) are optional so a helper started by hand still answers.
///
/// # Errors
///
/// I/O errors on stdout.
pub fn serve(
    memory: &MemoryFiles,
    home: &Path,
    project: Option<&Path>,
    session: Option<&str>,
    agent: Option<&str>,
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
            "tools/call" => Ok(call_tool(memory, home, project, session, agent, &params)),
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
        },
        {
            "name": "memory_search",
            "description": MEMORY_SEARCH,
            "inputSchema": {"type":"object","properties":{
                "query": {"type":"string","description":"Words to look for (names, identifiers, topics). Not a question to answer."},
                "kind": {"type":"string","enum":["fact","preference","decision","howto","episode"],"description":"Only this kind. episode = digests of earlier sessions."},
                "limit": {"type":"integer","minimum":1,"maximum":20,"default":8}
            },"required":["query"]}
        },
        {
            "name": "memory_add",
            "description": MEMORY_ADD,
            "inputSchema": {"type":"object","properties":{
                "text": {"type":"string","description": format!("One fact in a sentence or two, at most {} characters. No credentials.", crate::recall::MAX_TEXT_CHARS)},
                "kind": {"type":"string","enum":["fact","preference","decision","howto"],"default":"fact"},
                "scope": {"type":"string","enum":["project","global"],"default":"project","description":"project = only sessions in this project see it; global = every project (user preferences)."},
                "pinned": {"type":"boolean","description":"Show it in every new session of its scope, whatever the request. Default: true for preferences, false otherwise."},
                "replaces": {"type":"string","description":"Id of a memory this one corrects or updates; the old one is retired."},
                "ttl_days": {"type":"integer","minimum":1,"description":"Forget it after this many days (temporary facts)."}
            },"required":["text"]}
        },
        {
            "name": "memory_forget",
            "description": "Forget one memory of the Harness local memory by its id (from memory_search or the recall block). Use it when a memory is wrong or no longer true and nothing replaces it.",
            "inputSchema": {"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}
        },
        {
            "name": "memory_recent",
            "description": "The latest sessions of this project (what was asked, how it ended) and the latest saved memories, newest first. Use it to see what was worked on before this session.",
            "inputSchema": {"type":"object","properties":{"limit":{"type":"integer","minimum":1,"maximum":20,"default":8}}}
        },
        {
            "name": "memory_session",
            "description": "Read an earlier Harness session: its digest and its working notes (files with line pointers, findings, decisions). Pass the session id shown by memory_search / memory_recent, or the id of an episode memory.",
            "inputSchema": {"type":"object","properties":{"session":{"type":"string"}},"required":["session"]}
        }
    ])
}

const MEMORY_SEARCH: &str =
    "Search the Harness local memory shared by every agent and session of this user: saved facts, \
preferences, decisions, how-tos and digests of earlier sessions, for this project and global. Word search \
(no embeddings): pass the names and terms that matter. Call it before asking the user to repeat something, \
and when the task refers to earlier work.";

const MEMORY_ADD: &str =
    "Save one durable memory to the Harness local memory, visible to every agent in later sessions. Save \
what a future session would otherwise have to rediscover or ask again: a user preference, a decision and \
its reason, a fact about the project or environment, a procedure that worked. Not for progress of the \
current task (use session_notes_write) and never credentials. The same text is not stored twice; pass \
`replaces` to correct an older memory.";

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

fn text_result(text: impl Into<String>) -> Value {
    json!({"content":[{"type":"text","text":text.into()}]})
}

/// One memory as a result line: id, kind, age, who wrote it, text.
fn memory_line(memory: &recall::Memory, now: i64) -> String {
    use std::fmt::Write as _;
    let mut head = format!(
        "[{}] {} · {}",
        memory.id,
        memory.kind.as_str(),
        recall::age(now, memory.at)
    );
    if let Some(agent) = &memory.agent {
        let _ = write!(head, " · {agent}");
    }
    if memory.project.is_none() {
        head.push_str(" · global");
    }
    if memory.pinned {
        head.push_str(" · pinned");
    }
    if memory.kind == recall::Kind::Episode {
        if let Some(session) = &memory.session {
            let _ = write!(head, " · session {session}");
        }
        if let Some(title) = &memory.title {
            let _ = write!(head, " · \"{title}\"");
        }
    }
    format!("{head}\n{}", memory.text)
}

/// What the recall tools know about their caller.
struct RecallCall<'a> {
    store: recall::Store,
    home: &'a Path,
    /// [`recall::project_key`] of the session's workspace.
    project: Option<String>,
    session: Option<&'a str>,
    agent: Option<&'a str>,
    now: i64,
}

fn limit_of(args: &Value, default: usize) -> usize {
    args.get("limit")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(default)
        .clamp(1, 20)
}

fn recall_search(call: &RecallCall<'_>, args: &Value) -> Value {
    let Some(query) = args.get("query").and_then(Value::as_str) else {
        return tool_error("query is required");
    };
    let kind = match args.get("kind").and_then(Value::as_str) {
        None => None,
        Some("episode") => Some(recall::Kind::Episode),
        Some(other) => match recall::Kind::parse(other) {
            Some(kind) => Some(kind),
            None => return tool_error("kind must be fact, preference, decision, howto or episode"),
        },
    };
    let all = call.store.load();
    let seen = recall::visible(&all, call.project.as_deref());
    let hits = recall::search(
        &seen,
        &recall::Query {
            text: query,
            limit: limit_of(args, 8),
            need: 1,
            kind,
        },
    );
    if hits.is_empty() {
        return text_result(format!(
            "No memory matches \"{query}\" ({} stored for this project and global). Try other words, or memory_recent.",
            seen.len()
        ));
    }
    let lines: Vec<String> = hits
        .iter()
        .map(|hit| memory_line(hit.memory, call.now))
        .collect();
    text_result(lines.join("\n\n"))
}

fn recall_add(call: &RecallCall<'_>, args: &Value) -> Value {
    let Some(text) = args.get("text").and_then(Value::as_str) else {
        return tool_error("text is required");
    };
    let kind = match args.get("kind").and_then(Value::as_str) {
        None => None,
        Some(other) => match recall::Kind::parse(other) {
            Some(kind) => Some(kind),
            None => return tool_error("kind must be fact, preference, decision or howto"),
        },
    };
    let project = match args.get("scope").and_then(Value::as_str) {
        Some("global") => None,
        Some("project") | None => match &call.project {
            Some(key) => Some(key.clone()),
            None => {
                return tool_error("no project configured for this session: use scope \"global\"")
            }
        },
        Some(_) => return tool_error("scope must be \"project\" or \"global\""),
    };
    let new = recall::NewMemory {
        kind,
        text: text.to_owned(),
        project,
        pinned: args.get("pinned").and_then(Value::as_bool),
        session: call.session.map(str::to_owned),
        agent: call.agent.map(str::to_owned),
        replaces: args
            .get("replaces")
            .and_then(Value::as_str)
            .map(str::to_owned),
        ttl_days: args
            .get("ttl_days")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
    };
    match call.store.add(new) {
        Ok(recall::Added::New(id)) => text_result(format!("saved as {id}")),
        Ok(recall::Added::Duplicate(id)) => {
            text_result(format!("already stored as {id} (nothing added)"))
        }
        Err(message) => tool_error(&message),
    }
}

fn recall_forget(call: &RecallCall<'_>, args: &Value) -> Value {
    let Some(id) = args.get("id").and_then(Value::as_str) else {
        return tool_error("id is required");
    };
    match call.store.forget(id) {
        Ok(0) => tool_error(&format!("no memory with id {id}")),
        Ok(_) => text_result("forgotten"),
        Err(e) => tool_error(&format!("write failed: {e}")),
    }
}

fn recall_recent(call: &RecallCall<'_>, args: &Value) -> Value {
    let all = call.store.load();
    let mut seen = recall::visible(&all, call.project.as_deref());
    seen.retain(|m| m.kind != recall::Kind::Episode || m.session.as_deref() != call.session);
    seen.sort_by_key(|m| std::cmp::Reverse(m.at));
    if seen.is_empty() {
        return text_result("(the local memory is empty for this project)");
    }
    let lines: Vec<String> = seen
        .iter()
        .take(limit_of(args, 8))
        .map(|m| memory_line(m, call.now))
        .collect();
    text_result(lines.join("\n\n"))
}

fn recall_session(call: &RecallCall<'_>, args: &Value) -> Value {
    let Some(wanted) = args.get("session").and_then(Value::as_str) else {
        return tool_error("session is required");
    };
    let all = call.store.load();
    let seen = recall::visible(&all, call.project.as_deref());
    let Some(episode) = seen.iter().find(|m| {
        m.kind == recall::Kind::Episode && (m.session.as_deref() == Some(wanted) || m.id == wanted)
    }) else {
        return tool_error(&format!(
            "no earlier session {wanted} in this project's memory (memory_recent lists them)"
        ));
    };
    let id = episode.session.clone().unwrap_or_default();
    let notes = session_notes::read(call.home, &id);
    let notes = if notes.trim().is_empty() {
        "(that session kept no working notes)".to_owned()
    } else {
        notes
    };
    text_result(format!(
        "{}\n\nWorking notes of that session:\n{notes}",
        memory_line(episode, call.now)
    ))
}

/// The recall tools (`memory_search`, `memory_add`, `memory_forget`, `memory_recent`, `memory_session`).
fn call_recall(
    name: &str,
    args: &Value,
    home: &Path,
    project: Option<&Path>,
    session: Option<&str>,
    agent: Option<&str>,
) -> Value {
    let call = RecallCall {
        store: recall::Store::new(home),
        home,
        project: project.map(|p| recall::project_key(&p.to_string_lossy())),
        session,
        agent,
        now: recall::now_ms(),
    };
    match name {
        "memory_search" => recall_search(&call, args),
        "memory_add" => recall_add(&call, args),
        "memory_forget" => recall_forget(&call, args),
        "memory_recent" => recall_recent(&call, args),
        "memory_session" => recall_session(&call, args),
        _ => tool_error(&format!("unknown tool: {name}")),
    }
}

fn call_tool(
    memory: &MemoryFiles,
    home: &Path,
    project: Option<&Path>,
    session: Option<&str>,
    agent: Option<&str>,
    params: &Value,
) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    if matches!(
        name,
        "memory_search" | "memory_add" | "memory_forget" | "memory_recent" | "memory_session"
    ) {
        return call_recall(name, &args, home, project, session, agent);
    }
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
        serve_in(home, None, session, None, lines)
    }

    fn serve_in(
        home: &Path,
        project: Option<&Path>,
        session: Option<&str>,
        agent: Option<&str>,
        lines: &[&str],
    ) -> Vec<Value> {
        let memory = MemoryFiles::new(home);
        let input = lines.join("\n");
        let mut output = Vec::new();
        serve(
            &memory,
            home,
            project,
            session,
            agent,
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
                "session_notes_write",
                "memory_search",
                "memory_add",
                "memory_forget",
                "memory_recent",
                "memory_session"
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

    #[allow(clippy::needless_pass_by_value)]
    fn call(id: u32, name: &str, arguments: Value) -> String {
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}})
            .to_string()
    }

    /// One agent saves in one session; another agent in a later session of the same project finds it, reads
    /// the earlier session, corrects the memory and forgets it. A session of another project sees nothing.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn recall_tools_carry_memories_across_sessions_and_agents() {
        let home = std::env::temp_dir().join(format!(
            "harness-mcp-recall-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&home).unwrap();
        let project = Path::new("/work/demo");
        // Session 1, Claude Code.
        let lines = [
            call(
                1,
                "memory_add",
                json!({"text": "Pairing tokens expire after ten minutes because of replay attacks", "kind": "decision"}),
            ),
            call(
                2,
                "memory_add",
                json!({"text": "Reply in Indonesian, casual", "kind": "preference", "scope": "global"}),
            ),
            call(
                3,
                "memory_add",
                json!({"text": "pairing tokens expire after ten minutes because of  replay attacks"}),
            ),
            call(
                4,
                "memory_add",
                json!({"text": "the deploy key is ghp_abcdefghijklmnopqrstuvwxyz0123456789"}),
            ),
            call(
                5,
                "session_notes_write",
                json!({"content": "auth.rs:40 is_live reads the clock twice"}),
            ),
        ];
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let first = serve_in(
            &home,
            Some(project),
            Some("ses-1"),
            Some("Claude Code"),
            &lines,
        );
        assert!(tool_text(&first[0]).starts_with("saved as m"));
        assert!(tool_text(&first[2]).starts_with("already stored as m"));
        assert_eq!(first[3]["result"]["isError"], true);
        assert!(tool_text(&first[3]).contains("Never store credentials"));
        crate::recall::Store::new(&home)
            .put_episode(
                "ses-1",
                "/work/demo",
                "Pairing expiry",
                "Claude Code",
                "Asked: fix the pairing expiry | Result: fixed",
            )
            .unwrap();
        let decision = tool_text(&first[0])
            .trim_start_matches("saved as ")
            .to_owned();

        // Session 2, Codex, same project.
        let lines = [
            call(
                1,
                "memory_search",
                json!({"query": "why do pairing tokens expire"}),
            ),
            call(2, "memory_recent", json!({})),
            call(3, "memory_session", json!({"session": "ses-1"})),
            call(
                4,
                "memory_add",
                json!({"text": "Pairing tokens expire after five minutes since v0.2", "kind": "decision", "replaces": decision}),
            ),
            call(
                5,
                "memory_search",
                json!({"query": "pairing tokens expire"}),
            ),
            call(6, "memory_search", json!({"query": "kubernetes ingress"})),
        ];
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let second = serve_in(&home, Some(project), Some("ses-2"), Some("Codex"), &lines);
        let found = tool_text(&second[0]);
        assert!(
            found.contains("decision")
                && found.contains("Claude Code")
                && found.contains("replay attacks"),
            "{found}"
        );
        assert!(
            found.contains("session ses-1"),
            "the episode of the earlier session is found too: {found}"
        );
        let recent = tool_text(&second[1]);
        assert!(
            recent.contains("Reply in Indonesian") && recent.contains("global · pinned"),
            "{recent}"
        );
        let earlier = tool_text(&second[2]);
        assert!(
            earlier.contains("\"Pairing expiry\"")
                && earlier.contains("auth.rs:40 is_live reads the clock twice"),
            "{earlier}"
        );
        let after = tool_text(&second[4]);
        assert!(
            after.contains("five minutes")
                && after.contains("Codex")
                && !after.contains("ten minutes"),
            "{after}"
        );
        assert!(tool_text(&second[5]).starts_with("No memory matches"));

        // Another project: only the global preference.
        let lines = [
            call(1, "memory_search", json!({"query": "pairing tokens"})),
            call(2, "memory_recent", json!({})),
        ];
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let other = serve_in(
            &home,
            Some(Path::new("/work/other")),
            Some("ses-3"),
            Some("Codex"),
            &lines,
        );
        assert!(tool_text(&other[0]).starts_with("No memory matches"));
        assert!(
            tool_text(&other[1]).contains("Reply in Indonesian")
                && !tool_text(&other[1]).contains("Pairing")
        );

        // Forget.
        let id = tool_text(&second[3])
            .trim_start_matches("saved as ")
            .to_owned();
        let lines = [
            call(1, "memory_forget", json!({"id": id})),
            call(2, "memory_forget", json!({"id": "m-unknown"})),
        ];
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let gone = serve_in(&home, Some(project), Some("ses-2"), Some("Codex"), &lines);
        assert_eq!(tool_text(&gone[0]), "forgotten");
        assert_eq!(gone[1]["result"]["isError"], true);
        let _ = std::fs::remove_dir_all(&home);
    }
}
