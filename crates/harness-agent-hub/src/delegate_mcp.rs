//! `harness-delegate` MCP server (phase-9-parity.md §9.8): stdio MCP for the parent agent, forwarding each tool
//! call as one JSON line over a loopback TCP connection to the hub's delegate server.
//!
//! Synchronous like the memory server; every `tools/call` runs on its own thread because `delegate` blocks until
//! the subagent finishes and an agent may run several in parallel.

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::mcp::MCP_PROTOCOL_VERSION;

/// How often a waiting call sends `notifications/progress` (keeps client tool timeouts from firing when the client
/// resets them on progress).
const PROGRESS_EVERY: Duration = Duration::from_secs(15);

/// Where the hub listens; from `HARNESS_DELEGATE_ADDR` / `_TOKEN` / `_SESSION`.
#[derive(Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct DelegateEnv {
    pub addr: String,
    pub token: String,
    pub session: String,
    /// `HARNESS_DELEGATE_ROLE=child` (§9.8.1): only selects the tool list; the hub enforces the role itself.
    pub child: bool,
    pub router: bool,
    /// `HARNESS_DELEGATE_ROLE=nested`: a subagent that may start subagents (delegate tools plus `notify_parent`).
    pub nested: bool,
    /// `HARNESS_DELEGATE_COMPACT=1` (agy): small tool definitions, details in the instructions.
    pub compact: bool,
}

impl std::fmt::Debug for DelegateEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DelegateEnv")
            .field("addr", &self.addr)
            .field("session", &self.session)
            .field("child", &self.child)
            .field("router", &self.router)
            .field("nested", &self.nested)
            .finish_non_exhaustive()
    }
}

impl DelegateEnv {
    /// Reads the three env vars; `None` when any is missing.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let get = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        Some(Self {
            addr: get("HARNESS_DELEGATE_ADDR")?,
            token: get("HARNESS_DELEGATE_TOKEN")?,
            session: get("HARNESS_DELEGATE_SESSION")?,
            child: get("HARNESS_DELEGATE_ROLE").as_deref() == Some("child"),
            router: get("HARNESS_DELEGATE_ROLE").as_deref() == Some("router"),
            nested: get("HARNESS_DELEGATE_ROLE").as_deref() == Some("nested"),
            compact: get("HARNESS_DELEGATE_COMPACT").as_deref() == Some("1"),
        })
    }
}

type Out = Arc<Mutex<Box<dyn Write + Send>>>;

/// Runs the server until stdin closes, then waits for running calls.
///
/// # Errors
///
/// I/O errors on stdin.
pub fn serve(
    env: &DelegateEnv,
    input: impl BufRead,
    output: Box<dyn Write + Send>,
) -> std::io::Result<()> {
    let out: Out = Arc::new(Mutex::new(output));
    let mut workers = Vec::new();
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            write(
                &out,
                &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}),
            );
            continue;
        };
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        match method {
            "initialize" => {
                let mut result = json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "harness-delegate", "version": env!("CARGO_PKG_VERSION")}
                });
                if !env.child && !env.router {
                    result["instructions"] = json!(PARENT_INSTRUCTIONS);
                }
                if env.compact && !env.router {
                    let base = result["instructions"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    result["instructions"] =
                        json!(format!("{base}\n\n{}", crate::delegate_compact::DETAILS)
                            .trim()
                            .to_owned());
                }
                reply(&out, &id, &result);
            }
            "ping" => reply(&out, &id, &json!({})),
            "tools/list" => {
                let list = |child: bool| {
                    if env.compact {
                        crate::delegate_compact::tools(child)
                    } else {
                        tool_list(child)
                    }
                };
                let tools = always_loaded(if env.router {
                    router_tools()
                } else if env.nested {
                    nested_tools(&list(false), &list(true))
                } else {
                    list(env.child)
                });
                reply(&out, &id, &json!({ "tools": tools }));
            }
            "tools/call" => {
                let (env, out, params) = (env.clone(), Arc::clone(&out), message["params"].clone());
                workers.push(std::thread::spawn(move || {
                    let result = call(&env, &out, &params);
                    reply(&out, &id, &result);
                }));
            }
            _ => write(
                &out,
                &json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":format!("method not found: {method}")}}),
            ),
        }
        workers.retain(|w| !w.is_finished());
    }
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}

fn write(out: &Out, value: &Value) {
    if let Ok(mut out) = out.lock() {
        let _ = out.write_all(format!("{value}\n").as_bytes());
        let _ = out.flush();
    }
}

fn reply(out: &Out, id: &Value, result: &Value) {
    write(out, &json!({"jsonrpc":"2.0","id":id,"result":result}));
}

const PARENT_INSTRUCTIONS: &str = "You run inside Harness. Subagents in Harness are the user's other AI agents (for example Codex, DeepSeek or another Claude account), configured in the Harness Orchestra. Whenever the user asks you to deploy, spawn, use or ask a subagent, delegate work, or ask another agent or model, call this server's list_agents and delegate tools. Do not use your own built-in subagent, Task or Agent tool for that: it runs a copy of yourself, not the user's subagents.";

/// Marks every tool `antigravity/alwaysLoad` so agy declares it up front instead of loading it on demand
/// (agy lists on-demand tools only through a lookup it does not offer over ACP).
fn always_loaded(mut tools: Value) -> Value {
    for tool in tools.as_array_mut().into_iter().flatten() {
        tool["_meta"] = json!({"antigravity/alwaysLoad": true});
    }
    tools
}

fn claim_tools() -> [Value; 2] {
    [
        json!({
            "name": "claim_paths",
            "description": "Claim files or folders (relative to the workspace) before editing them, so parallel agents do not edit the same files. Returns which paths were granted, which are already held by another agent (with its session, title and note), and every current claim. Do not edit a path you could not claim; coordinate instead. Call with no paths to only list claims. Your claims are released automatically when your turn ends.",
            "inputSchema": {"type": "object", "properties": {
                "paths": {"type": "array", "items": {"type": "string"}, "description": "Workspace-relative files or folders, e.g. src/app.rs or crates/hub."},
                "note": {"type": "string", "description": "What you are changing there (shown to other agents)."}
            }}
        }),
        json!({
            "name": "release_paths",
            "description": "Release paths you claimed (all of them when paths is omitted) as soon as you are done editing them.",
            "inputSchema": {"type": "object", "properties": {
                "paths": {"type": "array", "items": {"type": "string"}}
            }}
        }),
    ]
}

fn router_tools() -> Value {
    json!([
        {
            "name": "list_agents",
            "description": "List the Orchestra team you can route requests to (id, name, state, role).",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "route_trigger",
            "description": "Hand one inbox request to an agent of the team. Harness opens a ticket with its own git worktree for that agent and adds your brief to the agent's first message.",
            "inputSchema": {"type": "object", "properties": {
                "trigger_id": {"type": "string", "description": "The request id, e.g. comment:123 or issue:456."},
                "agent": {"type": "string", "description": "Agent id from list_agents (or your own id to take it yourself)."},
                "brief": {"type": "string", "description": "Short notes for the agent: what to do and where to look."}
            }, "required": ["trigger_id", "agent"]}
        },
        {
            "name": "dismiss_trigger",
            "description": "Close one inbox request without work (spam, duplicate, already answered). Nothing is posted to GitHub.",
            "inputSchema": {"type": "object", "properties": {
                "trigger_id": {"type": "string", "description": "The request id."},
                "reason": {"type": "string", "description": "One short sentence shown in the Harness trigger log."}
            }, "required": ["trigger_id", "reason"]}
        }
    ])
}

/// Delegate tools of a parent plus `notify_parent` from the subagent list.
pub(crate) fn nested_tools(parent: &Value, child: &Value) -> Value {
    let mut tools = parent.as_array().cloned().unwrap_or_default();
    if let Some(notify) = child
        .as_array()
        .and_then(|list| list.iter().find(|t| t["name"] == "notify_parent"))
    {
        tools.push(notify.clone());
    }
    Value::Array(tools)
}

pub(crate) fn tool_list(child: bool) -> Value {
    let [claim, release] = claim_tools();
    if child {
        return json!([
            {
                "name": "notify_parent",
                "description": "Send a short note to the agent that delegated your task, without ending your turn: progress, a question, a blocker, or a conflict on claimed paths. It is delivered when the parent next waits on its subagents.",
                "inputSchema": {"type": "object", "properties": {
                    "text": {"type": "string", "description": "The note (at most 4000 characters)."}
                }, "required": ["text"]}
            },
            claim,
            release
        ]);
    }
    json!([
        {
            "name": "list_agents",
            "description": "List the AI coding agents Harness can delegate to (id, name, state). Use an id as the `agent` of delegate.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "delegate",
            "description": "Hand a self-contained task to another AI coding agent managed by Harness (for example Codex, another Claude account, or an API model such as DeepSeek). Use this, not a built-in subagent/Task/Agent tool, whenever the user asks for a subagent. The subagent works in the same project folder in its own Harness session, which the user can watch live. The subagent does not see this conversation: put every needed detail (files, goal, constraints, expected output) in `task`. By default this call waits and returns the final answer. With `background: true it returns at once with the subagent's session id, so you can keep working or end your turn and send it more instructions with subagent_message. When it finishes, Harness delivers its answer to you automatically as a new message, so do not poll with subagent_wait; stopping your own turn does not stop the subagent. Several subagents can run at once (the user sets the limit in Harness settings). To send a follow-up to the same subagent, pass the session_id from its earlier result. Split work so subagents edit different files; they claim paths with claim_paths.",
            "inputSchema": {"type": "object", "properties": {
                "task": {"type": "string", "description": "Complete instructions for the subagent."},
                "agent": {"type": "string", "description": "Agent id from list_agents. Omit to let Harness pick: the user's decision model chooses the agent (and model) that suits the task when one is set, otherwise the first available agent."},
                "title": {"type": "string", "description": "Short label shown in the Harness subagent panel."},
                "session_id": {"type": "string", "description": "Continue an earlier subagent session instead of starting a new one."},
                "background": {"type": "boolean", "description": "Return immediately; the answer is delivered to you as a new message when the subagent finishes."},
                "scope": {"type": "object", "description": "Optional narrowing of the subagent's permissions; a wider scope than your own is rejected.", "properties": {
                    "paths": {"type": "array", "items": {"type": "string"}, "description": "Workspace-relative paths or folders it may touch; omit for the whole workspace."},
                    "tools": {"type": "array", "items": {"type": "string"}, "description": "Tool name fragments it may use; omit for every tool."},
                    "network": {"type": "boolean", "description": "Whether network tools are allowed (default true, and it cannot be wider than your own scope)."},
                    "minutes": {"type": "integer", "description": "Minutes until its permissions expire."}
                }}
            }, "required": ["task"]}
        },
        {
            "name": "subagent_message",
            "description": "Send new instructions to one of your subagents while it runs (or after it finished). The message is queued as its next turn; with interrupt: true its current turn is stopped first and the message is sent right away. Returns immediately; the reply is delivered to you as a new message.",
            "inputSchema": {"type": "object", "properties": {
                "session_id": {"type": "string", "description": "The subagent's session id."},
                "text": {"type": "string", "description": "The message."},
                "interrupt": {"type": "boolean", "description": "Stop the current turn first (default false)."}
            }, "required": ["session_id", "text"]}
        },
        {
            "name": "subagent_wait",
            "description": "Wait until one of your subagents finishes a turn or sends a note (notify_parent), then return every subagent's status, the unread notes and the full answers of newly finished turns. Only needed when you must have a result inside the current turn: finished answers are otherwise delivered to you automatically as a new message. `timeout_s: 0 returns the status immediately.",
            "inputSchema": {"type": "object", "properties": {
                "session_ids": {"type": "array", "items": {"type": "string"}, "description": "Only these subagents (default: all of yours)."},
                "timeout_s": {"type": "integer", "description": "Seconds to wait (default 300, at most 3600)."}
            }}
        },
        claim,
        release
    ])
}
fn tool_text(text: &str, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

#[allow(clippy::too_many_lines)]
fn call(env: &DelegateEnv, out: &Out, params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let op = match name {
        "list_agents" => "agents",
        "delegate" => "delegate",
        "subagent_message" => "message",
        "subagent_wait" => "wait",
        "notify_parent" => "notify",
        "claim_paths" => "claim",
        "release_paths" => "release",
        "route_trigger" => "route",
        "dismiss_trigger" => "dismiss",
        _ => return tool_text(&format!("unknown tool: {name}"), true),
    };
    let progress = params["_meta"]["progressToken"].clone();
    let request = json!({"token": env.token, "parent": env.session, "op": op, "args": args});
    let response = match exchange(env, &request, out, &progress) {
        Ok(value) => value,
        Err(error) => {
            return tool_text(
                &format!("Harness delegate server unreachable: {error}"),
                true,
            );
        }
    };
    if response["ok"].as_bool() != Some(true) {
        let error = response["error"].as_str().unwrap_or("delegate failed");
        return tool_text(error, true);
    }
    let result = &response["result"];
    if op == "agents" {
        let lines: Vec<String> = result
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|x| {
                        let mut line = format!(
                            "{} — {} ({})",
                            x["id"].as_str().unwrap_or(""),
                            x["name"].as_str().unwrap_or(""),
                            x["state"].as_str().unwrap_or("")
                        );
                        if let Some(role) = x["role"].as_str() {
                            let _ = write!(line, " · role: {role}");
                            if let Some(note) = x["instructions"].as_str().filter(|n| !n.is_empty())
                            {
                                let _ = write!(line, " · {note}");
                            }
                        }
                        line
                    })
                    .collect()
            })
            .unwrap_or_default();
        return tool_text(&lines.join("\n"), false);
    }
    match op {
        "message" => return tool_text(&message_text(result), false),
        "wait" => return wait_text(result),
        "notify" => return tool_text("Note delivered to the parent agent.", false),
        "route" => {
            return tool_text(
                &format!(
                    "Routed {} to {} (ticket {}).",
                    result["triggerId"].as_str().unwrap_or(""),
                    result["agentName"].as_str().unwrap_or(""),
                    result["ticketId"].as_str().unwrap_or("none")
                ),
                false,
            );
        }
        "dismiss" => {
            return tool_text(
                &format!("Dismissed {}.", result["triggerId"].as_str().unwrap_or("")),
                false,
            );
        }
        "claim" => return claim_text(result),
        "release" => {
            return tool_text(
                &format!(
                    "Released {} claim(s).",
                    result["released"].as_u64().unwrap_or(0)
                ),
                false,
            );
        }
        _ => {}
    }
    if result["background"].as_bool() == Some(true) {
        return tool_text(
            &format!(
                "subagent {} ({}) started in the background (turn {}). Its answer is delivered to you as a new message when it finishes (no need to call subagent_wait); send more instructions with subagent_message.{}",
                result["sessionId"].as_str().unwrap_or(""),
                result["agentName"].as_str().unwrap_or(""),
                result["turnId"].as_str().unwrap_or(""),
                routed_note(result)
            ),
            false,
        );
    }
    let (text, is_error) = answer(result);
    tool_text(&format!("{text}{}", routed_note(result)), is_error)
}

/// " [Picked by the decision model: <agent>, model <m>: <reason>]" when Harness's decision model chose the
/// subagent of a `delegate` call that named none; empty otherwise.
fn routed_note(result: &Value) -> String {
    let route = &result["routedBy"];
    if route["decisionModel"].as_bool() != Some(true) {
        return String::new();
    }
    let mut note = "\n\n[Picked by the decision model".to_owned();
    if let Some(model) = route["model"].as_str().filter(|m| !m.is_empty()) {
        note.push_str(", model ");
        note.push_str(model);
    }
    if let Some(reason) = route["reason"].as_str().filter(|r| !r.is_empty()) {
        note.push_str(": ");
        note.push_str(reason);
    }
    note.push(']');
    note
}

/// Header line + answer of one finished subagent turn (blocking `delegate` and `subagent_wait`).
fn answer(result: &Value) -> (String, bool) {
    let outcome = result["outcome"].as_str().unwrap_or("failed");
    let mut text = format!(
        "subagent {} ({}) {outcome}",
        result["sessionId"].as_str().unwrap_or(""),
        result["agentName"].as_str().unwrap_or("")
    );
    if let Some(error) = result["error"].as_str().filter(|e| !e.is_empty()) {
        text.push_str(": ");
        text.push_str(error);
    }
    text.push_str("\n\n");
    text.push_str(result["text"].as_str().unwrap_or(""));
    (text, outcome != "done")
}

fn message_text(result: &Value) -> String {
    let id = result["sessionId"].as_str().unwrap_or("");
    if result["interrupted"].as_bool() == Some(true) {
        format!(
            "Stopped subagent {id}'s turn and sent the message. Its reply is delivered to you as a new message."
        )
    } else if result["queued"].as_bool() == Some(true) {
        format!(
            "Queued for subagent {id}; it runs after the current turn. Its reply is delivered to you as a new message."
        )
    } else {
        format!("Sent to subagent {id}. Its reply is delivered to you as a new message.")
    }
}

fn wait_text(result: &Value) -> Value {
    let mut out = String::from("Subagents:\n");
    for v in result["subagents"].as_array().into_iter().flatten() {
        let activity = v["activity"].as_str().unwrap_or("");
        let _ = writeln!(
            out,
            "- {} ({}) {}: {}{}",
            v["sessionId"].as_str().unwrap_or(""),
            v["agentName"].as_str().unwrap_or(""),
            v["title"].as_str().unwrap_or(""),
            v["status"].as_str().unwrap_or(""),
            if activity.is_empty() {
                String::new()
            } else {
                format!(" — {activity}")
            }
        );
    }
    let notes = result["notes"].as_array().cloned().unwrap_or_default();
    let finished = result["finished"].as_array().cloned().unwrap_or_default();
    if notes.is_empty() && finished.is_empty() {
        out.push_str("\nNo new answers or notes yet.");
    }
    for n in &notes {
        let _ = write!(
            out,
            "\nNote from {} ({}):\n{}\n",
            n["sessionId"].as_str().unwrap_or(""),
            n["title"].as_str().unwrap_or(""),
            n["text"].as_str().unwrap_or("")
        );
    }
    for f in &finished {
        out.push_str("\n---\n");
        out.push_str(&answer(f).0);
        out.push('\n');
    }
    tool_text(&out, false)
}

fn claim_text(result: &Value) -> Value {
    let list = |key: &str| result[key].as_array().cloned().unwrap_or_default();
    let mut out = String::new();
    let granted: Vec<String> = list("granted")
        .iter()
        .filter_map(|p| p.as_str().map(str::to_owned))
        .collect();
    if !granted.is_empty() {
        let _ = writeln!(out, "Granted: {}", granted.join(", "));
    }
    let conflicts = list("conflicts");
    for c in &conflicts {
        let _ = writeln!(
            out,
            "CONFLICT: {} is held by {} ({}, {}) via {}{} — do not edit it; coordinate first.",
            c["requested"].as_str().unwrap_or(""),
            c["sessionId"].as_str().unwrap_or(""),
            c["title"].as_str().unwrap_or(""),
            c["agent"].as_str().unwrap_or(""),
            c["path"].as_str().unwrap_or(""),
            c["note"]
                .as_str()
                .filter(|n| !n.is_empty())
                .map(|n| format!(": {n}"))
                .unwrap_or_default()
        );
    }
    out.push_str("Current claims in this workspace:\n");
    let claims = list("claims");
    if claims.is_empty() {
        out.push_str("(none)\n");
    }
    for c in &claims {
        let _ = writeln!(
            out,
            "- {} — {} ({})",
            c["path"]
                .as_str()
                .map_or("", |p| if p.is_empty() { "(whole workspace)" } else { p }),
            c["title"].as_str().unwrap_or(""),
            c["sessionId"].as_str().unwrap_or("")
        );
    }
    tool_text(&out, !conflicts.is_empty())
}

/// Sends one request line and reads one reply line, emitting progress while waiting.
fn exchange(
    env: &DelegateEnv,
    request: &Value,
    out: &Out,
    progress: &Value,
) -> std::io::Result<Value> {
    let mut stream = TcpStream::connect(&env.addr)?;
    stream.write_all(format!("{request}\n").as_bytes())?;
    stream.flush()?;
    stream.set_read_timeout(Some(PROGRESS_EVERY))?;
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    let mut ticks: u64 = 0;
    loop {
        match reader.read_until(b'\n', &mut line) {
            Ok(_) if line.ends_with(b"\n") => break,
            Ok(0) => break,
            Ok(_) => {}
            // 读超时只是“仍在运行”，已读到的字节保留在 line 中，继续等待。
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                ticks += 1;
                if !progress.is_null() {
                    write(
                        out,
                        &json!({"jsonrpc":"2.0","method":"notifications/progress","params":{
                        "progressToken": progress, "progress": ticks, "message": "subagent working"}}),
                    );
                }
            }
            Err(e) => return Err(e),
        }
    }
    serde_json::from_slice(&line).map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_tool_is_always_loaded_for_agy() {
        for tools in [
            super::tool_list(false),
            super::tool_list(true),
            super::router_tools(),
        ] {
            let tools = super::always_loaded(tools);
            for tool in tools.as_array().into_iter().flatten() {
                assert_eq!(tool["_meta"]["antigravity/alwaysLoad"], true, "{tool}");
            }
        }
    }

    #[test]
    fn a_routed_delegation_says_who_picked_and_why() {
        use serde_json::json;
        let routed = json!({"routedBy": {"decisionModel": true, "agent": "x", "model": "worker/strong", "reason": "review work"}});
        assert_eq!(
            super::routed_note(&routed),
            "\n\n[Picked by the decision model, model worker/strong: review work]"
        );
        assert_eq!(super::routed_note(&json!({"sessionId": "s"})), "");
    }
}
