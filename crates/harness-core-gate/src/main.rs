//! Phase 8 core gate (spec phase-8-core.md §13). Run through `scripts/e2e/phase8-core.mjs`, which provides an
//! isolated `HARNESS_HOME`, measures idle RSS and verifies that every agent process is gone.
//!
//! Protocol on stdout: `STEP <name> OK`, `AGENT_PID <pid>`, `IDLE_RSS_CHECK <pid>` (then one stdin line is
//! awaited), finally `GATE_OK` or `GATE_FAIL <reason>`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use harness_agent_hub::agents::{AgentSpec, ErrorDialect, Transport};
use harness_agent_hub::{Hub, HubConfig};
use harness_core_api::{
    AgentId, AgentRuntimeState, CreateSession, EventKind, HarnessCore, HubEvent, MemoryScope,
    MemoryWriteMode, PauseReason, PermissionDecision, QuotaState, RecvError, SendMessage,
    SessionId, SwitchReason, TurnId, TurnStatus, TurnView,
};
use serde_json::Value;
use tokio::io::AsyncBufReadExt;

mod phase9;
mod phase9_admin;
mod phase9_chat;
mod phase9_coord;
mod phase9_delegate;
mod phase9_providers;
mod smoke;
mod workspace;

type R<T> = Result<T, String>;

const A: &str = "fake-a";
const B: &str = "fake-b";
const C: &str = "fake-c";
const IDLE: Duration = Duration::from_secs(2);
const TURN_TIMEOUT: Duration = Duration::from_secs(30);

fn check(condition: bool, message: impl Into<String>) -> R<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

fn step(name: &str) {
    println!("STEP {name} OK");
}

struct Env {
    home: PathBuf,
    workspace: PathBuf,
    state: PathBuf,
    log: PathBuf,
    bin_dir: PathBuf,
}

impl Env {
    fn from_env() -> R<Self> {
        let home = std::env::var_os("HARNESS_HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .ok_or("HARNESS_HOME must be set (isolated home)")?;
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let bin_dir = exe.parent().ok_or("exe has no parent")?.to_path_buf();
        let env = Self {
            workspace: home.join("workspace"),
            state: home.join("fake-state"),
            log: home.join("fake-agent.jsonl"),
            bin_dir,
            home,
        };
        for dir in [&env.home, &env.workspace, &env.state] {
            std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        }
        Ok(env)
    }

    fn exe(&self, name: &str) -> PathBuf {
        self.bin_dir.join(if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_owned()
        })
    }

    fn config(&self) -> HubConfig {
        let agent = |id: &str, name: &str| AgentSpec {
            id: AgentId::new(id),
            display_name: name.to_owned(),
            program: self
                .exe("harness-fake-agent")
                .to_string_lossy()
                .into_owned(),
            args: vec!["--name".into(), id.to_owned()],
            env: vec![
                (
                    "FAKE_AGENT_STATE".into(),
                    self.state.to_string_lossy().into_owned(),
                ),
                (
                    "FAKE_AGENT_LOG".into(),
                    self.log.to_string_lossy().into_owned(),
                ),
            ],
            install_hint: "build harness-fake-agent".into(),
            transport: Transport::Acp,
            dialect: ErrorDialect::Generic,
        };
        let agents = vec![agent(A, "Fake A"), agent(B, "Fake B"), agent(C, "Fake C")];
        HubConfig {
            home: self.home.clone(),
            default_fallback_order: agents.iter().map(|a| a.id.clone()).collect(),
            agents,
            agent_idle_timeout: IDLE,
            helpers: harness_agent_hub::helpers::HelperLaunch::Separate {
                memory_mcp: self.exe("harness-memory-mcp"),
                api_agent: self.exe("harness-api-agent"),
            },
            event_capacity: 8192,
            load_agents_file: false,
            turn_snapshots: true,
            agent_defaults: std::collections::HashMap::new(),
            agent_kinds: std::collections::HashMap::new(),
            auth_probe: harness_agent_hub::SharedAuthProbe::default(),
            file_search: None,
            // 门禁绝不指向真实 CLI 配置或历史目录：只用夹具（phase9-chat 自行设置）。
            config_paths: None,
            import_roots: harness_import::ImportRoots::default(),
            schedule_min_interval_secs: 1,
            agy_creds: harness_agent_hub::SharedAgyCredStore::default(),
        }
    }

    fn workspace(&self) -> String {
        self.workspace.to_string_lossy().into_owned()
    }

    fn log_entries(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// Prompt blocks every time `agent` received a prompt whose user message (last block) is `user`.
    fn prompts(&self, agent: &str, user: &str) -> Vec<Vec<String>> {
        self.log_entries()
            .into_iter()
            .filter(|e| e["event"] == "prompt" && e["agent"] == agent)
            .filter_map(|e| {
                let blocks: Vec<String> = e["blocks"]
                    .as_array()?
                    .iter()
                    .filter_map(|b| b.as_str().map(str::to_owned))
                    .collect();
                (blocks.last().map(String::as_str) == Some(user)).then_some(blocks)
            })
            .collect()
    }

    fn set_exhausted(&self, agent: &str, resets_at: i64) -> R<()> {
        std::fs::write(
            self.state.join(format!("{agent}.exhausted")),
            resets_at.to_string(),
        )
        .map_err(|e| e.to_string())
    }

    fn clear_exhausted(&self, agent: &str) {
        let _ = std::fs::remove_file(self.state.join(format!("{agent}.exhausted")));
    }
}

/// Live hub + event recorder.
struct Gate {
    hub: Arc<Hub>,
    events: Arc<Mutex<Vec<HubEvent>>>,
}

impl Gate {
    async fn start(env: &Env) -> R<Self> {
        let hub = Hub::start(env.config()).await.map_err(|e| e.to_string())?;
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut stream = hub.subscribe();
        let sink = Arc::clone(&events);
        tokio::spawn(async move {
            loop {
                match stream.recv().await {
                    Ok(event) => {
                        if let EventKind::AgentRuntime { pid: Some(pid), .. } = &event.kind {
                            println!("AGENT_PID {pid}");
                        }
                        if let Ok(mut all) = sink.lock() {
                            all.push(event);
                        }
                    }
                    Err(RecvError::Lagged(n)) => println!("NOTE events lagged by {n}"),
                    Err(RecvError::Closed) => break,
                }
            }
        });
        Ok(Self { hub, events })
    }

    fn events(&self) -> Vec<HubEvent> {
        self.events.lock().map(|e| e.clone()).unwrap_or_default()
    }

    async fn session(&self, env: &Env, agent: &str, title: &str) -> R<SessionId> {
        let summary = self
            .hub
            .create_session(CreateSession {
                workspace_path: env.workspace(),
                workspace_identity: None,
                title: Some(title.into()),
                agent: Some(AgentId::new(agent)),
                worktree: None,
            })
            .await
            .map_err(|e| format!("create_session: {e}"))?;
        Ok(summary.id)
    }

    async fn send(&self, session: &SessionId, text: &str, agent: Option<&str>) -> R<TurnId> {
        let accepted = self
            .hub
            .send_message(SendMessage {
                session_id: session.clone(),
                text: text.into(),
                agent: agent.map(AgentId::new),
                client_message_id: None,
                context: Vec::new(),
            })
            .await
            .map_err(|e| format!("send_message: {e}"))?;
        Ok(accepted.turn_id)
    }

    async fn turn(&self, session: &SessionId, turn: &TurnId) -> R<TurnView> {
        let detail = self
            .hub
            .open_session(session.clone())
            .await
            .map_err(|e| format!("open_session: {e}"))?;
        detail
            .turns
            .into_iter()
            .find(|t| &t.id == turn)
            .ok_or_else(|| format!("turn {turn} missing"))
    }

    async fn wait_turn(
        &self,
        session: &SessionId,
        turn: &TurnId,
        done: impl Fn(&TurnView) -> bool,
        what: &str,
    ) -> R<TurnView> {
        let deadline = Instant::now() + TURN_TIMEOUT;
        loop {
            let view = self.turn(session, turn).await?;
            if done(&view) {
                return Ok(view);
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "timeout waiting for {what}: status {:?}, text {:?}",
                    view.status, view.assistant_text
                ));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn finished(&self, session: &SessionId, turn: &TurnId) -> R<TurnView> {
        self.wait_turn(
            session,
            turn,
            |t| {
                matches!(
                    t.status,
                    TurnStatus::Completed | TurnStatus::Cancelled | TurnStatus::Failed
                )
            },
            "turn end",
        )
        .await
    }

    async fn completed(&self, session: &SessionId, turn: &TurnId) -> R<TurnView> {
        let view = self.finished(session, turn).await?;
        check(
            view.status == TurnStatus::Completed,
            format!("turn {turn} ended {:?} ({:?})", view.status, view.error),
        )?;
        Ok(view)
    }

    async fn wait_event(&self, what: &str, find: impl Fn(&HubEvent) -> bool) -> R<HubEvent> {
        let deadline = Instant::now() + TURN_TIMEOUT;
        loop {
            if let Some(event) = self.events().into_iter().find(|e| find(e)) {
                return Ok(event);
            }
            if Instant::now() > deadline {
                return Err(format!("timeout waiting for event: {what}"));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn event_index(&self, find: impl Fn(&EventKind) -> bool) -> Option<usize> {
        self.events().iter().position(|e| find(&e.kind))
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The OS process-list line of `pid` (diagnostics: tells PID reuse apart from a surviving agent).
fn process_line(pid: u32) -> String {
    let output = if cfg!(windows) {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
    } else {
        std::process::Command::new("ps")
            .args(["-o", "comm=", "-p", &pid.to_string()])
            .output()
    };
    output
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

fn pid_alive(pid: u32) -> bool {
    let output = if cfg!(windows) {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
    } else {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
    };
    match output {
        Ok(out) if cfg!(windows) => {
            String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
        }
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("smoke") {
        return smoke::run(&args[1..]).await;
    }
    if args.first().map(String::as_str) == Some("phase9-settings") {
        return phase9::main().await;
    }
    if args.first().map(String::as_str) == Some("phase9-chat") {
        return phase9_chat::main().await;
    }
    match run().await {
        Ok(()) => {
            println!("GATE_OK");
            std::process::ExitCode::SUCCESS
        }
        Err(reason) => {
            println!("GATE_FAIL {reason}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> R<()> {
    let env = Env::from_env()?;
    // 隔离 home 位于 ZCode 仓库内（Worse/.work/tmp）；没有 ceiling 时 git 会向上找到用户仓库，快照会写入其对象库。
    check(
        std::env::var_os("GIT_CEILING_DIRECTORIES").is_some_and(|v| !v.is_empty()),
        "GIT_CEILING_DIRECTORIES must be set (the isolated home is inside a repository)",
    )?;
    let gate = Gate::start(&env).await?;
    let result = scenarios(&env, gate).await;
    // Never leave fake agents behind, whatever happened.
    for agent in [A, B, C] {
        env.clear_exhausted(agent);
    }
    result
}

async fn scenarios(env: &Env, gate: Gate) -> R<()> {
    let s1 = basics(env, &gate).await?;
    tools_and_memory(env, &gate, &s1).await?;
    workspace::run(env, &gate).await?;
    let s2 = agent_switch(env, &gate).await?;
    let s3 = failover(env, &gate).await?;
    sticky_and_swap(env, &gate, &s3).await?;
    exhaustion(env, &gate, &s3).await?;
    idle(&gate).await?;
    restart(env, gate, &s2, &s3).await
}

async fn basics(env: &Env, gate: &Gate) -> R<SessionId> {
    // ---- memory injection + send/stream ------------------------------------------------------------
    gate.hub
        .memory_write(
            MemoryScope::Global,
            "GLOBAL-MARK-7".into(),
            MemoryWriteMode::Replace,
        )
        .await
        .map_err(|e| e.to_string())?;
    gate.hub
        .memory_write(
            MemoryScope::Project {
                workspace_path: env.workspace(),
            },
            "PROJECT-MARK-9".into(),
            MemoryWriteMode::Replace,
        )
        .await
        .map_err(|e| e.to_string())?;
    let s1 = gate.session(env, A, "basic").await?;
    let text = "hello stream [[slow 5]]";
    let t = gate.send(&s1, text, None).await?;
    let view = gate.completed(&s1, &t).await?;
    check(
        view.assistant_text.contains("chunk-4"),
        format!("stream text: {}", view.assistant_text),
    )?;
    let deltas = gate
        .events()
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::MessageDelta { turn_id, .. } if turn_id == &t))
        .count();
    check(deltas >= 6, format!("expected >=6 deltas, got {deltas}"))?;
    step("send_stream");
    let blocks = env
        .prompts(A, text)
        .pop()
        .ok_or("fake-a prompt not logged")?;
    let joined = blocks.join("\n");
    check(
        joined.contains("<harness-memory")
            && joined.contains("GLOBAL-MARK-7")
            && joined.contains("PROJECT-MARK-9"),
        "MEMORY.md injection block missing from the agent context",
    )?;
    check(
        view.assistant_text.contains("memory=true"),
        "agent did not see memory",
    )?;
    step("memory_injection");

    // ---- stop mid-stream --------------------------------------------------------------------------
    let t = gate.send(&s1, "long [[slow 200]]", None).await?;
    gate.wait_event("first delta of long turn", |e| {
        matches!(&e.kind, EventKind::MessageDelta { turn_id, text, .. } if turn_id == &t && text.starts_with("chunk-"))
    })
    .await?;
    let stopped_at = Instant::now();
    gate.hub.stop(s1.clone()).await.map_err(|e| e.to_string())?;
    let view = gate.finished(&s1, &t).await?;
    check(
        view.status == TurnStatus::Cancelled,
        format!("stop: status {:?}", view.status),
    )?;
    check(
        stopped_at.elapsed() < Duration::from_secs(3),
        "stop took too long",
    )?;
    check(
        !view.assistant_text.contains("chunk-199"),
        "stream was not interrupted",
    )?;
    step("stop_mid_stream");

    // ---- queued second message ordering -----------------------------------------------------------
    let first = gate.send(&s1, "first [[slow 8]]", None).await?;
    let second = gate.send(&s1, "second", None).await?;
    let detail = gate
        .hub
        .open_session(s1.clone())
        .await
        .map_err(|e| e.to_string())?;
    check(detail.queue.contains(&second), "second message not queued")?;
    gate.completed(&s1, &first).await?;
    gate.completed(&s1, &second).await?;
    let first_end = gate
        .event_index(|k| matches!(k, EventKind::TurnFinished { turn_id, .. } if turn_id == &first))
        .ok_or("no finish event for first")?;
    let second_start = gate
        .event_index(|k| matches!(k, EventKind::TurnStarted { turn_id, .. } if turn_id == &second))
        .ok_or("no start event for second")?;
    check(
        first_end < second_start,
        "second turn started before the first finished",
    )?;
    step("queue_order");
    Ok(s1)
}

async fn tools_and_memory(env: &Env, gate: &Gate, s1: &SessionId) -> R<()> {
    let s1 = s1.clone();
    // ---- permissions ------------------------------------------------------------------------------
    for (option, expect) in [
        ("allow", "permission=allowed"),
        ("deny", "permission=denied"),
    ] {
        let t = gate
            .send(&s1, &format!("tool {option} [[permission]]"), None)
            .await?;
        let event = gate
            .wait_event("permission request", |e| {
                matches!(&e.kind, EventKind::PermissionRequested { request } if request.turn_id == t)
            })
            .await?;
        let EventKind::PermissionRequested { request } = event.kind else {
            return Err("unexpected event".into());
        };
        gate.hub
            .respond_permission(s1.clone(), request.request_id.clone(), option.into())
            .await
            .map_err(|e| e.to_string())?;
        let view = gate.completed(&s1, &t).await?;
        check(
            view.assistant_text.contains(expect),
            format!("{option}: {}", view.assistant_text),
        )?;
        check(
            matches!(view.permissions.first(), Some((_, PermissionDecision::Selected { option_id, .. })) if option_id == option),
            format!(
                "permission decision not recorded on the turn: {:?}",
                view.permissions
            ),
        )?;
        step(&format!("permission_{option}"));
    }

    // ---- memory via MCP across two different agents ------------------------------------------------
    let t = gate
        .send(
            &s1,
            "[[mem-write project shared-from-a]] [[mem-write global global-from-a]]",
            Some(A),
        )
        .await?;
    let view = gate.completed(&s1, &t).await?;
    check(
        view.assistant_text.contains("mem-write[project]=<<ok>>"),
        format!("mcp write: {}", view.assistant_text),
    )?;
    let t = gate
        .send(&s1, "[[mem-read project]] [[mem-read global]]", Some(B))
        .await?;
    let view = gate.completed(&s1, &t).await?;
    check(
        view.agent.as_ref().map(AgentId::as_str) == Some(B)
            && view.assistant_text.contains("shared-from-a")
            && view.assistant_text.contains("global-from-a"),
        format!(
            "fake-b did not read fake-a's memory: {}",
            view.assistant_text
        ),
    )?;
    let project = gate
        .hub
        .memory_read(MemoryScope::Project {
            workspace_path: env.workspace(),
        })
        .await
        .map_err(|e| e.to_string())?;
    check(
        project.contains("shared-from-a"),
        "project MEMORY.md lacks the MCP write",
    )?;
    check(
        env.workspace.join(".harness").join("MEMORY.md").is_file()
            && !env.workspace.join("CLAUDE.md").exists()
            && !env.workspace.join("AGENTS.md").exists()
            && !env.workspace.join("GEMINI.md").exists(),
        "memory must live in <project>/.harness/MEMORY.md and never in repo rule files",
    )?;
    step("memory_mcp_cross_agent");
    Ok(())
}

async fn agent_switch(env: &Env, gate: &Gate) -> R<SessionId> {
    // ---- agent switch catch-up --------------------------------------------------------------------
    let s2 = gate.session(env, A, "switch").await?;
    let t1 = gate.send(&s2, "alpha-one", None).await?;
    gate.completed(&s2, &t1).await?;
    let t2 = gate.send(&s2, "beta-two", Some(B)).await?;
    gate.completed(&s2, &t2).await?;
    let b_prompt = env
        .prompts(B, "beta-two")
        .pop()
        .ok_or("fake-b prompt missing")?
        .join("\n");
    check(
        b_prompt.contains("kind=\"seed\"") && b_prompt.contains("alpha-one"),
        "second agent did not receive the earlier turn (seed)",
    )?;
    let t3 = gate.send(&s2, "beta-three", None).await?;
    let v3 = gate.completed(&s2, &t3).await?;
    check(
        v3.agent.as_ref().map(AgentId::as_str) == Some(B),
        "sticky agent was not fake-b",
    )?;
    let t4 = gate.send(&s2, "alpha-four", Some(A)).await?;
    gate.completed(&s2, &t4).await?;
    let a_prompt = env
        .prompts(A, "alpha-four")
        .pop()
        .ok_or("fake-a prompt missing")?
        .join("\n");
    check(
        a_prompt.contains("kind=\"catch-up\"")
            && a_prompt.contains("beta-two")
            && a_prompt.contains("beta-three")
            && !a_prompt.contains("alpha-one"),
        format!("first agent did not receive exactly the missed turns: {a_prompt}"),
    )?;
    step("agent_switch_catchup");
    Ok(s2)
}

async fn failover(env: &Env, gate: &Gate) -> R<SessionId> {
    // ---- failover mid-turn ------------------------------------------------------------------------
    let s3 = gate.session(env, A, "failover").await?;
    let user = "do work [[quota-mid fake-a]]";
    let t = gate.send(&s3, user, None).await?;
    let view = gate.completed(&s3, &t).await?;
    check(
        view.agent.as_ref().map(AgentId::as_str) == Some(B),
        format!("failover served by {:?}", view.agent),
    )?;
    check(
        view.attempts.len() == 2
            && view.attempts[0].agent.as_str() == A
            && view.attempts[0].status == "quotaExhausted"
            && view.attempts[0].partial_text.contains("partial-2")
            && view.attempts[1].agent.as_str() == B,
        format!("attempts: {:?}", view.attempts),
    )?;
    check(env.prompts(A, user).len() == 1, "turn ran twice on fake-a")?;
    let b_blocks = env
        .prompts(B, user)
        .pop()
        .ok_or("fake-b failover prompt missing")?;
    check(
        b_blocks.iter().filter(|b| b.contains(user)).count() == 1,
        "user message duplicated in the failover prompt",
    )?;
    check(
        b_blocks
            .iter()
            .any(|b| b.contains("<harness-failover") && b.contains("partial-0")),
        "failed agent's partial output not forwarded",
    )?;
    let switched = gate
        .wait_event("failover switch event", |e| {
            e.session_id.as_ref() == Some(&s3)
                && matches!(
                    &e.kind,
                    EventKind::AgentSwitched {
                        reason: SwitchReason::QuotaExhausted { resets_at: Some(_) },
                        ..
                    }
                )
        })
        .await?;
    if let EventKind::AgentSwitched { message, .. } = &switched.kind {
        check(
            message.starts_with("switched from Fake A to Fake B: Fake A limit reached (resets at "),
            format!("switch message: {message}"),
        )?;
    }
    let agents = gate.hub.list_agents().await.map_err(|e| e.to_string())?;
    check(
        agents.iter().any(|a| {
            a.id.as_str() == A
                && matches!(
                    a.quota,
                    QuotaState::Exhausted {
                        resets_at: Some(_),
                        ..
                    }
                )
        }),
        "fake-a not marked exhausted with a reset time",
    )?;
    step("failover_mid_turn");

    Ok(s3)
}

async fn sticky_and_swap(env: &Env, gate: &Gate, s3: &SessionId) -> R<()> {
    let s3 = s3.clone();
    // ---- sticky after failover --------------------------------------------------------------------
    let t = gate.send(&s3, "after failover", None).await?;
    let view = gate.completed(&s3, &t).await?;
    check(
        view.agent.as_ref().map(AgentId::as_str) == Some(B),
        "did not stay on fake-b",
    )?;
    check(
        env.prompts(A, "after failover").is_empty(),
        "switched back to the exhausted agent",
    )?;
    let detail = gate
        .hub
        .open_session(s3.clone())
        .await
        .map_err(|e| e.to_string())?;
    check(
        detail.summary.active_agent.as_str() == B,
        "sticky agent not fake-b",
    )?;
    step("sticky_after_failover");

    // ---- manual swap ------------------------------------------------------------------------------
    gate.hub
        .set_active_agent(s3.clone(), AgentId::new(A))
        .await
        .map_err(|e| e.to_string())?;
    gate.wait_event("manual switch event", |e| {
        e.session_id.as_ref() == Some(&s3)
            && matches!(&e.kind, EventKind::AgentSwitched { reason: SwitchReason::Manual, to, .. } if to.as_str() == A)
    })
    .await?;
    let t = gate.send(&s3, "back on a", None).await?;
    let view = gate.completed(&s3, &t).await?;
    check(
        view.agent.as_ref().map(AgentId::as_str) == Some(A),
        "manual swap ignored",
    )?;
    let a_prompt = env
        .prompts(A, "back on a")
        .pop()
        .ok_or("fake-a prompt missing")?
        .join("\n");
    check(
        a_prompt.contains("after failover"),
        "swapped-in agent missed the turns served by fake-b",
    )?;
    step("manual_swap");

    // ---- failover at start ------------------------------------------------------------------------
    let user = "start fails [[quota-start fake-a]]";
    let t = gate.send(&s3, user, None).await?;
    let view = gate.completed(&s3, &t).await?;
    check(
        view.agent.as_ref().map(AgentId::as_str) == Some(B)
            && view
                .attempts
                .first()
                .is_some_and(|a| a.status == "quotaExhausted" && a.partial_text.is_empty()),
        format!("start failover attempts: {:?}", view.attempts),
    )?;
    step("failover_at_start");
    Ok(())
}

async fn exhaustion(env: &Env, gate: &Gate, s3: &SessionId) -> R<()> {
    let s3 = s3.clone();
    // ---- all exhausted: pause, resume by manual swap ----------------------------------------------
    let far = now_secs() + 3600;
    for agent in [A, B, C] {
        env.set_exhausted(agent, far)?;
    }
    let user = "everyone is out";
    let t = gate.send(&s3, user, None).await?;
    let view = gate
        .wait_turn(&s3, &t, |v| v.status == TurnStatus::Paused, "paused turn")
        .await?;
    // fake-a is still marked exhausted from the start-failover step, so this round tries fake-b and fake-c only.
    let tried: Vec<&str> = view.attempts.iter().map(|a| a.agent.as_str()).collect();
    check(
        tried == [B, C] && view.attempts.iter().all(|a| a.status == "quotaExhausted"),
        format!("paused after attempts {tried:?}"),
    )?;
    let paused = gate
        .wait_event(
            "pause event",
            |e| matches!(&e.kind, EventKind::TurnPaused { turn_id, .. } if turn_id == &t),
        )
        .await?;
    if let EventKind::TurnPaused {
        reason: PauseReason::AllAgentsExhausted { agents },
        ..
    } = &paused.kind
    {
        check(
            agents.len() == 3 && agents.iter().all(|(_, reset)| reset.is_some()),
            format!("pause reason: {agents:?}"),
        )?;
    }
    // A queued message behind a paused turn stays queued.
    let behind = gate.send(&s3, "queued behind pause", None).await?;
    env.clear_exhausted(B);
    gate.hub
        .set_active_agent(s3.clone(), AgentId::new(B))
        .await
        .map_err(|e| e.to_string())?;
    let view = gate.completed(&s3, &t).await?;
    check(
        view.agent.as_ref().map(AgentId::as_str) == Some(B),
        "resumed on wrong agent",
    )?;
    let b_blocks = env.prompts(B, user);
    check(
        b_blocks.len() == 2,
        format!("fake-b prompts for paused turn: {}", b_blocks.len()),
    )?;
    check(
        b_blocks
            .last()
            .is_some_and(|blocks| blocks.iter().filter(|b| b.contains(user)).count() == 1),
        "user message duplicated after resume",
    )?;
    gate.completed(&s3, &behind).await?;
    step("all_exhausted_pause_manual_resume");

    // ---- all exhausted: resume when a reset passes -------------------------------------------------
    let soon = now_secs() + 2;
    for agent in [A, B, C] {
        env.set_exhausted(agent, soon)?;
    }
    let user = "wait for reset";
    let t = gate.send(&s3, user, None).await?;
    gate.wait_turn(&s3, &t, |v| v.status == TurnStatus::Paused, "paused turn")
        .await?;
    for agent in [A, B, C] {
        env.clear_exhausted(agent);
    }
    let view = gate.completed(&s3, &t).await?;
    // Agents still marked exhausted (far reset) are skipped; the turn resumes once fake-b's reset passes.
    check(
        view.attempts.len() >= 2
            && view
                .attempts
                .last()
                .is_some_and(|a| a.status == "completed"),
        format!("reset resume attempts: {:?}", view.attempts),
    )?;
    step("all_exhausted_pause_reset_resume");
    Ok(())
}

async fn idle(gate: &Gate) -> R<()> {
    // ---- idle shutdown ----------------------------------------------------------------------------
    let deadline = Instant::now() + IDLE + Duration::from_secs(15);
    loop {
        let agents = gate.hub.list_agents().await.map_err(|e| e.to_string())?;
        if agents
            .iter()
            .all(|a| a.runtime == AgentRuntimeState::Stopped)
        {
            break;
        }
        check(
            Instant::now() < deadline,
            format!(
                "agents not stopped after idle: {:?}",
                agents.iter().map(|a| a.runtime).collect::<Vec<_>>()
            ),
        )?;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let pids: Vec<u32> = gate
        .events()
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::AgentRuntime { pid: Some(pid), .. } => Some(*pid),
            _ => None,
        })
        .collect();
    check(!pids.is_empty(), "no agent pid observed")?;
    // 修复说明：门禁偶发 "agent processes still alive after idle stop"。诊断显示失败 pid 的事件序列以正常
    // Stopped 结尾，且生成失败消息时 tasklist 已查不到该进程：hub 已 wait 到退出，但 Windows 进程列表短暂仍列出
    // 正在销毁的进程。存活判定因此在短时间内复查（进程真的残留时 3 秒后仍会失败），而不是一次快照即判失败。
    let mut alive: Vec<u32> = pids.iter().copied().filter(|p| pid_alive(*p)).collect();
    let recheck = Instant::now() + Duration::from_secs(3);
    while !alive.is_empty() && Instant::now() < recheck {
        tokio::time::sleep(Duration::from_millis(200)).await;
        alive.retain(|p| pid_alive(*p));
    }
    let history = |pid: u32| -> Vec<String> {
        gate.events()
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::AgentRuntime {
                    agent,
                    state,
                    pid: Some(p),
                } if *p == pid => Some(format!("{agent}:{state:?}")),
                _ => None,
            })
            .collect()
    };
    check(
        alive.is_empty(),
        format!(
            "agent processes still alive after idle stop: {:?}",
            alive
                .iter()
                .map(|p| (*p, history(*p), process_line(*p)))
                .collect::<Vec<_>>()
        ),
    )?;
    step("idle_shutdown");

    // ---- idle RSS of the core (measured by the driver script) -------------------------------------
    println!("IDLE_RSS_CHECK {}", std::process::id());
    let mut line = String::new();
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let _ = tokio::time::timeout(Duration::from_secs(30), stdin.read_line(&mut line)).await;
    println!("IDLE_RSS_ACK {}", line.trim());
    Ok(())
}

async fn restart(env: &Env, gate: Gate, s2: &SessionId, s3: &SessionId) -> R<()> {
    let s2 = s2.clone();
    // ---- resume after restart ---------------------------------------------------------------------
    let handle_before = env
        .log_entries()
        .into_iter()
        .filter(|e| e["event"] == "new" && e["agent"] == A)
        .count();
    gate.hub.shutdown().await.map_err(|e| e.to_string())?;
    drop(gate);
    let gate = Gate::start(env).await?;
    let detail = gate
        .hub
        .open_session(s2.clone())
        .await
        .map_err(|e| e.to_string())?;
    check(
        detail.turns.len() == 4,
        format!("transcript after restart: {} turns", detail.turns.len()),
    )?;
    check(
        detail.summary.active_agent.as_str() == A,
        "sticky agent lost on restart",
    )?;
    // Regression: every attempt of a turn resumed after a pause is persisted (fake-b ran it in two rounds).
    let failover_detail = gate
        .hub
        .open_session(s3.clone())
        .await
        .map_err(|e| e.to_string())?;
    let resumed = failover_detail
        .turns
        .iter()
        .find(|t| t.user_text == "everyone is out")
        .ok_or("resumed turn missing after restart")?;
    let tried: Vec<&str> = resumed.attempts.iter().map(|a| a.agent.as_str()).collect();
    check(
        tried == [B, C, B] && resumed.status == TurnStatus::Completed,
        format!(
            "persisted attempts after restart: {tried:?} {:?}",
            resumed.status
        ),
    )?;
    // fake-a is still marked exhausted (persisted, far reset); a manual swap clears it.
    gate.hub
        .set_active_agent(s2.clone(), AgentId::new(A))
        .await
        .map_err(|e| e.to_string())?;
    let loads_before = env
        .log_entries()
        .iter()
        .filter(|e| e["event"] == "load")
        .count();
    let t = gate.send(&s2, "after restart", None).await?;
    let view = gate.completed(&s2, &t).await?;
    check(
        view.agent.as_ref().map(AgentId::as_str) == Some(A),
        "restart turn on wrong agent",
    )?;
    let entries = env.log_entries();
    check(
        entries.iter().filter(|e| e["event"] == "load").count() > loads_before,
        "native session was not resumed with session/load",
    )?;
    check(
        entries
            .iter()
            .filter(|e| e["event"] == "new" && e["agent"] == A)
            .count()
            == handle_before,
        "a new native session was created instead of resuming",
    )?;
    let blocks = env
        .prompts(A, "after restart")
        .pop()
        .ok_or("restart prompt missing")?
        .join("\n");
    check(
        !blocks.contains("kind=\"seed\""),
        "resumed session was re-seeded",
    )?;
    step("resume_after_restart");

    gate.hub.shutdown().await.map_err(|e| e.to_string())?;
    let pids: Vec<u32> = gate
        .events()
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::AgentRuntime { pid: Some(pid), .. } => Some(*pid),
            _ => None,
        })
        .collect();
    let alive: Vec<u32> = pids.iter().copied().filter(|p| pid_alive(*p)).collect();
    check(
        alive.is_empty(),
        format!("agent processes alive after hub shutdown: {alive:?}"),
    )?;
    step("shutdown_cleanup");
    Ok(())
}
