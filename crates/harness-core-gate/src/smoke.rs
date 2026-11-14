//! `harness-core-gate smoke <agent-id>... --workspace <dir>`: MANUAL smoke test against the REAL agent CLIs
//! (built-in adapters: claude-code, codex, agy). It consumes the user's subscription, so it only runs when the
//! user starts it through `scripts/manual/phase8-real-cli-smoke.mjs` with the explicit confirmation flag.
//!
//! Per agent: one session, one short prompt, one MCP memory round trip request, stream printed, outcome printed.
//! Harness data goes to `HARNESS_HOME` (set by the driver to a throwaway directory).

use std::process::ExitCode;
use std::time::Duration;

use harness_agent_hub::{Hub, HubConfig};
use harness_core_api::{
    AgentId, Availability, CreateSession, EventKind, HarnessCore, PermissionOptionKind,
    SendMessage, TurnStatus,
};

const PROMPT: &str =
    "Harness smoke test. Reply with the single word READY. Then call the harness-memory MCP tool \
memory_write with scope \"project\" and content \"smoke-ok\", and say DONE.";

pub(crate) async fn run(args: &[String]) -> ExitCode {
    let mut agents = Vec::new();
    let mut workspace = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--workspace" {
            workspace = iter.next().cloned();
        } else {
            agents.push(arg.clone());
        }
    }
    let Some(workspace) = workspace else {
        println!("SMOKE_FAIL --workspace <dir> is required");
        return ExitCode::FAILURE;
    };
    let config = match HubConfig::from_env() {
        Ok(config) => config,
        Err(error) => {
            println!("SMOKE_FAIL config: {error}");
            return ExitCode::FAILURE;
        }
    };
    let hub = match Hub::start(config).await {
        Ok(hub) => hub,
        Err(error) => {
            println!("SMOKE_FAIL hub: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut failed = false;
    for agent in &agents {
        match smoke_one(&hub, agent, &workspace).await {
            Ok(summary) => println!("SMOKE_AGENT_OK {agent} {summary}"),
            Err(error) => {
                failed = true;
                println!("SMOKE_AGENT_FAIL {agent} {error}");
            }
        }
    }
    let _ = hub.shutdown().await;
    if failed {
        ExitCode::FAILURE
    } else {
        println!("SMOKE_OK");
        ExitCode::SUCCESS
    }
}

async fn smoke_one(hub: &Hub, agent: &str, workspace: &str) -> Result<String, String> {
    let agents = hub.list_agents().await.map_err(|e| e.to_string())?;
    let info = agents
        .iter()
        .find(|a| a.id.as_str() == agent)
        .ok_or_else(|| format!("unknown agent {agent}"))?;
    if let Availability::NotInstalled { hint } = &info.availability {
        return Err(format!("not installed: {hint}"));
    }
    let mut events = hub.subscribe();
    let session = hub
        .create_session(CreateSession {
            workspace_path: workspace.to_owned(),
            workspace_identity: None,
            title: Some(format!("smoke {agent}")),
            agent: Some(AgentId::new(agent)),
            worktree: None,
        })
        .await
        .map_err(|e| e.to_string())?;
    let accepted = hub
        .send_message(SendMessage {
            session_id: session.id.clone(),
            text: PROMPT.into(),
            agent: None,
            client_message_id: None,
            context: Vec::new(),
        })
        .await
        .map_err(|e| e.to_string())?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .map_err(|_| "timeout after 300 s".to_owned())?
            .map_err(|e| e.to_string())?;
        match event.kind {
            EventKind::MessageDelta { text, .. } => print!("{text}"),
            EventKind::PermissionRequested { request } => {
                // Smoke runs never grant tool permissions automatically except the Harness memory tool.
                let allow = request.tool.title.contains("memory_write");
                let wanted = if allow {
                    PermissionOptionKind::AllowOnce
                } else {
                    PermissionOptionKind::RejectOnce
                };
                let option = request
                    .options
                    .iter()
                    .find(|o| o.kind == wanted)
                    .map(|o| o.option_id.clone());
                println!(
                    "\n[permission {} -> {}]",
                    request.tool.title,
                    if allow { "allow" } else { "reject" }
                );
                if let Some(option) = option {
                    let _ = hub
                        .respond_permission(session.id.clone(), request.request_id, option)
                        .await;
                }
            }
            EventKind::AgentSwitched { message, .. } => println!("\n[{message}]"),
            EventKind::TurnFinished { turn_id, .. } if turn_id == accepted.turn_id => break,
            _ => {}
        }
    }
    println!();
    let detail = hub
        .open_session(session.id)
        .await
        .map_err(|e| e.to_string())?;
    let turn = detail
        .turns
        .iter()
        .find(|t| t.id == accepted.turn_id)
        .ok_or("turn missing")?;
    let memory = hub
        .memory_read(harness_core_api::MemoryScope::Project {
            workspace_path: workspace.to_owned(),
        })
        .await
        .unwrap_or_default();
    if turn.status != TurnStatus::Completed {
        return Err(format!(
            "turn {:?}: {:?} attempts={:?}",
            turn.status, turn.error, turn.attempts
        ));
    }
    Ok(format!(
        "served_by={:?} chars={} memory_written={}",
        turn.agent.as_ref().map(AgentId::as_str),
        turn.assistant_text.len(),
        memory.contains("smoke-ok")
    ))
}
