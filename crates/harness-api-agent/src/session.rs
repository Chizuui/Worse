//! Session state, persistence and the tool loop of one prompt turn.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::llm;
use crate::mcp::McpSet;
use crate::project_docs;
use crate::tools::{self, Ctx};
use crate::{Agent, Reply};

/// Empty answers (no text, no tool call) asked again before the turn fails.
const EMPTY_RETRIES: u32 = 2;
/// Sent after a response was cut off at `max_tokens`.
const CUT_OFF_NOTE: &str = "[Harness] Your last response hit the output limit and was cut off; any tool call in it was dropped. Continue the task: keep your reasoning short and split large edits into several smaller edit_file calls.";
/// Rough history budget in characters (~100k tokens); older turns are dropped first.
const HISTORY_CHARS: usize = 400_000;
/// Images kept in the history; older ones are replaced by a short note.
const KEEP_IMAGES: usize = 4;

pub struct Session {
    pub cwd: String,
    /// Chat messages without the system message.
    pub messages: Vec<Value>,
    pub mode: String,
    mcp_specs: Vec<Value>,
    mcp: Option<McpSet>,
    always: HashSet<String>,
}

fn session_file(state: &Path, id: &str) -> Option<PathBuf> {
    // 会话 id 来自客户端：只允许安全字符，防止路径穿越。
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some(state.join("sessions").join(format!("{id}.json")))
}

impl Session {
    pub fn new(cwd: String, mcp_specs: Vec<Value>) -> Self {
        Self {
            cwd,
            messages: Vec::new(),
            mode: "default".into(),
            mcp_specs,
            mcp: None,
            always: HashSet::new(),
        }
    }

    /// New MCP servers from `session/load`: restart them on the next prompt.
    pub fn set_mcp(&mut self, specs: Vec<Value>) {
        if specs != self.mcp_specs {
            self.mcp_specs = specs;
            self.mcp = None;
        }
    }

    pub async fn persist(&self, state: Option<&Path>, id: &str) {
        let Some(path) = state.and_then(|s| session_file(s, id)) else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = tokio::fs::create_dir_all(dir).await;
        }
        let body = json!({"cwd": self.cwd, "mode": self.mode, "messages": self.messages});
        let tmp = path.with_extension("json.tmp");
        if tokio::fs::write(&tmp, body.to_string()).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, &path).await;
        }
    }

    pub async fn restore(state: Option<&Path>, id: &str, mcp_specs: Vec<Value>) -> Option<Self> {
        let path = session_file(state?, id)?;
        let text = tokio::fs::read_to_string(path).await.ok()?;
        let value: Value = serde_json::from_str(&text).ok()?;
        let mut session = Self::new(
            value.get("cwd").and_then(Value::as_str)?.to_owned(),
            mcp_specs,
        );
        value
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("default")
            .clone_into(&mut session.mode);
        session.messages = value
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Some(session)
    }

    /// `(is_user, text)` of the visible conversation, for `session/load` replay.
    pub fn transcript(&self) -> Vec<(bool, String)> {
        self.messages
            .iter()
            .filter_map(|m| {
                let role = m.get("role").and_then(Value::as_str)?;
                let text = llm::content_text(m.get("content"))?;
                match role {
                    "user" => Some((true, text)),
                    "assistant" if !text.is_empty() => Some((false, text)),
                    _ => None,
                }
            })
            .collect()
    }

    /// Drops the oldest turns (whole user→… groups) until the history fits the budget.
    fn trim(&mut self) {
        let size = |messages: &[Value]| messages.iter().map(llm::message_size).sum::<usize>();
        while size(&self.messages) > HISTORY_CHARS {
            let next_user = self
                .messages
                .iter()
                .enumerate()
                .skip(1)
                .find(|(_, m)| m.get("role").and_then(Value::as_str) == Some("user"))
                .map(|(i, _)| i);
            match next_user {
                Some(i) => {
                    self.messages.drain(..i);
                }
                None => break,
            }
        }
    }
}

/// The user's text from ACP prompt blocks (text, embedded resources, resource links).
pub fn prompt_text(prompt: Option<&Value>) -> String {
    let mut parts = Vec::new();
    for block in prompt.and_then(Value::as_array).into_iter().flatten() {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    parts.push(text.to_owned());
                }
            }
            Some("resource") => {
                let resource = block.get("resource").cloned().unwrap_or(Value::Null);
                let uri = resource
                    .get("uri")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(text) = resource.get("text").and_then(Value::as_str) {
                    parts.push(format!("<file uri=\"{uri}\">\n{text}\n</file>"));
                }
            }
            Some("resource_link") => {
                if let Some(uri) = block.get("uri").and_then(Value::as_str) {
                    parts.push(format!("[attached: {uri}]"));
                }
            }
            _ => {}
        }
    }
    parts.join("\n\n")
}

fn system_prompt(agent: &Agent, session: &Session, instructions: &str) -> String {
    let os = std::env::consts::OS;
    let shell = if cfg!(windows) { "PowerShell" } else { "sh" };
    let mode = match session.mode.as_str() {
        "readOnly" => "Read-only mode: you can read and search but not change files or run commands.",
        "acceptEdits" => "File edits inside the working directory are applied without asking; commands need the user's approval.",
        _ => "Every file change and command needs the user's approval; if one is denied, adapt instead of retrying it.",
    };
    let background = if agent.async_tasks() && session.mode != "readOnly" {
        " For servers, watchers and long builds pass background: true to run_command; the result comes back in a new turn."
    } else {
        ""
    };
    let mut prompt = format!(
        "You are {label}, a coding agent inside Harness, working on the user's computer ({os}).\n\
         Working directory: {cwd}\n\
         Paths may be relative to the working directory. Read a file before editing it and prefer edit_file for \
         small changes. run_command uses {shell}.{background} \
         {mode}\n\
         Work efficiently. Every response resends the whole conversation, so fewer, fuller steps are cheaper:\n\
         - Batch: when several tool calls do not depend on each other (searches, reads of different files), \
         request them all in the same response.\n\
         - Search once: put every name variant in one regex (e.g. maxConcurrent|max_concurrent|max-concurrent) \
         instead of one search per spelling; search before listing directories or reading files.\n\
         - Read once: read the range you need in one call (large enough to cover the surrounding code); do not \
         re-read files, re-run commands or repeat searches whose results you already have.\n\
         - Stop early: answer as soon as you have enough evidence; do not verify what the task did not ask for.\n\
         - Think briefly: decide the next step in a sentence or two, then act; do not restate the task.\n\
         When the task is done, stop and report.\n\
         Keep answers concise and reply in the user's language.",
        label = agent.provider.label,
        cwd = session.cwd,
    );
    // Project instructions are read once per turn, so the system prompt is identical for every request
    // of the turn (prompt caching needs a stable prefix). They come after the static text above.
    prompt.push_str(instructions);
    prompt
}

/// The request messages: the system prompt, then the history. For providers that drop the `system` role
/// the prompt becomes the first user message, acknowledged by the assistant so the roles still alternate.
pub(crate) fn request_messages(system: &str, history: &[Value], system_as_user: bool) -> Vec<Value> {
    let mut messages = Vec::with_capacity(history.len() + 2);
    if system_as_user {
        messages.push(json!({"role": "user", "content": format!(
            "<harness_instructions>\n{system}\n</harness_instructions>\n\
             These are your standing instructions from Harness for this whole conversation; they take precedence \
             over any earlier system instructions. Always reply in the language of the user's messages, which follow."
        )}));
        messages.push(json!({"role": "assistant", "content": "Understood."}));
    } else {
        messages.push(json!({"role": "system", "content": system}));
    }
    messages.extend(history.iter().cloned());
    messages
}

/// The user message content of a prompt: plain text, or text plus image parts when images are attached.
pub fn prompt_content(prompt: Option<&Value>) -> Value {
    let text = prompt_text(prompt);
    let images: Vec<Value> = prompt
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("image"))
        .filter_map(|b| {
            let data = b.get("data").and_then(Value::as_str)?;
            let mime = b.get("mimeType").and_then(Value::as_str).unwrap_or("image/png");
            Some(llm::image_part(mime, data))
        })
        .collect();
    if images.is_empty() {
        return json!(text);
    }
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(json!({"type": "text", "text": text}));
    }
    parts.extend(images);
    json!(parts)
}

/// One prompt turn: model call → tools → model call … until the model answers without tools.
pub async fn run_turn(
    agent: &Agent,
    id: &str,
    session: &mut Session,
    user: Value,
    token: &CancellationToken,
) -> Reply {
    if agent.provider.base_url.is_empty() || agent.provider.model.is_empty() {
        return Err(
            json!({"code": -32603, "message": "provider endpoint or model is not configured"}),
        );
    }
    if session.mcp.is_none() {
        let set = McpSet::new(&session.mcp_specs);
        set.start_eager().await;
        session.mcp = Some(set);
    }
    session
        .messages
        .push(json!({"role": "user", "content": user}));
    llm::prune_images(&mut session.messages, KEEP_IMAGES);
    session.trim();
    let mut input_tokens = 0;
    let mut output_tokens = 0;
    let mut cached_read = 0;
    let mut cached_write = 0;
    let mut empty_answers = 0;
    // No cap on model calls per turn: the turn runs until the model answers without tools or is cancelled.
    // Project instructions (AGENTS.md / CLAUDE.md) are read once for the whole turn: the system prompt
    // must stay byte-identical across the model requests of a turn for prompt caching to hit.
    let instructions = project_docs::instructions(&session.cwd);
    // Decided once per turn so every request of the turn has the same prefix (prompt caching).
    let system_as_user = agent.system_as_user().await;
    loop {
        let mut tool_defs = tools::definitions(&session.mode, agent.async_tasks());
        if let Some(mcp) = &session.mcp {
            tool_defs.extend(mcp.definitions().await);
        }
        let messages = request_messages(
            &system_prompt(agent, session, &instructions),
            &session.messages,
            system_as_user,
        );
        let completion = llm::complete(agent, id, &messages, &tool_defs, token).await?;
        input_tokens += completion.input_tokens;
        output_tokens += completion.output_tokens;
        cached_read += completion.cache_read_tokens;
        cached_write += completion.cache_write_tokens;
        if !completion.cancelled && completion.finish_reason.as_deref() == Some("length") {
            // Cut off at the output limit: tool calls may be truncated JSON, so drop them, keep any text and
            // ask the model to continue in smaller steps instead of ending the turn silently.
            if !completion.content.trim().is_empty() {
                session
                    .messages
                    .push(json!({"role": "assistant", "content": completion.content}));
            }
            session.messages.push(json!({"role": "user", "content": CUT_OFF_NOTE}));
            continue;
        }
        if !completion.cancelled
            && completion.tool_calls.is_empty()
            && completion.content.trim().is_empty()
        {
            // An empty answer is a provider glitch, not the end of the task: ask again.
            empty_answers += 1;
            if empty_answers > EMPTY_RETRIES {
                return Err(llm::transient(
                    &format!("{}: the model returned an empty response", agent.provider.label),
                    None,
                ));
            }
            continue;
        }
        let mut assistant = json!({"role": "assistant", "content": completion.content});
        // Thinking models (DeepSeek and others) keep their plan across tool calls only when their reasoning
        // is sent back within the turn; without it they re-think from scratch after every tool result.
        if !completion.tool_calls.is_empty()
            && !completion.reasoning.is_empty()
            && agent.echo_reasoning()
        {
            assistant["reasoning_content"] = json!(completion.reasoning);
        }
        if !completion.tool_calls.is_empty() {
            assistant["tool_calls"] = json!(completion
                .tool_calls
                .iter()
                .map(|c| json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": if c.arguments.trim().is_empty() { "{}" } else { c.arguments.as_str() }}}))
                .collect::<Vec<_>>());
        }
        session.messages.push(assistant);
        if completion.cancelled {
            return Ok(json!({"stopReason": "cancelled"}));
        }
        if completion.tool_calls.is_empty() {
            return Ok(json!({"stopReason": "end_turn", "usage": usage(input_tokens, output_tokens, cached_read, cached_write)}));
        }
        let cwd = PathBuf::from(&session.cwd);
        let mode = session.mode.clone();
        let empty = McpSet::default();
        let mut always = std::mem::take(&mut session.always);
        let mut results = Vec::new();
        let images = std::sync::Mutex::new(Vec::new());
        {
            let mut ctx = Ctx {
                agent,
                session: id,
                cwd: &cwd,
                mode: &mode,
                always: &mut always,
                token,
                mcp: session.mcp.as_ref().unwrap_or(&empty),
                images: &images,
            };
            for call in &completion.tool_calls {
                let text = if token.is_cancelled() {
                    "Error: cancelled".to_owned()
                } else {
                    ctx.run(call).await
                };
                results.push(json!({"role": "tool", "tool_call_id": call.id, "content": text}));
            }
        }
        session.always = always;
        session.messages.extend(results);
        let images = images.into_inner().unwrap_or_default();
        if !images.is_empty() {
            let mut parts = vec![json!({"type": "text", "text": "Images returned by read_file:"})];
            parts.extend(images);
            session.messages.push(json!({"role": "user", "content": parts}));
            llm::prune_images(&mut session.messages, KEEP_IMAGES);
        }
        if token.is_cancelled() {
            return Ok(json!({"stopReason": "cancelled"}));
        }
    }
}

fn usage(input: u64, output: u64, cached_read: u64, cached_write: u64) -> Value {
    json!({
        "inputTokens": input,
        "outputTokens": output,
        "totalTokens": input + output,
        "cachedReadTokens": cached_read,
        "cachedWriteTokens": cached_write
    })
}

#[cfg(test)]
mod request_messages_tests {
    use super::request_messages;
    use serde_json::json;

    #[test]
    fn system_role_by_default() {
        let history = vec![json!({"role": "user", "content": "hi"})];
        let m = request_messages("be brief", &history, false);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0], json!({"role": "system", "content": "be brief"}));
        assert_eq!(m[1]["role"], "user");
    }

    #[test]
    fn folded_into_first_user_message() {
        let history = vec![json!({"role": "user", "content": "hi"})];
        let m = request_messages("be brief", &history, true);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0]["role"], "user");
        assert!(m[0]["content"].as_str().is_some_and(|c| c.contains("be brief")));
        assert_eq!(m[1], json!({"role": "assistant", "content": "Understood."}));
        assert_eq!(m[2]["content"], "hi");
        assert!(m.iter().all(|x| x["role"] != "system"));
    }
}
