//! One streamed `POST {base}/chat/completions` call (OpenAI-compatible) and the error mapping of §9.7.

use std::time::Duration;

use futures_util::StreamExt as _;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::Agent;

/// No byte from the provider for this long ends the call as a transient error.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// A `Retry-After` up to this many seconds is a short rate limit (transient), above it the quota is gone.
const SHORT_RETRY_SECS: u64 = 120;
const BODY_PREVIEW: usize = 500;

pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Default)]
pub struct Completion {
    pub content: String,
    /// Streamed `reasoning_content` / `reasoning` text (thinking models).
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    /// `choices[0].finish_reason` of the last chunk that had one (`stop`, `tool_calls`, `length`, …).
    pub finish_reason: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Prompt tokens served from the provider's cache.
    pub cache_read_tokens: u64,
    /// Prompt tokens written into the provider's cache.
    pub cache_write_tokens: u64,
    pub cancelled: bool,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn transient(message: &str, retry_after: Option<u64>) -> Value {
    let mut data = json!({"harnessErrorKind": "transient"});
    if let Some(secs) = retry_after {
        data["retryAfterSecs"] = json!(secs);
    }
    json!({"code": -32603, "message": message, "data": data})
}

fn quota(label: &str, retry_after: Option<u64>) -> Value {
    let resets_at = now_secs() + retry_after.unwrap_or(3600);
    json!({
        "code": -32603,
        "message": format!("{label} usage limit reached"),
        "data": {"harnessErrorKind": "quota_exhausted", "resetsAt": resets_at}
    })
}

/// Body preview with the API key scrubbed (a proxy may echo request headers).
fn preview(agent: &Agent, body: &str) -> String {
    let mut text: String = body.chars().take(BODY_PREVIEW).collect();
    let key = agent.provider.api_key.as_str();
    if key.len() >= 8 {
        text = text.replace(key, "***");
    }
    text.trim().to_owned()
}

/// Maps a non-success HTTP status to an ACP error (phase-9-parity.md §9.7).
fn status_error(agent: &Agent, status: u16, retry_after: Option<u64>, body: &str) -> Value {
    let label = &agent.provider.label;
    match status {
        401 | 403 => json!({
            "code": -32000,
            "message": format!("Authentication required: {label} rejected the API key (HTTP {status})"),
            "data": {"detail": preview(agent, body)}
        }),
        402 => quota(label, retry_after),
        429 => match retry_after {
            Some(secs) if secs <= SHORT_RETRY_SECS => {
                transient(&format!("{label} is rate limited (HTTP 429)"), Some(secs))
            }
            _ => quota(label, retry_after),
        },
        500..=599 => transient(
            &format!(
                "{label} server error (HTTP {status}): {}",
                preview(agent, body)
            ),
            None,
        ),
        _ => json!({
            "code": -32603,
            "message": format!("{label} request failed (HTTP {status}): {}", preview(agent, body))
        }),
    }
}

/// `(cache read, cache write)` tokens of one `usage` object, across provider spellings.
pub fn cache_tokens(usage: &Value) -> (u64, u64) {
    let n = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
    let read = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .max(n("prompt_cache_hit_tokens"))
        .max(n("cache_read_input_tokens"));
    let write = n("cache_creation_input_tokens");
    (read, write)
}

/// Accumulates streamed deltas into a [`Completion`] and forwards text to the client.
struct Accumulator<'a> {
    agent: &'a Agent,
    session: &'a str,
    out: Completion,
    calls: Vec<(String, String, String)>,
}

impl Accumulator<'_> {
    fn text(&mut self, kind: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        if kind == "agent_message_chunk" {
            self.out.content.push_str(text);
        } else {
            self.out.reasoning.push_str(text);
        }
        self.agent.update(
            self.session,
            &json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}}),
        );
    }

    fn usage(&mut self, usage: Option<&Value>) {
        let Some(usage) = usage else { return };
        let n = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
        self.out.input_tokens = self.out.input_tokens.max(n("prompt_tokens"));
        self.out.output_tokens = self.out.output_tokens.max(n("completion_tokens"));
        let (read, write) = cache_tokens(usage);
        self.out.cache_read_tokens = self.out.cache_read_tokens.max(read);
        self.out.cache_write_tokens = self.out.cache_write_tokens.max(write);
    }

    fn tool_delta(&mut self, call: &Value) {
        let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
        let index = match call.get("index").and_then(Value::as_u64) {
            Some(i) => usize::try_from(i).unwrap_or(usize::MAX),
            // 部分兼容服务不带 index：新 id 视为新调用，否则续写最后一个。
            None if !id.is_empty() && self.calls.last().map_or(true, |c| c.0 != id) => {
                self.calls.len()
            }
            None => self.calls.len().saturating_sub(1),
        };
        if index == usize::MAX {
            return;
        }
        while self.calls.len() <= index {
            self.calls
                .push((String::new(), String::new(), String::new()));
        }
        let slot = &mut self.calls[index];
        if !id.is_empty() {
            id.clone_into(&mut slot.0);
        }
        if let Some(f) = call.get("function") {
            if let Some(name) = f.get("name").and_then(Value::as_str) {
                slot.1.push_str(name);
            }
            if let Some(args) = f.get("arguments") {
                match args {
                    Value::String(s) => slot.2.push_str(s),
                    Value::Null => {}
                    other => slot.2.push_str(&other.to_string()),
                }
            }
        }
    }

    /// One parsed chunk (streamed) or the whole response (non-streamed).
    fn chunk(&mut self, value: &Value) -> Result<(), Value> {
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| error.to_string(), str::to_owned);
            return Err(
                json!({"code": -32603, "message": format!("{}: {}", self.agent.provider.label, preview(self.agent, &message))}),
            );
        }
        self.usage(value.get("usage"));
        let Some(choice) = value.pointer("/choices/0") else {
            return Ok(());
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.out.finish_reason = Some(reason.to_owned());
        }
        let delta = choice.get("delta").or_else(|| choice.get("message"));
        let Some(delta) = delta else { return Ok(()) };
        for key in ["reasoning_content", "reasoning"] {
            if let Some(text) = delta.get(key).and_then(Value::as_str) {
                self.text("agent_thought_chunk", text);
            }
        }
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            self.text("agent_message_chunk", text);
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                self.tool_delta(call);
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Completion {
        for (i, (id, name, arguments)) in self.calls.into_iter().enumerate() {
            if name.is_empty() {
                continue;
            }
            self.out.tool_calls.push(ToolCall {
                id: if id.is_empty() {
                    format!("call_{i}_{}", now_secs())
                } else {
                    id
                },
                name,
                arguments,
            });
        }
        self.out
    }
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

enum Sent {
    Ok(reqwest::Response),
    Cancelled,
    /// HTTP status, `Retry-After` seconds, body.
    Status(u16, Option<u64>, String),
}

async fn send(
    agent: &Agent,
    messages: &[Value],
    tools: &[Value],
    token: &CancellationToken,
) -> Result<Sent, Value> {
    let provider = &agent.provider;
    let mut body = json!({
        "model": provider.model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true}
    });
    if provider.max_tokens > 0 {
        body["max_tokens"] = json!(provider.max_tokens);
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    let mut request = agent
        .http
        .post(format!("{}/chat/completions", provider.base_url))
        .json(&body);
    if !provider.api_key.is_empty() {
        request = request.bearer_auth(&provider.api_key);
    }
    let response = tokio::select! {
        () = token.cancelled() => return Ok(Sent::Cancelled),
        sent = request.send() => sent.map_err(|e| transient(&format!("{}: cannot reach {} ({})", provider.label, provider.base_url, e.without_url()), None))?,
    };
    if response.status().is_success() {
        return Ok(Sent::Ok(response));
    }
    let status = response.status().as_u16();
    let after = retry_after(response.headers());
    let text = response.text().await.unwrap_or_default();
    Ok(Sent::Status(status, after, text))
}

/// Asks the provider whether it passes the `system` message to the model: the code word lives only in the
/// system message, so a model that never saw it cannot repeat it. `None` when the probe itself failed.
pub async fn probe_system_role(agent: &Agent) -> Option<bool> {
    let provider = &agent.provider;
    let word: String = uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .filter(char::is_ascii_alphabetic)
        .take(6)
        .collect::<String>()
        .to_ascii_uppercase();
    let word = format!("HX{word}");
    let mut body = json!({
        "model": provider.model,
        "messages": [
            {"role": "system", "content": format!("The code word is {word}. When asked for the code word, reply with it and nothing else.")},
            {"role": "user", "content": "What is the code word?"}
        ],
        "stream": false
    });
    // Thinking models spend part of the budget on reasoning before the answer.
    body["max_tokens"] = json!(2048);
    let mut request = agent
        .http
        .post(format!("{}/chat/completions", provider.base_url))
        .timeout(Duration::from_secs(90))
        .json(&body);
    if !provider.api_key.is_empty() {
        request = request.bearer_auth(&provider.api_key);
    }
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let value: Value = response.json().await.ok()?;
    let content = value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)?;
    if content.trim().is_empty() {
        return None;
    }
    Some(content.to_ascii_uppercase().contains(&word))
}

/// `messages` without any `reasoning_content` field.
pub fn strip_reasoning(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| {
            let mut m = m.clone();
            if let Some(object) = m.as_object_mut() {
                object.remove("reasoning_content");
            }
            m
        })
        .collect()
}

/// The text of a message's `content` (string, or the text parts of an array).
pub fn content_text(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(""),
        ),
        _ => None,
    }
}

const IMAGE_NOTE: &str = "[image attached, but this model cannot view images; say so instead of guessing its content]";
const IMAGE_GONE: &str = "[earlier image removed from context]";
const IMAGE_COST: usize = 1500;

/// An OpenAI-style image part from base64 data.
pub fn image_part(mime: &str, data: &str) -> Value {
    json!({"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{data}")}})
}

fn is_image(part: &Value) -> bool {
    part.get("type").and_then(Value::as_str) == Some("image_url")
}

fn image_parts(message: &Value) -> impl Iterator<Item = &Value> {
    message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|p| is_image(p))
}

pub fn has_images(messages: &[Value]) -> bool {
    messages.iter().any(|m| image_parts(m).next().is_some())
}

/// Serialized size of a message for the history budget; an image counts as a small fixed cost.
pub fn message_size(message: &Value) -> usize {
    let data: usize = image_parts(message)
        .map(|p| p["image_url"]["url"].as_str().map_or(0, str::len))
        .sum();
    let count = image_parts(message).count();
    message.to_string().len().saturating_sub(data) + count * IMAGE_COST
}

fn replace_images(message: &mut Value, note: &str) {
    let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    if !parts.iter().any(is_image) {
        return;
    }
    let text = parts
        .iter()
        .map(|p| {
            if is_image(p) {
                note.to_owned()
            } else {
                p.get("text").and_then(Value::as_str).unwrap_or_default().to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    message["content"] = json!(text);
}

/// `messages` with every image replaced by a text note (for models that reject image input).
pub fn strip_images(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| {
            let mut m = m.clone();
            replace_images(&mut m, IMAGE_NOTE);
            m
        })
        .collect()
}

/// Keeps images only in the newest messages that carry them, up to `keep` images.
pub fn prune_images(messages: &mut [Value], keep: usize) {
    let mut seen = 0;
    for message in messages.iter_mut().rev() {
        let count = image_parts(message).count();
        if count == 0 {
            continue;
        }
        if seen >= keep {
            replace_images(message, IMAGE_GONE);
        }
        seen += count;
    }
}

fn image_rejection(status: u16, body: &str) -> bool {
    if !matches!(status, 400 | 404 | 415 | 422) {
        return false;
    }
    let body = body.to_ascii_lowercase();
    ["image", "vision", "modal"].iter().any(|w| body.contains(w))
}

/// One text part carrying an Anthropic-compatible cache breakpoint.
fn marked_part(text: &str) -> Value {
    json!({"type": "text", "text": text, "cache_control": {"type": "ephemeral"}})
}

/// A message that can hold a cache breakpoint: not the system message, with content to mark.
fn markable(message: &Value) -> bool {
    if message.get("role").and_then(Value::as_str) == Some("system") {
        return false;
    }
    match message.get("content") {
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(parts)) => parts.last().is_some_and(Value::is_object),
        _ => false,
    }
}

/// `messages` with prompt-cache breakpoints (a copy; the input is untouched):
/// one on the system message (the stable prefix) and one on the last non-system message.
pub fn with_cache_breakpoints(messages: &[Value]) -> Vec<Value> {
    let mut out = messages.to_vec();
    if let Some(system) = out
        .iter_mut()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("system"))
    {
        if let Some(text) = content_text(system.get("content")) {
            if !text.is_empty() {
                system["content"] = json!([marked_part(&text)]);
            }
        }
    }
    // An assistant message with empty content cannot hold a breakpoint: fall back to the one before it.
    if let Some(i) = out.iter().rposition(markable) {
        let message = &mut out[i];
        match message.get("content") {
            Some(Value::String(text)) => {
                let text = text.clone();
                message["content"] = json!([marked_part(&text)]);
            }
            Some(Value::Array(parts)) => {
                let mut parts = parts.clone();
                if let Some(last) = parts.last_mut().and_then(Value::as_object_mut) {
                    last.insert("cache_control".into(), json!({"type": "ephemeral"}));
                }
                message["content"] = Value::Array(parts);
            }
            _ => {}
        }
    }
    out
}

/// Streams one completion. Text and reasoning are forwarded as ACP updates while they arrive.
pub async fn complete(
    agent: &Agent,
    session: &str,
    messages: &[Value],
    tools: &[Value],
    token: &CancellationToken,
) -> Result<Completion, Value> {
    let provider = &agent.provider;
    let echoed = messages.iter().any(|m| m.get("reasoning_content").is_some());
    // Prompt-cache breakpoints, dropped for good if the provider rejects them (HTTP 400). Reasoning echoes
    // are dropped the same way (older reasoners reject `reasoning_content` in the input).
    let mut marked = agent.cache_control() && provider.prompt_cache;
    let mut echoing = agent.echo_reasoning();
    let pictured = has_images(messages);
    let mut seeing = agent.vision();
    let mut sends = 0_u8;
    let response = loop {
        sends += 1;
        // Two independent steps: history without `reasoning_content`, then the cache markers on top.
        let base = if !echoing && echoed {
            strip_reasoning(messages)
        } else {
            messages.to_vec()
        };
        let base = if pictured && !seeing {
            strip_images(&base)
        } else {
            base
        };
        let payload = if marked {
            with_cache_breakpoints(&base)
        } else {
            base
        };
        match send(agent, &payload, tools, token).await? {
            Sent::Ok(response) => break response,
            Sent::Cancelled => {
                return Ok(Completion { cancelled: true, ..Completion::default() })
            }
            Sent::Status(status, _, ref text)
                if pictured && seeing && sends < 5 && image_rejection(status, text) =>
            {
                agent.disable_vision();
                seeing = false;
            }
            Sent::Status(400, _, _) if sends < 4 && (marked || (echoing && echoed)) => {
                if marked {
                    agent.disable_cache_control();
                    marked = false;
                } else {
                    agent.disable_echo_reasoning();
                    echoing = false;
                }
            }
            Sent::Status(400 | 415 | 422, _, _) if pictured && seeing && sends < 5 => {
                agent.disable_vision();
                seeing = false;
            }
            Sent::Status(status, after, text) => {
                return Err(status_error(agent, status, after, &text))
            }
        }
    };
    let mut acc = Accumulator {
        agent,
        session,
        out: Completion::default(),
        calls: Vec::new(),
    };
    let mut stream = response.bytes_stream();
    let mut buffer: Vec<u8> = Vec::new();
    let mut raw = String::new();
    let mut saw_data = false;
    loop {
        let next = tokio::select! {
            () = token.cancelled() => {
                let mut out = acc.finish();
                out.cancelled = true;
                out.tool_calls.clear();
                return Ok(out);
            }
            next = tokio::time::timeout(IDLE_TIMEOUT, stream.next()) => next,
        };
        let bytes = match next {
            Err(_) => {
                return Err(transient(
                    &format!("{}: no data for 5 minutes", provider.label),
                    None,
                ))
            }
            Ok(None) => break,
            Ok(Some(Err(e))) => {
                return Err(transient(
                    &format!("{}: stream broken ({})", provider.label, e.without_url()),
                    None,
                ))
            }
            Ok(Some(Ok(bytes))) => bytes,
        };
        buffer.extend_from_slice(&bytes);
        while let Some(pos) = buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buffer.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim();
            if let Some(data) = line.strip_prefix("data:") {
                saw_data = true;
                let data = data.trim();
                if data == "[DONE]" {
                    return Ok(acc.finish());
                }
                if let Ok(value) = serde_json::from_str::<Value>(data) {
                    acc.chunk(&value)?;
                }
            } else if !saw_data && !line.starts_with(':') && !line.starts_with("event:") {
                raw.push_str(line);
            }
        }
    }
    if !saw_data {
        // 服务忽略了 stream=true，直接返回整段 JSON。
        raw.push_str(String::from_utf8_lossy(&buffer).trim());
        let value: Value = serde_json::from_str(&raw).map_err(|_| {
            json!({"code": -32603, "message": format!("{}: unexpected response: {}", provider.label, preview(agent, &raw))})
        })?;
        acc.chunk(&value)?;
    } else if acc.out.finish_reason.is_none() {
        // The stream closed without `[DONE]` or a finish reason: the response is cut off, not complete.
        return Err(transient(
            &format!(
                "{}: stream ended before the response finished",
                provider.label
            ),
            None,
        ));
    }
    Ok(acc.finish())
}
#[cfg(test)]
mod tests {
    use super::*;

    fn messages() -> Vec<Value> {
        vec![
            json!({"role": "system", "content": "be brief"}),
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": ""}),
        ]
    }

    #[test]
    fn system_message_gets_breakpoint() {
        let out = with_cache_breakpoints(&messages());
        assert_eq!(
            out[0]["content"],
            json!([{"type": "text", "text": "be brief", "cache_control": {"type": "ephemeral"}}])
        );
    }

    #[test]
    fn exactly_one_non_system_breakpoint() {
        let out = with_cache_breakpoints(&messages());
        let marked = out
            .iter()
            .skip(1)
            .filter(|m| m.to_string().contains("cache_control"))
            .count();
        assert_eq!(marked, 1);
        // The empty assistant message is skipped: the user message before it holds the breakpoint.
        assert_eq!(
            out[1]["content"],
            json!([{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}])
        );
    }

    #[test]
    fn array_content_marks_last_part() {
        let out = with_cache_breakpoints(&[
            json!({"role": "user", "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]}),
            json!({"role": "assistant", "content": null}),
        ]);
        assert!(out[0]["content"][0].get("cache_control").is_none());
        assert_eq!(out[0]["content"][1]["cache_control"], json!({"type": "ephemeral"}));
    }

    #[test]
    fn input_slice_is_unchanged() {
        let input = messages();
        let before = json!(input);
        let _ = with_cache_breakpoints(&input);
        assert_eq!(json!(input), before);
    }

    #[test]
    fn cache_tokens_parses_provider_spellings() {
        assert_eq!(
            cache_tokens(&json!({"prompt_tokens_details": {"cached_tokens": 12}})),
            (12, 0)
        );
        assert_eq!(cache_tokens(&json!({"prompt_cache_hit_tokens": 34})), (34, 0));
        assert_eq!(
            cache_tokens(&json!({"cache_read_input_tokens": 56, "cache_creation_input_tokens": 7})),
            (56, 7)
        );
        assert_eq!(cache_tokens(&json!({"prompt_tokens": 3})), (0, 0));
    }

    #[test]
    fn images_strip_to_plain_text() {
        let m = vec![json!({"role": "user", "content": [
            {"type": "text", "text": "look"}, image_part("image/png", "AAAA")]})];
        assert!(has_images(&m));
        let out = strip_images(&m);
        assert!(!has_images(&out));
        assert!(out[0]["content"].as_str().is_some_and(|c| c.starts_with("look") && c.contains("cannot view")));
        assert!(has_images(&m));
    }

    #[test]
    fn prune_keeps_newest_images() {
        let img = |t: &str| json!({"role": "user", "content": [{"type": "text", "text": t}, image_part("image/png", "AAAA")]});
        let mut m = vec![img("a"), img("b"), img("c")];
        prune_images(&mut m, 2);
        assert!(m[0]["content"].is_string());
        assert!(m[1]["content"].is_array() && m[2]["content"].is_array());
    }

    #[test]
    fn image_size_is_fixed_cost() {
        let big = "A".repeat(500_000);
        let m = json!({"role": "user", "content": [image_part("image/png", &big)]});
        assert!(message_size(&m) < 5_000);
    }

    #[test]
    fn rejection_needs_image_hint() {
        assert!(image_rejection(400, "unknown variant `image_url`, expected `text`"));
        assert!(!image_rejection(400, "cache_control is not supported"));
        assert!(!image_rejection(500, "image"));
    }
}
