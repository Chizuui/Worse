//! Loopback delegate server (phase-9-parity.md §9.8 Transport): one JSON request line per TCP connection from
//! `harness-memory-mcp --delegate`, one JSON reply line.

use std::sync::{Arc, Weak};

use harness_core_api::{
    AgentId, CreateSession, EventKind, HarnessCore, OrchestraNode, OrchestraRole, QuotaState,
    RecvError, SendMessage, SessionId, SubagentStatus, SubagentView, TurnStatus,
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::delegate::{preview, Endpoint, MAX_ACTIVE};
use crate::hub::Hub;
use crate::store;
use crate::util::now_ms;

/// Largest request line (a task is at most 100 000 characters, i.e. up to 400 KB of UTF-8).
const MAX_REQUEST: u64 = 1024 * 1024;
const MAX_TASK_CHARS: usize = 100_000;

/// Binds `127.0.0.1:0` and serves until aborted. Returns the endpoint and the accept task.
pub(crate) async fn start(
    hub: Weak<Hub>,
) -> std::io::Result<(Endpoint, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let endpoint = Endpoint {
        addr,
        token: token.clone(),
    };
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let (hub, token) = (hub.clone(), token.clone());
            tokio::spawn(async move {
                if let Err(error) = connection(hub, &token, stream).await {
                    tracing::debug!(%error, "delegate connection ended");
                }
            });
        }
    });
    Ok((endpoint, task))
}

/// Delay before a finished subagent turn is pushed, so a blocking `delegate` or a running `subagent_wait`
/// can take the answer first.
const PUSH_DELAY: std::time::Duration = std::time::Duration::from_millis(1500);
/// Longest subagent answer copied into a pushed parent message.
const PUSH_TEXT_CHARS: usize = 20_000;

/// Delivers finished subagent turns to an idle parent as a new turn, so the parent does not have to poll
/// with `subagent_wait`. Runs when a child turn ends (parent idle) and when the parent's own turn ends
/// (results that arrived while it was busy). Ends when the hub or the event bus goes away.
pub(crate) async fn push(hub: Weak<Hub>) {
    let Some(mut events) = hub.upgrade().map(|h| h.ctx.events.subscribe()) else {
        return;
    };
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return,
        };
        let EventKind::TurnFinished { outcome, .. } = &event.kind else {
            continue;
        };
        let Some(session) = event.session_id.clone() else {
            continue;
        };
        let Some(strong) = hub.upgrade() else {
            return;
        };
        let parent = match strong.ctx.subagents.get(&session) {
            Some(view) => view.parent_id,
            // The parent itself was stopped by the user: do not restart it right away.
            None if matches!(outcome, harness_core_api::TurnOutcome::Cancelled) => continue,
            None => session,
        };
        if !strong.ctx.subagents.coord.has_pending(&parent) {
            continue;
        }
        drop(strong);
        let hub = hub.clone();
        tokio::spawn(async move {
            tokio::time::sleep(PUSH_DELAY).await;
            if let Some(hub) = hub.upgrade() {
                push_to(&hub, &parent).await;
            }
        });
    }
}

async fn push_to(hub: &Arc<Hub>, parent: &SessionId) {
    let Ok(detail) = hub.open_session(parent.clone()).await else {
        return;
    };
    let busy = detail.turns.iter().any(|t| {
        matches!(
            t.status,
            TurnStatus::Queued | TurnStatus::Running | TurnStatus::Paused
        )
    });
    if busy || !hub.ctx.subagents.coord.has_pending(parent) {
        return;
    }
    let (finished, notes) = hub.ctx.subagents.coord.take_ready(parent, None);
    if finished.is_empty() {
        return;
    }
    let mut text = String::from(
        "[Harness] A background subagent finished. Review its result below and continue the task; \
         you do not need to call subagent_wait for it.",
    );
    for (child, turn) in &finished {
        let title = hub
            .ctx
            .subagents
            .get(child)
            .map(|v| v.title)
            .unwrap_or_default();
        match result(hub, child, turn).await {
            Ok(value) => {
                let answer: String = value["text"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(PUSH_TEXT_CHARS)
                    .collect();
                text.push_str(&format!(
                    "\n\n## Subagent \"{title}\" ({}, session {child}): {}",
                    value["agentName"].as_str().unwrap_or_default(),
                    value["outcome"].as_str().unwrap_or_default(),
                ));
                if let Some(error) = value["error"].as_str().filter(|e| !e.is_empty()) {
                    text.push_str(&format!("\nError: {error}"));
                }
                text.push_str(&format!("\n{answer}"));
            }
            Err(error) => {
                text.push_str(&format!(
                    "\n\n## Subagent \"{title}\" (session {child}): failed\n{error}"
                ));
            }
        }
    }
    for note in &notes {
        text.push_str(&format!("\n\nNote from {}: {}", note.child, note.text));
    }
    if let Err(error) = hub
        .send_message(SendMessage {
            session_id: parent.clone(),
            text,
            agent: None,
            client_message_id: None,
            context: Vec::new(),
        })
        .await
    {
        tracing::warn!(%parent, %error, "pushing subagent result failed");
    }
}

async fn connection(hub: Weak<Hub>, token: &str, stream: TcpStream) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read.take(MAX_REQUEST));
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let request: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
    let reply = match (request["token"].as_str(), hub.upgrade()) {
        (Some(given), Some(hub)) if constant_eq(given, token) => {
            // 读端保留下来：父 agent 取消工具调用时 MCP 进程会断开连接，据此停止子会话（§9.8）。
            let closed = async move {
                let mut sink = [0u8; 64];
                let mut rest = reader.into_inner();
                loop {
                    match rest.read(&mut sink).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            };
            match handle(&hub, &request, closed).await {
                Ok(value) => json!({"ok": true, "result": value}),
                Err(message) => json!({"ok": false, "error": message}),
            }
        }
        (_, None) => json!({"ok": false, "error": "Harness is shutting down"}),
        _ => json!({"ok": false, "error": "invalid delegate token"}),
    };
    write.write_all(format!("{reply}\n").as_bytes()).await?;
    write.flush().await
}

fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

async fn handle(
    hub: &Arc<Hub>,
    request: &Value,
    closed: impl std::future::Future<Output = ()>,
) -> Result<Value, String> {
    let parent = SessionId::new(request["parent"].as_str().unwrap_or_default());
    let parent_row = {
        let lookup = parent.clone();
        hub.ctx
            .store
            .run(move |c| store::load_session(c, &lookup))
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("unknown parent session {parent}"))?
    };
    // 角色以 hub 注册表为准（§9.8.1），环境变量里的 role 只决定工具列表，不授予权限。
    let is_child = hub.ctx.subagents.is_child(&parent);
    let is_router = hub.ctx.routers.is_router(&parent);
    let args = &request["args"];
    match request["op"].as_str() {
        Some("route" | "dismiss") if !is_router => {
            Err("only the Harness inbox router can route or dismiss triggers".into())
        }
        Some("route") => hub.route_trigger(&parent, args).await,
        Some("dismiss") => hub.dismiss_trigger(&parent, args).await,
        Some(op) if is_router && op != "agents" => {
            Err("the inbox router can only list agents, route and dismiss triggers".into())
        }
        Some("agents" | "delegate" | "message" | "wait") if is_child => {
            Err("a subagent cannot delegate or manage subagents; use notify_parent".into())
        }
        Some("notify") if !is_child => Err("notify_parent is only for subagents".into()),
        Some("agents") => Ok(agents(hub, &parent_row).await),
        Some("delegate") => delegate(hub, &parent_row, args, closed).await,
        Some("message") => message(hub, &parent_row, args).await,
        Some("wait") => wait(hub, &parent_row, args, closed).await,
        Some("notify") => notify(hub, &parent_row, args).await,
        Some("claim") => claim(hub, &parent_row, args),
        Some("release") => release(hub, &parent_row, args),
        other => Err(format!("unknown op {other:?}")),
    }
}

/// The caller's own subagent, or an error.
fn own_child(hub: &Arc<Hub>, parent: &store::SessionRow, id: &str) -> Result<SessionId, String> {
    let child = SessionId::new(id.trim());
    let known = hub.ctx.subagents.get(&child);
    if id.trim().is_empty() || known.as_ref().map(|v| &v.parent_id) != Some(&parent.id) {
        return Err(format!("{id} is not a subagent of this session"));
    }
    Ok(child)
}

fn task_text(args: &Value, key: &str) -> Result<String, String> {
    let text = args[key].as_str().unwrap_or_default().trim().to_owned();
    if text.is_empty() {
        return Err(format!("{key} is required"));
    }
    if text.chars().count() > MAX_TASK_CHARS {
        return Err(format!("{key} is longer than {MAX_TASK_CHARS} characters"));
    }
    Ok(text)
}

/// `subagent_message`: queue (or interrupt and send) a message to a running or idle subagent.
async fn message(
    hub: &Arc<Hub>,
    parent: &store::SessionRow,
    args: &Value,
) -> Result<Value, String> {
    let child = own_child(hub, parent, args["session_id"].as_str().unwrap_or_default())?;
    let text = task_text(args, "text")?;
    let active = hub
        .ctx
        .subagents
        .get(&child)
        .is_some_and(|v| v.status.is_active());
    if !active && hub.ctx.subagents.active_count(&parent.id) >= MAX_ACTIVE {
        return Err(format!(
            "at most {MAX_ACTIVE} subagents can run at once; wait for one to finish"
        ));
    }
    let interrupted = args["interrupt"].as_bool() == Some(true) && active;
    if interrupted {
        let mut events = hub.ctx.events.subscribe();
        let _ = hub.stop(child.clone()).await;
        // 打断：等当前这一轮真正结束再发送，最多 10 秒（§9.8.1），以事件为准而不是固定延时。
        let ended = async {
            loop {
                if !busy(hub, &child).await {
                    return;
                }
                match events.recv().await {
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return,
                }
            }
        };
        if tokio::time::timeout(std::time::Duration::from_secs(10), ended)
            .await
            .is_err()
        {
            return Err("the subagent did not stop within 10 s; the message was not sent".into());
        }
    }
    let accepted = hub
        .send_message(SendMessage {
            session_id: child.clone(),
            text,
            agent: None,
            client_message_id: None,
            context: Vec::new(),
        })
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "sessionId": child,
        "turnId": accepted.turn_id,
        "position": accepted.position,
        "interrupted": interrupted,
        "queued": active && !interrupted,
    }))
}

async fn busy(hub: &Arc<Hub>, child: &SessionId) -> bool {
    hub.open_session(child.clone()).await.is_ok_and(|d| {
        d.turns
            .iter()
            .any(|t| matches!(t.status, TurnStatus::Running | TurnStatus::Paused))
    })
}

/// `subagent_wait`: block until a subagent turn finishes or a note arrives (or the timeout passes).
async fn wait(
    hub: &Arc<Hub>,
    parent: &store::SessionRow,
    args: &Value,
    closed: impl std::future::Future<Output = ()>,
) -> Result<Value, String> {
    let only = match args["session_ids"].as_array() {
        Some(ids) if !ids.is_empty() => Some(
            ids.iter()
                .map(|id| own_child(hub, parent, id.as_str().unwrap_or_default()))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        _ => None,
    };
    let timeout = args["timeout_s"].as_u64().unwrap_or(300).min(3600);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout);
    let coord = &hub.ctx.subagents.coord;
    tokio::pin!(closed);
    let (finished, notes) = loop {
        // 先登记唤醒再检查，避免检查与等待之间到达的通知丢失。
        let notified = coord.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let (finished, notes) = coord.take_ready(&parent.id, only.as_deref());
        if !finished.is_empty() || !notes.is_empty() || tokio::time::Instant::now() >= deadline {
            break (finished, notes);
        }
        tokio::select! {
            () = &mut notified => {}
            () = tokio::time::sleep_until(deadline) => {}
            // 父 agent 放弃等待：后台子 agent 继续运行，不做任何停止。
            () = &mut closed => return Err("cancelled".into()),
        }
    };
    let mut results = Vec::new();
    for (child, turn) in &finished {
        match result(hub, child, turn).await {
            Ok(value) => results.push(value),
            Err(error) => results
                .push(json!({"sessionId": child, "outcome": "failed", "error": error, "text": ""})),
        }
    }
    let views: Vec<Value> = hub
        .ctx
        .subagents
        .of_parent(&parent.id)
        .into_iter()
        .filter(|v| !matches!(&only, Some(o) if !o.contains(&v.session_id)))
        .map(|v| {
            json!({
                "sessionId": v.session_id,
                "agentName": hub.ctx.display_name(&v.agent),
                "title": v.title,
                "status": v.status,
                "activity": v.activity,
            })
        })
        .collect();
    let notes: Vec<Value> = notes
        .into_iter()
        .map(|n| {
            let title = hub
                .ctx
                .subagents
                .get(&n.child)
                .map(|v| v.title)
                .unwrap_or_default();
            json!({"sessionId": n.child, "title": title, "text": n.text})
        })
        .collect();
    Ok(json!({"subagents": views, "notes": notes, "finished": results}))
}

/// `notify_parent`: a subagent leaves a note for its parent and shows it as its activity.
async fn notify(hub: &Arc<Hub>, child: &store::SessionRow, args: &Value) -> Result<Value, String> {
    let text = args["text"].as_str().unwrap_or_default().trim().to_owned();
    if text.is_empty() {
        return Err("text is required".into());
    }
    let text: String = text
        .chars()
        .take(crate::delegate_coord::MAX_NOTE_CHARS)
        .collect();
    let Some(mut view) = hub.ctx.subagents.get(&child.id) else {
        return Err("notify_parent is only for subagents".into());
    };
    let first = text.lines().next().unwrap_or_default();
    view.activity = format!("Note: {first}").chars().take(160).collect();
    let parent = view.parent_id.clone();
    hub.ctx.put_subagent(view).await;
    hub.ctx.subagents.coord.add_note(
        &parent,
        crate::delegate_coord::Note {
            child: child.id.clone(),
            text,
            at: now_ms(),
        },
    );
    Ok(json!({"delivered": true}))
}

fn claim_paths(args: &Value) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for path in args["paths"].as_array().into_iter().flatten() {
        let normalized = crate::delegate_coord::normalize(path.as_str().unwrap_or_default())?;
        if !out.contains(&normalized) {
            out.push(normalized);
        }
    }
    Ok(out)
}

fn claim_json(c: &crate::delegate_coord::Claim) -> Value {
    json!({"path": c.path, "sessionId": c.holder, "title": c.title, "agent": c.agent, "note": c.note})
}

/// `claim_paths`: advisory claims, scoped to the workspace identity key.
fn claim(hub: &Arc<Hub>, caller: &store::SessionRow, args: &Value) -> Result<Value, String> {
    let paths = claim_paths(args)?;
    if paths.is_empty() {
        let all = hub.ctx.subagents.coord.claims_in(&caller.workspace_key);
        return Ok(
            json!({"granted": [], "conflicts": [], "claims": all.iter().map(claim_json).collect::<Vec<_>>()}),
        );
    }
    let request = crate::delegate_coord::Claim {
        workspace: caller.workspace_key.clone(),
        path: String::new(),
        holder: caller.id.clone(),
        title: caller.title.clone(),
        agent: caller.active_agent.clone(),
        note: args["note"]
            .as_str()
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect(),
    };
    let outcome = hub.ctx.subagents.coord.claim(&request, &paths);
    Ok(json!({
        "granted": outcome.granted,
        "conflicts": outcome.conflicts.iter().map(|(path, held)| {
            let mut v = claim_json(held);
            v["requested"] = json!(path);
            v
        }).collect::<Vec<_>>(),
        "claims": outcome.all.iter().map(claim_json).collect::<Vec<_>>(),
    }))
}

fn release(hub: &Arc<Hub>, caller: &store::SessionRow, args: &Value) -> Result<Value, String> {
    let paths = claim_paths(args)?;
    let released = hub.ctx.subagents.coord.release(&caller.id, &paths);
    Ok(json!({"released": released}))
}

fn agent_state(a: &harness_core_api::AgentInfo) -> &'static str {
    if !matches!(
        a.availability,
        harness_core_api::Availability::Available { .. }
    ) {
        "not installed"
    } else if matches!(a.quota, QuotaState::Exhausted { .. }) {
        "limit reached"
    } else {
        "available"
    }
}

/// The Orchestra team of `parent` when Orchestra is active for it (§9.11).
async fn team(hub: &Arc<Hub>, parent: &store::SessionRow) -> Option<Vec<OrchestraNode>> {
    let orchestra = hub.load_orchestra().await;
    let root = orchestra.orchestrator()?;
    // A Claude account (`claude-code-<label>`) is the same orchestrator as `claude-code` with another login.
    let config = &hub.ctx.config;
    let same = root.agent == parent.active_agent
        || (crate::accounts::is_claude(config, &root.agent)
            && crate::accounts::is_claude(config, &parent.active_agent));
    if !same {
        return None;
    }
    Some(orchestra.team().into_iter().cloned().collect())
}

async fn agents(hub: &Arc<Hub>, parent: &store::SessionRow) -> Value {
    let list = hub.list_agents().await.unwrap_or_default();
    let Some(team) = team(hub, parent).await else {
        return Value::Array(
            list.iter()
                .map(|a| json!({"id": a.id, "name": a.display_name, "state": agent_state(a)}))
                .collect(),
        );
    };
    Value::Array(
        team.iter()
            .filter_map(|node| {
                let a = list.iter().find(|a| a.id == node.agent)?;
                Some(json!({
                    "id": a.id,
                    "name": a.display_name,
                    "state": agent_state(a),
                    "role": node.role.label(),
                    "instructions": node.instructions.lines().next().unwrap_or_default(),
                }))
            })
            .collect(),
    )
}

/// Runs the task as one turn of a (new or existing) subagent session and waits for its answer.
#[allow(clippy::too_many_lines)]
async fn delegate(
    hub: &Arc<Hub>,
    parent: &store::SessionRow,
    args: &Value,
    closed: impl std::future::Future<Output = ()>,
) -> Result<Value, String> {
    let mut task = task_text(args, "task")?;
    let background = args["background"].as_bool() == Some(true);
    if hub.ctx.subagents.active_count(&parent.id) >= MAX_ACTIVE {
        return Err(format!(
            "at most {MAX_ACTIVE} subagents can run at once; wait for one to finish"
        ));
    }
    let mut agent = args["agent"]
        .as_str()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(AgentId::new);
    let mut role_line = String::new();
    if let Some(team) = team(hub, parent).await {
        let node = match &agent {
            Some(id) => team
                .iter()
                .find(|n| &n.agent == id)
                .ok_or_else(|| format!("{id} is not in the Orchestra team; use list_agents"))?,
            None => {
                let list = hub.list_agents().await.unwrap_or_default();
                team.iter()
                    .filter(|n| n.role == OrchestraRole::Subagent)
                    .find(|n| {
                        list.iter()
                            .any(|a| a.id == n.agent && agent_state(a) == "available")
                    })
                    .ok_or("no available Subagent in the Orchestra team; pass an agent from list_agents")?
            }
        };
        agent = Some(node.agent.clone());
        role_line = format!("[Harness Orchestra] Your role: {}.", node.role.label());
        if !node.instructions.trim().is_empty() {
            role_line = format!("{role_line}\n{}", node.instructions.trim());
        }
    }
    if let Some(agent) = &agent {
        hub.ctx
            .require_available(agent)
            .map_err(|e| e.to_string())?;
    }
    let title = args["title"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| task.lines().next().unwrap_or_default())
        .chars()
        .take(60)
        .collect::<String>();
    let (child, send_agent) =
        if let Some(id) = args["session_id"].as_str().filter(|s| !s.is_empty()) {
            {
                let child = SessionId::new(id);
                let known = hub.ctx.subagents.get(&child);
                if known.as_ref().map(|v| &v.parent_id) != Some(&parent.id) {
                    return Err(format!("{id} is not a subagent of this session"));
                }
                (child, agent)
            }
        } else {
            {
                let request = CreateSession {
                    workspace_path: parent.workspace_path.clone(),
                    workspace_identity: Some(parent.workspace_key.clone()),
                    title: Some(title.clone()),
                    agent,
                    worktree: None,
                };
                let summary = hub
                    .create_child_session(request, &parent.id)
                    .await
                    .map_err(|e| e.to_string())?;
                // New subagents start in Machine mode (every permission request is approved), so they
                // never stop to ask (user request 2026-11-05). The user can turn it off in the subagent's
                // session.
                if let Err(error) = hub.set_machine(summary.id.clone(), true).await {
                    tracing::warn!(session = %summary.id, %error, "subagent machine mode not applied");
                }
                // §9.8.1：新子会话的第一轮附带协作说明。
                task = if role_line.is_empty() {
                    format!("{PREAMBLE}\n\n{task}")
                } else {
                    format!("{PREAMBLE}\n\n{role_line}\n\n{task}")
                };
                (summary.id, None)
            }
        };
    let mut events = hub.ctx.events.subscribe();
    let accepted = hub
        .send_message(SendMessage {
            session_id: child.clone(),
            text: task,
            agent: send_agent,
            client_message_id: None,
            context: Vec::new(),
        })
        .await
        .map_err(|e| e.to_string())?;
    if background {
        let agent = hub
            .ctx
            .subagents
            .get(&child)
            .map_or_else(|| parent.active_agent.clone(), |v| v.agent);
        return Ok(json!({
            "sessionId": child,
            "turnId": accepted.turn_id,
            "agent": agent,
            "agentName": hub.ctx.display_name(&agent),
            "background": true,
        }));
    }
    let wait = async {
        loop {
            match events.recv().await {
                Ok(event) if event.session_id.as_ref() == Some(&child) => {
                    if matches!(&event.kind, EventKind::TurnFinished { turn_id, .. } if turn_id == &accepted.turn_id)
                    {
                        return true;
                    }
                }
                Ok(_) => {}
                // 事件丢失时以会话快照为准，避免漏掉 TurnFinished 而永远等待。
                Err(RecvError::Lagged(_)) => {
                    if turn_ended(hub, &child, &accepted.turn_id).await {
                        return true;
                    }
                }
                Err(RecvError::Closed) => return false,
            }
        }
    };
    tokio::select! {
        finished = wait => {
            if !finished {
                return Err("Harness is shutting down".into());
            }
        }
        () = closed => {
            // The parent gave up waiting (e.g. the user stopped it): the subagent keeps running and its
            // answer is pushed to the parent when it finishes (`push`).
            return Err("cancelled".into());
        }
    }
    hub.ctx
        .subagents
        .coord
        .mark_reported(&parent.id, &child, &accepted.turn_id);
    result(hub, &child, &accepted.turn_id).await
}

/// Coordination note put before a new subagent's first task (§9.8.1).
const PREAMBLE: &str = "[Harness] You are a subagent working for another agent in this workspace; other agents may be editing files at the same time. Before you edit files, call the claim_paths tool (harness-delegate) with the files or folders you will change; if a path is held by someone else, do not edit it and use `notify_parent` to explain. Use `notify_parent` for important progress or when you are blocked. Your final answer is returned to the parent agent.";

async fn turn_ended(hub: &Arc<Hub>, child: &SessionId, turn: &harness_core_api::TurnId) -> bool {
    hub.open_session(child.clone()).await.is_ok_and(|d| {
        d.turns.iter().any(|t| {
            &t.id == turn
                && !matches!(
                    t.status,
                    TurnStatus::Queued | TurnStatus::Running | TurnStatus::Paused
                )
        })
    })
}

async fn result(
    hub: &Arc<Hub>,
    child: &SessionId,
    turn_id: &harness_core_api::TurnId,
) -> Result<Value, String> {
    let detail = hub
        .open_session(child.clone())
        .await
        .map_err(|e| e.to_string())?;
    let turn = detail
        .turns
        .iter()
        .find(|t| &t.id == turn_id)
        .ok_or("the subagent turn disappeared")?;
    let agent = turn
        .agent
        .clone()
        .unwrap_or_else(|| detail.summary.active_agent.clone());
    let outcome = match turn.status {
        TurnStatus::Completed => "done",
        TurnStatus::Cancelled => "cancelled",
        _ => "failed",
    };
    if let Some(mut view) = hub.ctx.subagents.get(child) {
        view.result_preview = preview(&turn.assistant_text);
        if view.status.is_active() {
            // 防御：TurnFinished 尚未被 tracker 处理时也要给出终态。
            view.status = if outcome == "done" {
                SubagentStatus::Done
            } else if outcome == "cancelled" {
                SubagentStatus::Cancelled
            } else {
                SubagentStatus::Failed
            };
            view.finished_at = Some(now_ms());
        }
        hub.ctx.put_subagent(view).await;
    }
    Ok(json!({
        "sessionId": child,
        "agent": agent,
        "agentName": hub.ctx.display_name(&agent),
        "outcome": outcome,
        "error": turn.error,
        "text": turn.assistant_text,
    }))
}

/// The view registered for a new subagent before its session becomes visible.
pub(crate) fn new_view(
    child: &SessionId,
    parent: &SessionId,
    agent: &AgentId,
    title: &str,
) -> SubagentView {
    SubagentView {
        session_id: child.clone(),
        parent_id: parent.clone(),
        agent: agent.clone(),
        title: title.to_owned(),
        status: SubagentStatus::Running,
        activity: String::new(),
        started_at: now_ms(),
        finished_at: None,
        result_preview: String::new(),
    }
}
