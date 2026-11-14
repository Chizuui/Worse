//! `harness-fake-agent --name <name>`: a scripted ACP agent for gates (spec phase-8-core.md §13).
//!
//! Environment: `FAKE_AGENT_STATE` (directory for persisted sessions and `<name>.exhausted` flag files),
//! `FAKE_AGENT_LOG` (JSONL file; every received prompt/new/load is appended).
//!
//! Directives, read only from the LAST text block of a prompt (the user's message):
//! `[[slow N]]` stream N chunks 50 ms apart (honours `session/cancel`), `[[permission]]` ask for a tool
//! permission, `[[mem-write global|project TEXT]]` / `[[mem-read global|project]]` call the Harness MCP
//! memory server, `[[quota-start NAME]]` / `[[quota-mid NAME]]` fail with a usage-limit error when this agent
//! is NAME. A `<state>/<name>.exhausted` file makes every prompt fail with a usage-limit error.
//! Round 8.2: `[[say TEXT]]` streams TEXT as one chunk; `[[write REL_PATH TEXT]]` reports an `edit` tool call and
//! writes TEXT + newline to `REL_PATH` under the session cwd (turn-diff gates; paths with `..` are refused).
//! Phase 9.1: advertises modes `default|plan|acceptEdits`, models `fake-small|fake-large` and the config option
//! `effort` (`low|medium|high`) on new/load, implements `session/set_mode`, `session/set_model` and
//! `session/set_config_option` (logged), logs the `mcpServers` of every new/load; `[[options]]` reports the
//! session's current values.
//! Phase 9.2: `--caps image,embedded` sets `promptCapabilities`; every prompt log entry carries `blockTypes`.
//! `[[plan A|B|C]]` sends a plan (first completed, second in progress, rest pending); `[[tools]]` reports a tool
//! call with diff, terminal and text content plus a location; `[[commands]]` sends `available_commands_update`;
//! `[[usage USED SIZE]]` sends `usage_update` and adds prompt `usage` to the result; `[[permission all]]` asks with
//! the four ACP option kinds and notes the chosen option id; `[[think TEXT]]` sends TEXT as an
//! `agent_thought_chunk` (in directive order, so put it first to have it before the reply).
//! Round 9.2.5 (typed failures): `[[fail auth]]` fails with ACP `auth_required` (-32000), `[[fail exit]]` writes a
//! line to stderr and exits mid-prompt, `[[fail other]]` fails with -32603 plus `data`, `[[fail transient [SECS]]]`
//! fails with `harnessErrorKind: "transient"` (and `retryAfterSecs`). A `<state>/<name>.initfail` file makes
//! `session/new` and `session/load` fail.
//! `[[bg]]` announces a background task, ends it after 300 ms and then runs an autonomous turn "autonomous ok".

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

struct SessionData {
    cwd: String,
    mcp: Vec<Value>,
    history: Vec<(String, String)>,
    mode: String,
    model: String,
    effort: String,
}

impl SessionData {
    fn new(cwd: String, mcp: Vec<Value>, history: Vec<(String, String)>) -> Self {
        Self {
            cwd,
            mcp,
            history,
            mode: MODES[0].to_owned(),
            model: MODELS[0].to_owned(),
            effort: "medium".to_owned(),
        }
    }
}

const MODES: [&str; 3] = ["default", "plan", "acceptEdits"];
const MODELS: [&str; 2] = ["fake-small", "fake-large"];
const EFFORTS: [&str; 3] = ["low", "medium", "high"];

fn config_options(effort: &str) -> Value {
    json!([{
        "id": "effort", "name": "Reasoning effort", "category": "thought_level", "type": "select",
        "currentValue": effort,
        "options": EFFORTS.iter().map(|e| json!({"value": e, "name": e})).collect::<Vec<_>>()
    }])
}

/// ACP option advertisement of one session (`modes`, `models`, `configOptions`).
fn advertisement(data: &SessionData) -> Value {
    json!({
        "modes": {
            "currentModeId": data.mode,
            "availableModes": MODES.iter().map(|m| json!({"id": m, "name": m, "description": format!("fake {m} mode")})).collect::<Vec<_>>()
        },
        "models": {
            "currentModelId": data.model,
            "availableModels": MODELS.iter().map(|m| json!({"modelId": m, "name": m})).collect::<Vec<_>>()
        },
        "configOptions": config_options(&data.effort)
    })
}

struct Agent {
    name: String,
    state_dir: Option<PathBuf>,
    log: Option<PathBuf>,
    out: mpsc::UnboundedSender<String>,
    sessions: Mutex<HashMap<String, SessionData>>,
    cancels: Mutex<HashMap<String, Arc<AtomicBool>>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    next_id: Mutex<u64>,
    /// `promptCapabilities.image` / `embeddedContext` (`--caps`).
    image: bool,
    embedded: bool,
}

type Reply = Result<Value, Value>;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut name = "fake".to_owned();
    let mut caps = String::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--name" {
            if let Some(value) = args.next() {
                name = value;
            }
        } else if arg == "--caps" {
            if let Some(value) = args.next() {
                caps = value;
            }
        }
    }
    let (out, mut out_rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = out_rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err()
                || stdout.write_all(b"\n").await.is_err()
                || stdout.flush().await.is_err()
            {
                break;
            }
        }
    });
    let agent = Arc::new(Agent {
        state_dir: std::env::var_os("FAKE_AGENT_STATE").map(PathBuf::from),
        log: std::env::var_os("FAKE_AGENT_LOG").map(PathBuf::from),
        sessions: Mutex::new(HashMap::new()),
        cancels: Mutex::new(HashMap::new()),
        pending: Mutex::new(HashMap::new()),
        next_id: Mutex::new(0),
        image: caps.split(',').any(|c| c == "image"),
        embedded: caps.split(',').any(|c| c == "embedded"),
        name,
        out,
    });
    agent.load_persisted();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match (method, id) {
            (None, Some(id)) => {
                let sender = id
                    .as_u64()
                    .and_then(|id| agent.pending.lock().ok().and_then(|mut p| p.remove(&id)));
                if let Some(sender) = sender {
                    let _ = sender.send(message.get("result").cloned().unwrap_or(Value::Null));
                }
            }
            (Some(method), None) => {
                if method == "session/cancel" {
                    if let Some(session) = params.get("sessionId").and_then(Value::as_str) {
                        agent.cancel_flag(session).store(true, Ordering::SeqCst);
                    }
                }
            }
            (Some(method), Some(id)) => {
                let agent = Arc::clone(&agent);
                tokio::spawn(async move {
                    let reply = agent.handle(&method, &params).await;
                    agent.reply(&id, reply);
                });
            }
            (None, None) => {}
        }
    }
    // stdin closed: the client is shutting us down.
    drop(agent);
    let _ = writer.await;
}

impl Agent {
    fn send(&self, value: &Value) {
        let _ = self.out.send(value.to_string());
    }

    fn reply(&self, id: &Value, reply: Reply) {
        match reply {
            Ok(result) => self.send(&json!({"jsonrpc":"2.0","id":id,"result":result})),
            Err(error) => self.send(&json!({"jsonrpc":"2.0","id":id,"error":error})),
        }
    }

    fn update(&self, session: &str, update: &Value) {
        self.send(&json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":update}}));
    }

    async fn request(&self, method: &str, params: Value) -> Value {
        let (tx, rx) = oneshot::channel();
        let id = {
            let Ok(mut next) = self.next_id.lock() else {
                return Value::Null;
            };
            *next += 1;
            *next
        };
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(id, tx);
        }
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        rx.await.unwrap_or(Value::Null)
    }

    fn cancel_flag(&self, session: &str) -> Arc<AtomicBool> {
        match self.cancels.lock() {
            Ok(mut map) => Arc::clone(map.entry(session.to_owned()).or_default()),
            Err(_) => Arc::new(AtomicBool::new(false)),
        }
    }

    fn log(&self, entry: &Value) {
        let Some(path) = &self.log else { return };
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{entry}");
        }
    }

    fn persist_path(&self) -> Option<PathBuf> {
        self.state_dir
            .as_ref()
            .map(|d| d.join(format!("{}-sessions.json", self.name)))
    }

    fn load_persisted(&self) {
        let Some(path) = self.persist_path() else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        if let Ok(mut sessions) = self.sessions.lock() {
            for (id, data) in map {
                let history = data
                    .get("history")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|i| {
                                Some((
                                    i.get(0)?.as_str()?.to_owned(),
                                    i.get(1)?.as_str()?.to_owned(),
                                ))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let cwd = data
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                sessions.insert(id, SessionData::new(cwd, Vec::new(), history));
            }
        }
    }

    fn persist(&self) {
        let Some(path) = self.persist_path() else {
            return;
        };
        let Ok(sessions) = self.sessions.lock() else {
            return;
        };
        let map: serde_json::Map<String, Value> = sessions
            .iter()
            .map(|(id, s)| (id.clone(), json!({"cwd": s.cwd, "history": s.history})))
            .collect();
        drop(sessions);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, Value::Object(map).to_string());
    }

    fn exhausted_flag(&self) -> Option<i64> {
        let path = self
            .state_dir
            .as_ref()?
            .join(format!("{}.exhausted", self.name));
        let text = std::fs::read_to_string(path).ok()?;
        Some(
            text.trim()
                .parse::<i64>()
                .unwrap_or_else(|_| now_secs() + 3600),
        )
    }

    fn init_fail(&self) -> bool {
        self.state_dir
            .as_ref()
            .is_some_and(|d| d.join(format!("{}.initfail", self.name)).exists())
    }

    /// `[[fail KIND ...]]` errors (phase-9-parity.md §9.2.5 R20).
    fn fail_error(&self, args: &[String]) -> Value {
        match args.first().map(String::as_str) {
            Some("auth") => json!({"code": -32000, "message": "Authentication required"}),
            Some("transient") => {
                let secs = args.get(1).and_then(|s| s.parse::<u64>().ok()).unwrap_or(5);
                json!({
                    "code": -32603,
                    "message": format!("{} is overloaded, try again later", self.name),
                    "data": {"harnessErrorKind": "transient", "retryAfterSecs": secs}
                })
            }
            _ => json!({
                "code": -32603,
                "message": format!("{} failed: model refused the request", self.name),
                "data": {"reason": "fake failure", "agent": self.name}
            }),
        }
    }

    fn quota_error(&self, resets_at: i64) -> Value {
        json!({
            "code": -32603,
            "message": format!("{} usage limit reached|{resets_at}", self.name),
            "data": {"harnessErrorKind": "quota_exhausted", "resetsAt": resets_at}
        })
    }

    async fn handle(&self, method: &str, params: &Value) -> Reply {
        match method {
            "initialize" => Ok(json!({
                "protocolVersion": 1,
                "agentCapabilities": {"loadSession": true, "promptCapabilities": {"image": self.image, "audio": false, "embeddedContext": self.embedded}, "mcpCapabilities": {"http": false, "sse": false}},
                "authMethods": [],
                "agentInfo": {"name": self.name, "version": "0.1.0"}
            })),
            "session/new" | "session/load" if self.init_fail() => Err(json!({
                "code": -32603,
                "message": format!("{} could not start the session", self.name),
                "data": {"reason": "initfail flag"}
            })),
            "session/new" => Ok(self.new_session(params)),
            "session/load" => self.load_session(params),
            "session/prompt" => self.prompt(params).await,
            "session/set_mode" | "session/set_model" | "session/set_config_option" => {
                self.set_option(method, params)
            }
            "_session/async_task/stop" => Ok(json!({"stopped": true})),
            _ => Err(json!({"code": -32601, "message": format!("unknown method {method}")})),
        }
    }

    fn new_session(&self, params: &Value) -> Value {
        let id = format!("{}-{}", self.name, uuid::Uuid::new_v4().simple());
        let mcp = params
            .get("mcpServers")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let data = SessionData::new(
            params
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            mcp.clone(),
            Vec::new(),
        );
        let mut result = advertisement(&data);
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.insert(id.clone(), data);
        }
        self.persist();
        self.log(&json!({"agent": self.name, "pid": std::process::id(), "event": "new", "sessionId": id, "mcpServers": mcp}));
        result["sessionId"] = json!(id);
        result
    }

    fn load_session(&self, params: &Value) -> Reply {
        let id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let mcp = params
            .get("mcpServers")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let (history, result) = {
            let Ok(mut sessions) = self.sessions.lock() else {
                return Err(json!({"code": -32603, "message": "lock"}));
            };
            let Some(session) = sessions.get_mut(&id) else {
                return Err(json!({"code": -32002, "message": format!("session {id} not found")}));
            };
            session.mcp.clone_from(&mcp);
            (session.history.clone(), advertisement(session))
        };
        // ACP: replay the conversation as updates before answering.
        for (role, text) in &history {
            let kind = if role == "user" {
                "user_message_chunk"
            } else {
                "agent_message_chunk"
            };
            self.update(
                &id,
                &json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}}),
            );
        }
        self.log(&json!({"agent": self.name, "pid": std::process::id(), "event": "load", "sessionId": id, "replayed": history.len(), "mcpServers": mcp}));
        Ok(result)
    }

    /// `session/set_mode` / `set_model` / `set_config_option`: validate, store, log.
    fn set_option(&self, method: &str, params: &Value) -> Reply {
        let session = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let text = |key: &str| {
            params
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let invalid =
            |what: String| Err(json!({"code": -32602, "message": format!("invalid {what}")}));
        let (key, value) = match method {
            "session/set_mode" => ("mode".to_owned(), text("modeId")),
            "session/set_model" => ("model".to_owned(), text("modelId")),
            _ => (text("configId"), text("value")),
        };
        let allowed: &[&str] = match key.as_str() {
            "mode" => &MODES,
            "model" => &MODELS,
            "effort" => &EFFORTS,
            _ => return invalid(format!("config option {key}")),
        };
        if !allowed.contains(&value.as_str()) {
            return invalid(format!("{key} {value}"));
        }
        let effort = {
            let Ok(mut sessions) = self.sessions.lock() else {
                return Err(json!({"code": -32603, "message": "lock"}));
            };
            let Some(data) = sessions.get_mut(&session) else {
                return Err(
                    json!({"code": -32002, "message": format!("session {session} not found")}),
                );
            };
            match key.as_str() {
                "mode" => data.mode.clone_from(&value),
                "model" => data.model.clone_from(&value),
                _ => data.effort.clone_from(&value),
            }
            data.effort.clone()
        };
        self.log(&json!({"agent": self.name, "pid": std::process::id(), "event": "set", "sessionId": session, "key": key, "value": value}));
        match key.as_str() {
            "mode" => {
                self.update(
                    &session,
                    &json!({"sessionUpdate": "current_mode_update", "currentModeId": value}),
                );
                Ok(json!({}))
            }
            "model" => Ok(json!({})),
            _ => Ok(json!({"configOptions": config_options(&effort)})),
        }
    }

    fn options_note(&self, session: &str) -> String {
        self.sessions
            .lock()
            .ok()
            .and_then(|s| {
                s.get(session).map(|d| {
                    format!(
                        "options=mode:{},model:{},effort:{}",
                        d.mode, d.model, d.effort
                    )
                })
            })
            .unwrap_or_else(|| "options=unknown".to_owned())
    }

    // 指令分派表：一条指令一行，按 rustfmt 换行后略超 100 行，拆开反而更难读。
    #[allow(clippy::too_many_lines)]
    async fn prompt(&self, params: &Value) -> Reply {
        let session = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let blocks: Vec<String> = params
            .get("prompt")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|b| b.get("text").and_then(Value::as_str).map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let block_types: Vec<String> = params
            .get("prompt")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|b| {
                        b.get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("?")
                            .to_owned()
                    })
                    .collect()
            })
            .unwrap_or_default();
        let user = blocks.last().cloned().unwrap_or_default();
        self.log(&json!({"agent": self.name, "pid": std::process::id(), "event": "prompt", "sessionId": session, "blocks": blocks, "blockTypes": block_types}));
        let cancel = self.cancel_flag(&session);
        cancel.store(false, Ordering::SeqCst);
        if let Some(resets_at) = self.exhausted_flag() {
            return Err(self.quota_error(resets_at));
        }
        let directives = parse_directives(&user);
        let mut reply = format!("[{}] ", self.name);
        let mut notes: Vec<String> = Vec::new();
        let mut usage: Option<Value> = None;
        for (verb, args) in &directives {
            let targeted = args.first().map(String::as_str) == Some(self.name.as_str());
            match verb.as_str() {
                "quota-start" if targeted => return Err(self.quota_error(now_secs() + 3600)),
                "fail" if args.first().map(String::as_str) == Some("exit") => {
                    self.chunk(&session, "partial ", &mut reply);
                    eprintln!("fake agent {}: simulated crash", self.name);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    std::process::exit(3);
                }
                "fail" => return Err(self.fail_error(args)),
                "quota-mid" if targeted => {
                    for i in 0..3 {
                        self.chunk(&session, &format!("partial-{i} "), &mut reply);
                        tokio::time::sleep(Duration::from_millis(30)).await;
                    }
                    return Err(self.quota_error(now_secs() + 3600));
                }
                "slow" => {
                    let n: usize = args.first().and_then(|a| a.parse().ok()).unwrap_or(20);
                    for i in 0..n {
                        if cancel.load(Ordering::SeqCst) {
                            self.remember(&session, &user, &reply);
                            return Ok(json!({"stopReason": "cancelled"}));
                        }
                        self.chunk(&session, &format!("chunk-{i} "), &mut reply);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
                "permission" => notes.push(format!(
                    "permission={}",
                    self.ask_permission(&session, args.first().map(String::as_str) == Some("all"))
                        .await
                )),
                "plan" => self.send_plan(&session, &args.join(" ")),
                "tools" => self.send_tools(&session),
                "commands" => self.update(&session, &commands_update()),
                "bg" => self.background(&session),
                "usage" => usage = Some(self.send_usage(&session, args)),
                "options" => notes.push(self.options_note(&session)),
                "say" => self.chunk(&session, &format!("{} ", args.join(" ")), &mut reply),
                "think" => self.update(
                    &session,
                    &json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": args.join(" ")}}),
                ),
                "write" => notes.push(self.write_file(&session, args).await),
                "mem-write" | "mem-read" => {
                    notes.push(self.memory_call(&session, verb, args).await);
                }
                _ => {}
            }
        }
        if cancel.load(Ordering::SeqCst) {
            self.remember(&session, &user, &reply);
            return Ok(json!({"stopReason": "cancelled"}));
        }
        let saw_memory = blocks.iter().any(|b| b.contains("<harness-memory"));
        let summary = format!(
            "reply to: {} | blocks={} memory={} {}",
            user.chars().take(80).collect::<String>(),
            blocks.len(),
            saw_memory,
            notes.join(" ")
        );
        self.chunk(&session, &summary, &mut reply);
        self.remember(&session, &user, &reply);
        let mut result = json!({"stopReason": "end_turn"});
        if let Some(usage) = usage {
            result["usage"] = usage;
        }
        Ok(result)
    }

    fn background(&self, session: &str) {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let task = format!("bg-{suffix}");
        let turn = format!("turn-{suffix}");
        self.update(
            session,
            &json!({"sessionUpdate": "async_task_spawned", "asyncTaskId": task, "name": "sleep", "taskType": "local_bash", "description": "gate sleep", "showInTranscript": true, "canStop": true}),
        );
        let out = self.out.clone();
        let session = session.to_owned();
        tokio::spawn(async move {
            let send = |update: Value| {
                let _ = out.send(json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":update}}).to_string());
            };
            tokio::time::sleep(Duration::from_millis(300)).await;
            send(
                json!({"sessionUpdate": "async_task_state_update", "asyncTaskId": task, "state": "completed", "summary": "exit code 0"}),
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
            send(
                json!({"sessionUpdate": "async_task_spawned", "asyncTaskId": turn, "name": "Working", "taskType": "turn", "description": "gate sleep (exit code 0)", "showInTranscript": true, "canStop": true}),
            );
            send(
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "autonomous ok"}}),
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
            send(
                json!({"sessionUpdate": "async_task_state_update", "asyncTaskId": turn, "state": "completed"}),
            );
        });
    }

    fn chunk(&self, session: &str, text: &str, reply: &mut String) {
        reply.push_str(text);
        self.update(
            session,
            &json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}),
        );
    }

    fn remember(&self, session: &str, user: &str, reply: &str) {
        if let Ok(mut sessions) = self.sessions.lock() {
            if let Some(data) = sessions.get_mut(session) {
                data.history.push(("user".into(), user.to_owned()));
                data.history.push(("agent".into(), reply.to_owned()));
            }
        }
        self.persist();
    }

    /// `usage_update` now; returns the prompt-result `usage`.
    fn send_usage(&self, session: &str, args: &[String]) -> Value {
        let arg =
            |i: usize, default: u64| args.get(i).and_then(|a| a.parse().ok()).unwrap_or(default);
        self.update(
            session,
            &json!({"sessionUpdate": "usage_update", "used": arg(0, 1200), "size": arg(1, 200_000), "cost": {"amount": 0.25, "currency": "USD"}}),
        );
        json!({"inputTokens": 100, "outputTokens": 20, "totalTokens": 120, "cachedReadTokens": 7})
    }

    fn send_plan(&self, session: &str, spec: &str) {
        let entries: Vec<Value> = spec
            .split('|')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .enumerate()
            .map(|(i, content)| {
                let status = match i {
                    0 => "completed",
                    1 => "in_progress",
                    _ => "pending",
                };
                json!({"content": content, "priority": if i == 0 { "high" } else { "medium" }, "status": status})
            })
            .collect();
        self.update(
            session,
            &json!({"sessionUpdate": "plan", "entries": entries}),
        );
    }

    fn send_tools(&self, session: &str) {
        let id = format!("tool-{}", uuid::Uuid::new_v4().simple());
        self.update(
            session,
            &json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": "edit src/lib.rs", "kind": "edit", "status": "in_progress",
                "locations": [{"path": "src/lib.rs", "line": 3}]}),
        );
        self.update(
            session,
            &json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed",
                "content": [
                    {"type": "diff", "path": "src/lib.rs", "oldText": "old line", "newText": "new line"},
                    {"type": "terminal", "terminalId": "term-1"},
                    {"type": "content", "content": {"type": "text", "text": "tool says hi"}}
                ],
                "rawOutput": {"ok": true}}),
        );
    }

    async fn ask_permission(&self, session: &str, all: bool) -> String {
        let tool_call_id = format!("tool-{}", uuid::Uuid::new_v4().simple());
        let tool = json!({"toolCallId": tool_call_id, "title": "write demo.txt", "kind": "edit", "status": "pending", "rawInput": {"path": "demo.txt"}});
        let mut update = tool.clone();
        if let Some(map) = update.as_object_mut() {
            map.insert("sessionUpdate".into(), json!("tool_call"));
        }
        self.update(session, &update);
        let options = if all {
            json!([
                {"optionId": "allow", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "allow-always", "name": "Always allow", "kind": "allow_always"},
                {"optionId": "deny", "name": "Deny", "kind": "reject_once"},
                {"optionId": "deny-always", "name": "Always deny", "kind": "reject_always"}
            ])
        } else {
            json!([
                {"optionId": "allow", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "deny", "name": "Deny", "kind": "reject_once"}
            ])
        };
        let response = self
            .request(
                "session/request_permission",
                json!({
                    "sessionId": session,
                    "toolCall": tool,
                    "options": options
                }),
            )
            .await;
        let outcome = response.get("outcome").cloned().unwrap_or(Value::Null);
        let chosen = outcome
            .get("optionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let decision = match outcome.get("outcome").and_then(Value::as_str) {
            Some("selected") => match chosen.as_str() {
                "allow" | "allow-always" => "allowed",
                _ => "denied",
            },
            _ => "cancelled",
        };
        let status = if decision == "allowed" {
            "completed"
        } else {
            "failed"
        };
        self.update(
            session,
            &json!({"sessionUpdate": "tool_call_update", "toolCallId": tool_call_id, "status": status}),
        );
        if all {
            format!("{decision}:{chosen}")
        } else {
            decision.to_owned()
        }
    }

    async fn write_file(&self, session: &str, args: &[String]) -> String {
        let Some(relative) = args.first() else {
            return "write=missing-path".into();
        };
        if relative.split(['/', '\\']).any(|part| part == "..")
            || std::path::Path::new(relative).is_absolute()
        {
            return "write=refused".into();
        }
        let cwd = self
            .sessions
            .lock()
            .ok()
            .and_then(|s| s.get(session).map(|d| d.cwd.clone()))
            .unwrap_or_default();
        let tool_call_id = format!("tool-{}", uuid::Uuid::new_v4().simple());
        self.update(
            session,
            &json!({"sessionUpdate": "tool_call", "toolCallId": tool_call_id, "title": format!("write {relative}"), "kind": "edit", "status": "in_progress", "rawInput": {"path": relative}}),
        );
        let path = PathBuf::from(cwd).join(relative);
        let content = format!("{}\n", args.get(1..).unwrap_or_default().join(" "));
        let result = async {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&path, content).await
        }
        .await;
        let status = if result.is_ok() {
            "completed"
        } else {
            "failed"
        };
        self.update(
            session,
            &json!({"sessionUpdate": "tool_call_update", "toolCallId": tool_call_id, "status": status}),
        );
        format!("write={status}")
    }

    async fn memory_call(&self, session: &str, verb: &str, args: &[String]) -> String {
        let server = self
            .sessions
            .lock()
            .ok()
            .and_then(|s| s.get(session).and_then(|d| d.mcp.first().cloned()));
        let Some(server) = server else {
            return format!("{verb}=no-mcp-server");
        };
        let scope = args.first().cloned().unwrap_or_else(|| "global".into());
        let (tool, arguments) = if verb == "mem-write" {
            (
                "memory_write",
                json!({"scope": scope, "content": args.get(1..).unwrap_or_default().join(" "), "mode": "append"}),
            )
        } else {
            ("memory_read", json!({"scope": scope}))
        };
        match call_mcp(&server, tool, arguments).await {
            Ok(text) => format!("{verb}[{scope}]=<<{}>>", text.trim()),
            Err(error) => format!("{verb}=error:{error}"),
        }
    }
}

fn commands_update() -> Value {
    json!({"sessionUpdate": "available_commands_update", "availableCommands": [
        {"name": "review", "description": "Agent review", "input": {"hint": "path"}},
        {"name": "compact", "description": "Compact the conversation"}
    ]})
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn parse_directives(text: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else { break };
        let mut words = after[..end].split_whitespace().map(str::to_owned);
        if let Some(verb) = words.next() {
            out.push((verb, words.collect()));
        }
        rest = &after[end + 2..];
    }
    out
}

/// Spawns the MCP server described by an ACP `McpServer` entry, calls one tool, returns its text.
async fn call_mcp(server: &Value, tool: &str, arguments: Value) -> Result<String, String> {
    let command = server
        .get("command")
        .and_then(Value::as_str)
        .ok_or("no command")?;
    let mut process = tokio::process::Command::new(command);
    if let Some(args) = server.get("args").and_then(Value::as_array) {
        process.args(args.iter().filter_map(Value::as_str));
    }
    if let Some(env) = server.get("env").and_then(Value::as_array) {
        for pair in env {
            if let (Some(k), Some(v)) = (
                pair.get("name").and_then(Value::as_str),
                pair.get("value").and_then(Value::as_str),
            ) {
                process.env(k, v);
            }
        }
    }
    process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    process.creation_flags(CREATE_NO_WINDOW);
    let mut child = process.spawn().map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let messages = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fake-agent","version":"0"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":tool,"arguments":arguments}}),
    ];
    for message in messages {
        stdin
            .write_all(format!("{message}\n").as_bytes())
            .await
            .map_err(|e| e.to_string())?;
    }
    stdin.flush().await.map_err(|e| e.to_string())?;
    let mut lines = BufReader::new(stdout).lines();
    let mut result = Err("no tools/call response".to_owned());
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if message.get("id").and_then(Value::as_u64) == Some(2) {
            let text = message
                .pointer("/result/content/0/text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let is_error =
                message.pointer("/result/isError").and_then(Value::as_bool) == Some(true);
            result = if is_error { Err(text) } else { Ok(text) };
            break;
        }
    }
    drop(stdin);
    let _ = child.wait().await;
    result
}
