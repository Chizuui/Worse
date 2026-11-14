//! Phase 9.2 core gate (spec phase-9-parity.md §9.2.4): organize, fork, rewind / edit / retry, file undo, find,
//! context blocks, slash commands, plan / usage / tool content, permission options, command search, read-only
//! import and scheduled prompts; then the §9.2.5 follow-ups (queue ops, pickers, typed failures, applied flag,
//! git ops, projects tree) in [more]. Run through `scripts/e2e/phase9-chat.mjs`, which provides the isolated home
//! (`HOME`, `USERPROFILE`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME`). Import roots are fixtures written here; the real
//! `~/.zcode` and `~/.claude` are never read.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use harness_agent_config::{AgentKind, ConfigPaths};
use harness_agent_hub::{HubConfig, SharedFileSearch};
use harness_core_api::{
    ActionEntry, AgentId, ApiError, ApplyFileChanges, BlockKind, ChangeDirection, CommandSearch,
    CommandSource, ContextItem, CreateSession, EditResend, EventKind, FileAction, FindField,
    FindInSession, ForkSession, GroupColor, HarnessCore, ImportRequest, ImportScan, ImportSource,
    ListSessions, MentionKind, MentionQuery, NewSchedule, PermissionDecision, PermissionOptionKind,
    Retry, Rewind, ScheduleKind, ScheduleTarget, SearchScope, SendMessage, SessionId,
    SessionSummary, TurnId, TurnStatus, UpdateGroup, UpdateSchedule,
};
use serde_json::json;

use super::{check, step, Env, Gate, A, B, C, R};

mod more;

pub async fn main() -> std::process::ExitCode {
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

fn io<T>(what: &str, r: std::io::Result<T>) -> R<T> {
    r.map_err(|e| format!("{what}: {e}"))
}

fn api<T>(what: &str, r: Result<T, ApiError>) -> R<T> {
    r.map_err(|e| format!("{what}: {e}"))
}

fn write(path: &Path, bytes: impl AsRef<[u8]>) -> R<()> {
    if let Some(dir) = path.parent() {
        io("mkdir", std::fs::create_dir_all(dir))?;
    }
    io(
        &format!("write {}", path.display()),
        std::fs::write(path, bytes),
    )
}

/// Data dir = `HARNESS_HOME`; workspace, git fixture, import fixtures, agent state and log live next to it.
struct Chat {
    env: Env,
    repo: PathBuf,
    import_root: PathBuf,
}

fn chat_env() -> R<Chat> {
    let mut env = Env::from_env()?;
    let parent = env
        .home
        .parent()
        .ok_or("HARNESS_HOME has no parent")?
        .to_path_buf();
    for key in ["CLAUDE_CONFIG_DIR", "CODEX_HOME"] {
        let dir = std::env::var_os(key)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .ok_or(format!("{key} must be set (isolated)"))?;
        check(
            dir.starts_with(&parent),
            format!("{key} is not inside the isolated home"),
        )?;
    }
    env.workspace = parent.join("ws92");
    env.state = parent.join("fake-state92");
    env.log = parent.join("fake-agent92.jsonl");
    for dir in [&env.workspace, &env.state] {
        io("mkdir", std::fs::create_dir_all(dir))?;
    }
    let repo = parent.join("repo92");
    let import_root = parent.join("import92");
    Ok(Chat {
        env,
        repo,
        import_root,
    })
}

fn config(chat: &Chat) -> R<HubConfig> {
    let mut config = chat.env.config();
    // fake-a 声明图片与嵌入上下文能力，fake-b 不声明：同一上下文按能力生成不同块。
    if let Some(a) = config.agents.iter_mut().find(|a| a.id.as_str() == A) {
        a.args.extend(["--caps".into(), "image,embedded".into()]);
    }
    config.turn_snapshots = true;
    config.file_search = Some(SharedFileSearch(Arc::new(
        harness_workspace::WorkspaceFileSearch::new(),
    )));
    config.config_paths = Some(
        ConfigPaths::from_env(&chat.env.home, None).map_err(|e| format!("config paths: {e}"))?,
    );
    config
        .agent_kinds
        .insert(AgentId::new(A), AgentKind::ClaudeCode);
    config.import_roots = harness_import::ImportRoots {
        zcode: Some(chat.import_root.join("zcode")),
        claude: Some(chat.import_root.join("claude")),
        codex: None,
    };
    config.schedule_min_interval_secs = 1;
    Ok(config)
}

async fn run() -> R<()> {
    let chat = chat_env()?;
    seed_workspace(&chat)?;
    let gate = super::phase9::start(config(&chat)?).await?;
    organize(&chat.env, &gate).await?;
    fork(&chat.env, &gate).await?;
    let rewound = rewind_edit_retry(&chat.env, &gate).await?;
    file_undo(&chat, &gate).await?;
    find(&chat.env, &gate).await?;
    context(&chat.env, &gate, &rewound).await?;
    commands(&chat.env, &gate).await?;
    background(&chat.env, &gate).await?;
    plan_usage_tools(&chat.env, &gate).await?;
    permissions(&chat.env, &gate).await?;
    command_search(&chat.env, &gate).await?;
    import(&chat, &gate).await?;
    more::run(&chat, &gate).await?;
    schedules(&chat, gate).await
}

// ---- fixtures ---------------------------------------------------------------------------------------------

fn seed_workspace(chat: &Chat) -> R<()> {
    let ws = &chat.env.workspace;
    write(&ws.join("ctx").join("small.txt"), "small text\n")?;
    // PNG 签名即可：门禁只关心 MIME 与大小，不解码图片。
    write(&ws.join("ctx").join("pic.png"), b"\x89PNG\r\n\x1a\nfixture")?;
    write(&ws.join("big.png"), vec![0_u8; 6 * 1024 * 1024])?;
    write(&ws.join("settings-notes.md"), "notes\n")?;
    write(
        &ws.join(".claude").join("commands").join("review.md"),
        "---\ndescription: Workspace review\n---\nReview $ARGUMENTS\n",
    )?;
    write(
        &ws.join(".claude").join("commands").join("ws-only.md"),
        "---\ndescription: Workspace only\n---\nBody\n",
    )?;
    let claude_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .ok_or("CLAUDE_CONFIG_DIR must be set")?;
    write(
        &claude_dir.join("commands").join("user-only.md"),
        "---\ndescription: User only\n---\nBody\n",
    )?;
    write(
        &chat
            .env
            .home
            .join("skills")
            .join("gate-skill")
            .join("SKILL.md"),
        "---\nname: gate-skill\ndescription: Gate skill\n---\nSKILL-BODY-7731\n",
    )?;
    // git 仓库（文件撤销依赖回合快照的 git 树）。
    if chat.repo.exists() {
        io("rm repo", std::fs::remove_dir_all(&chat.repo))?;
    }
    io("mkdir repo", std::fs::create_dir_all(&chat.repo))?;
    super::workspace::git(&chat.repo, &["init", "-q", "-b", "main"])?;
    for (key, value) in [
        ("user.name", "Harness Gate"),
        ("user.email", "gate@example.invalid"),
        ("commit.gpgsign", "false"),
        ("core.autocrlf", "false"),
        ("core.hooksPath", "no-hooks"),
    ] {
        super::workspace::git(&chat.repo, &["config", key, value])?;
    }
    write(&chat.repo.join("README.md"), "hello\n")?;
    super::workspace::git(&chat.repo, &["add", "-A"])?;
    super::workspace::git(&chat.repo, &["commit", "-q", "-m", "initial"])?;
    Ok(())
}

async fn create(gate: &Gate, workspace: &Path, agent: &str, title: &str) -> R<SessionId> {
    let summary = api(
        "create_session",
        gate.hub
            .create_session(CreateSession {
                workspace_path: workspace.to_string_lossy().into_owned(),
                workspace_identity: None,
                title: Some(title.into()),
                agent: Some(AgentId::new(agent)),
                worktree: None,
            })
            .await,
    )?;
    Ok(summary.id)
}

async fn send_ctx(
    gate: &Gate,
    session: &SessionId,
    text: &str,
    context: Vec<ContextItem>,
) -> Result<TurnId, ApiError> {
    gate.hub
        .send_message(SendMessage {
            session_id: session.clone(),
            text: text.into(),
            agent: None,
            client_message_id: None,
            context,
        })
        .await
        .map(|a| a.turn_id)
}

async fn run_turn(gate: &Gate, session: &SessionId, text: &str) -> R<TurnId> {
    let turn = gate.send(session, text, None).await?;
    gate.completed(session, &turn).await?;
    Ok(turn)
}

async fn summary_of(gate: &Gate, session: &SessionId) -> R<SessionSummary> {
    let list = api(
        "list_sessions",
        gate.hub.list_sessions(ListSessions::default()).await,
    )?;
    list.into_iter()
        .find(|s| &s.id == session)
        .ok_or_else(|| format!("session {session} not listed"))
}

async fn turns(gate: &Gate, session: &SessionId) -> R<Vec<harness_core_api::TurnView>> {
    Ok(api("open_session", gate.hub.open_session(session.clone()).await)?.turns)
}

fn saw(gate: &Gate, find: impl Fn(&EventKind) -> bool) -> bool {
    gate.event_index(find).is_some()
}

// ---- 1. organize ------------------------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
async fn organize(env: &Env, gate: &Gate) -> R<()> {
    let ws = env.workspace.clone();
    let s1 = create(gate, &ws, A, "first").await?;
    let s2 = create(gate, &ws, A, "second").await?;
    api(
        "rename",
        gate.hub
            .rename_session(s1.clone(), "  Renamed  ".into())
            .await,
    )?;
    check(
        summary_of(gate, &s1).await?.title == "Renamed",
        "rename not trimmed/saved",
    )?;
    let invalid = gate.hub.rename_session(s1.clone(), "   ".into()).await;
    check(
        matches!(invalid, Err(ApiError::InvalidArgument(_))),
        format!("empty rename: {invalid:?}"),
    )?;
    api("pin", gate.hub.set_pinned(vec![s1.clone()], true).await)?;
    check(summary_of(gate, &s1).await?.pinned, "pin not saved")?;
    // 新会话已读；回合结束后未读；显式标记已读/未读。
    check(!summary_of(gate, &s2).await?.unread, "new session unread")?;
    run_turn(gate, &s2, "unread rule").await?;
    check(
        summary_of(gate, &s2).await?.unread,
        "finished turn did not mark unread",
    )?;
    api("read", gate.hub.set_unread(s2.clone(), false).await)?;
    check(
        !summary_of(gate, &s2).await?.unread,
        "set_unread(false) ignored",
    )?;
    api("unread", gate.hub.set_unread(s2.clone(), true).await)?;
    check(
        summary_of(gate, &s2).await?.unread,
        "set_unread(true) ignored",
    )?;

    let colors = [
        GroupColor::Gray,
        GroupColor::Red,
        GroupColor::Orange,
        GroupColor::Yellow,
        GroupColor::Green,
        GroupColor::Blue,
        GroupColor::Purple,
    ];
    let mut ids = Vec::new();
    for (i, color) in colors.into_iter().enumerate() {
        let group = api(
            "create_group",
            gate.hub.create_group(format!("Group {i}"), color).await,
        )?;
        ids.push(group.id);
    }
    let bad = gate.hub.create_group(String::new(), GroupColor::Red).await;
    check(
        matches!(bad, Err(ApiError::InvalidArgument(_))),
        format!("empty group name: {bad:?}"),
    )?;
    let listed = api("list_groups", gate.hub.list_groups().await)?;
    check(
        listed.iter().map(|g| &g.id).eq(ids.iter()) && listed[6].color == GroupColor::Purple,
        "groups not listed in creation order with colours",
    )?;
    let updated = api(
        "update_group",
        gate.hub
            .update_group(UpdateGroup {
                id: ids[0].clone(),
                name: Some("Work".into()),
                color: Some(GroupColor::Blue),
            })
            .await,
    )?;
    check(
        updated.name == "Work" && updated.color == GroupColor::Blue,
        "group update not applied",
    )?;
    let reversed: Vec<String> = ids.iter().rev().cloned().collect();
    api("reorder", gate.hub.reorder_groups(reversed.clone()).await)?;
    let listed = api("list_groups", gate.hub.list_groups().await)?;
    check(
        listed.iter().map(|g| &g.id).eq(reversed.iter()),
        "reorder not applied",
    )?;
    let partial = gate.hub.reorder_groups(reversed[1..].to_vec()).await;
    check(
        matches!(partial, Err(ApiError::InvalidArgument(_))),
        format!("partial reorder: {partial:?}"),
    )?;
    let work = ids[0].clone();
    api(
        "move s1",
        gate.hub
            .move_sessions(vec![s1.clone()], Some(work.clone()))
            .await,
    )?;
    api(
        "move s2",
        gate.hub
            .move_sessions(vec![s2.clone()], Some(work.clone()))
            .await,
    )?;
    let (a, b) = (summary_of(gate, &s1).await?, summary_of(gate, &s2).await?);
    check(
        a.group_id.as_deref() == Some(work.as_str())
            && b.group_id.as_deref() == Some(work.as_str()),
        "move to group not applied",
    )?;
    check(b.order > a.order, "last moved session is not on top")?;
    api(
        "move s2 out",
        gate.hub.move_sessions(vec![s2.clone()], None).await,
    )?;
    check(
        summary_of(gate, &s2).await?.group_id.is_none(),
        "move to ungrouped",
    )?;
    api("delete group", gate.hub.delete_group(work.clone()).await)?;
    check(
        summary_of(gate, &s1).await?.group_id.is_none(),
        "deleting a group did not ungroup its sessions",
    )?;
    check(
        api("list_groups", gate.hub.list_groups().await)?.len() == 6,
        "group not deleted",
    )?;
    check(
        saw(gate, |k| matches!(k, EventKind::GroupsChanged))
            && saw(gate, |k| matches!(k, EventKind::SessionUpdated { .. })),
        "GroupsChanged / SessionUpdated not seen",
    )?;
    step("organize");
    Ok(())
}

// ---- 2. fork ----------------------------------------------------------------------------------------------

async fn fork(env: &Env, gate: &Gate) -> R<()> {
    let source = create(gate, &env.workspace, A, "fork source").await?;
    let first = run_turn(gate, &source, "fork one [[say alpha]]").await?;
    run_turn(gate, &source, "fork two [[say beta]]").await?;
    let forked = api(
        "fork_session",
        gate.hub
            .fork_session(ForkSession {
                session_id: source.clone(),
                up_to_turn: Some(first),
                title: None,
            })
            .await,
    )?;
    let origin = forked
        .forked_from
        .clone()
        .ok_or("fork has no forked_from")?;
    check(
        origin.session_id == source && origin.turn_seq == 1 && forked.turn_count == 1,
        format!("fork origin {origin:?}, turns {}", forked.turn_count),
    )?;
    let copied = turns(gate, &forked.id).await?;
    check(
        copied.len() == 1 && copied[0].user_text.starts_with("fork one"),
        "fork did not copy exactly turn 1",
    )?;
    run_turn(gate, &forked.id, "after fork").await?;
    let prompts = env.prompts(A, "after fork");
    let blocks = prompts.last().ok_or("fork prompt not logged")?;
    let seed = blocks
        .iter()
        .find(|b| b.contains("kind=\"seed\""))
        .ok_or("fork prompt has no seed block")?;
    check(
        seed.contains("fork one") && !seed.contains("fork two"),
        "seed of the fork does not hold exactly the copied turn",
    )?;
    step("fork");
    Ok(())
}

// ---- 3. rewind / edit / retry -----------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
async fn rewind_edit_retry(env: &Env, gate: &Gate) -> R<SessionId> {
    let s = create(gate, &env.workspace, A, "rewind").await?;
    let t1 = run_turn(gate, &s, "r one").await?;
    let t2 = run_turn(gate, &s, "r two").await?;
    run_turn(gate, &s, "r three").await?;
    let busy = gate.send(&s, "[[slow 40]] r busy", None).await?;
    let attempt = gate
        .hub
        .rewind_session(Rewind {
            session_id: s.clone(),
            turn_id: t2.clone(),
            restore_files: false,
        })
        .await;
    check(
        matches!(attempt, Err(ApiError::Busy(_))),
        format!("rewind while running: {attempt:?}"),
    )?;
    gate.completed(&s, &busy).await?;
    let news_before = env
        .log_entries()
        .iter()
        .filter(|e| e["event"] == "new" && e["agent"] == A)
        .count();
    let result = api(
        "rewind",
        gate.hub
            .rewind_session(Rewind {
                session_id: s.clone(),
                turn_id: t2.clone(),
                restore_files: false,
            })
            .await,
    )?;
    check(
        result.removed_turns == 3 && result.user_text == "r two",
        format!("rewind result {result:?}"),
    )?;
    check(turns(gate, &s).await?.len() == 1, "rewind left later turns")?;
    check(
        saw(gate, |k| {
            matches!(k, EventKind::TurnsRemoved { from_seq: 2 })
        }),
        "TurnsRemoved not seen",
    )?;
    let after = run_turn(gate, &s, "r after").await?;
    let news_after = env
        .log_entries()
        .iter()
        .filter(|e| e["event"] == "new" && e["agent"] == A)
        .count();
    check(
        news_after > news_before,
        "rewind did not drop the agent session handle",
    )?;
    let prompts = env.prompts(A, "r after");
    let blocks = prompts.last().ok_or("post-rewind prompt not logged")?;
    let seed = blocks
        .iter()
        .find(|b| b.contains("kind=\"seed\""))
        .ok_or("post-rewind prompt has no seed")?;
    check(
        seed.contains("r one") && !seed.contains("r two") && !seed.contains("r three"),
        "post-rewind seed holds removed turns",
    )?;
    let edited = api(
        "edit_and_resend",
        gate.hub
            .edit_and_resend(EditResend {
                session_id: s.clone(),
                turn_id: after,
                text: "r edited".into(),
                context: None,
                agent: None,
                restore_files: false,
            })
            .await,
    )?;
    gate.completed(&s, &edited.turn_id).await?;
    let now = turns(gate, &s).await?;
    check(
        now.len() == 2 && now[1].user_text == "r edited" && now[1].id == edited.turn_id,
        "edit-and-resend did not replace turn 2",
    )?;
    let old = gate
        .hub
        .retry_turn(Retry {
            session_id: s.clone(),
            turn_id: t1,
            agent: None,
            restore_files: false,
        })
        .await;
    check(
        matches!(old, Err(ApiError::InvalidArgument(_))),
        format!("retry of a non-last turn: {old:?}"),
    )?;
    let retried = api(
        "retry",
        gate.hub
            .retry_turn(Retry {
                session_id: s.clone(),
                turn_id: edited.turn_id,
                agent: Some(AgentId::new(B)),
                restore_files: false,
            })
            .await,
    )?;
    let view = gate.completed(&s, &retried.turn_id).await?;
    let now = turns(gate, &s).await?;
    check(
        now.len() == 2
            && view.user_text == "r edited"
            && view.agent.as_ref().map(AgentId::as_str) == Some(B),
        "retry did not re-run the last turn with the chosen agent",
    )?;
    step("rewind_edit_retry");
    Ok(s)
}

// ---- 4. file undo -----------------------------------------------------------------------------------------

fn norm(path: &str) -> String {
    path.replace('\\', "/")
}

#[allow(clippy::too_many_lines)]
async fn file_undo(chat: &Chat, gate: &Gate) -> R<()> {
    let repo = &chat.repo;
    let s = create(gate, repo, A, "files").await?;
    let turn = run_turn(gate, &s, "[[write notes/a.txt first]]").await?;
    let file = repo.join("notes").join("a.txt");
    check(file.exists(), "agent write missing")?;
    let plan = api(
        "plan undo",
        gate.hub
            .plan_file_changes(s.clone(), turn.clone(), ChangeDirection::Undo)
            .await,
    )?;
    let entry = plan
        .files
        .iter()
        .find(|f| norm(&f.path) == "notes/a.txt")
        .ok_or(format!("undo plan misses notes/a.txt: {plan:?}"))?;
    check(
        plan.available && entry.safe && entry.action == FileAction::Delete,
        format!("undo plan entry {entry:?}"),
    )?;
    let apply = |direction, include_unsafe| ApplyFileChanges {
        session_id: s.clone(),
        turn_id: turn.clone(),
        direction,
        paths: None,
        include_unsafe,
    };
    let report = api(
        "apply undo",
        gate.hub
            .apply_file_changes(apply(ChangeDirection::Undo, false))
            .await,
    )?;
    check(
        report.applied.iter().any(|p| norm(p) == "notes/a.txt") && !file.exists(),
        format!("undo not applied: {report:?}"),
    )?;
    api(
        "apply reapply",
        gate.hub
            .apply_file_changes(apply(ChangeDirection::Reapply, false))
            .await,
    )?;
    check(
        std::fs::read_to_string(&file).ok().as_deref() == Some("first\n"),
        "reapply did not restore the turn's content",
    )?;
    write(&file, "changed later\n")?;
    let plan = api(
        "plan undo (modified)",
        gate.hub
            .plan_file_changes(s.clone(), turn.clone(), ChangeDirection::Undo)
            .await,
    )?;
    check(
        plan.files
            .iter()
            .any(|f| norm(&f.path) == "notes/a.txt" && !f.safe && f.reason.is_some()),
        "a file modified after the turn is not unsafe",
    )?;
    let skipped = api(
        "apply undo (safe only)",
        gate.hub
            .apply_file_changes(apply(ChangeDirection::Undo, false))
            .await,
    )?;
    check(
        skipped
            .skipped
            .iter()
            .any(|(p, _)| norm(p) == "notes/a.txt")
            && std::fs::read_to_string(&file).ok().as_deref() == Some("changed later\n"),
        "unsafe file was not skipped",
    )?;
    let forced = api(
        "apply undo (include unsafe)",
        gate.hub
            .apply_file_changes(apply(ChangeDirection::Undo, true))
            .await,
    )?;
    check(
        forced.applied.iter().any(|p| norm(p) == "notes/a.txt") && !file.exists(),
        "include_unsafe did not apply",
    )?;
    let second = run_turn(gate, &s, "[[write notes/b.txt bee]]").await?;
    let b = repo.join("notes").join("b.txt");
    check(b.exists(), "second write missing")?;
    let rewound = api(
        "rewind with files",
        gate.hub
            .rewind_session(Rewind {
                session_id: s.clone(),
                turn_id: second.clone(),
                restore_files: true,
            })
            .await,
    )?;
    let files = rewound.files.ok_or("rewind returned no file report")?;
    check(
        files.applied.iter().any(|p| norm(p) == "notes/b.txt") && !b.exists(),
        format!("rewind did not restore files: {files:?}"),
    )?;
    check(
        saw(gate, |k| matches!(k, EventKind::FilesRestored { .. })),
        "FilesRestored not seen",
    )?;
    step("file_undo");
    Ok(())
}

// ---- 5. find ----------------------------------------------------------------------------------------------

async fn find(env: &Env, gate: &Gate) -> R<()> {
    let s = create(gate, &env.workspace, A, "find").await?;
    run_turn(gate, &s, "[[tools]] Needle question").await?;
    let query = |text: &str, case_sensitive: bool| FindInSession {
        session_id: s.clone(),
        query: text.into(),
        case_sensitive,
        limit: 50,
    };
    let hits = api(
        "find needle",
        gate.hub.find_in_session(query("needle", false)).await,
    )?;
    let user = hits
        .iter()
        .find(|h| h.field == FindField::User)
        .ok_or("no user hit")?;
    check(
        user.start == 10 && user.len == 6 && user.seq == 1,
        format!("user hit offsets {user:?}"),
    )?;
    check(
        hits.iter().any(|h| h.field == FindField::Assistant),
        "no assistant hit",
    )?;
    let tool = api(
        "find tool",
        gate.hub.find_in_session(query("src/lib", false)).await,
    )?;
    check(
        tool.iter()
            .any(|h| h.field == FindField::Tool && h.tool_call_id.is_some() && h.start == 5),
        format!("no tool hit: {tool:?}"),
    )?;
    let exact = api(
        "find case",
        gate.hub.find_in_session(query("needle", true)).await,
    )?;
    check(
        !exact.iter().any(|h| h.field == FindField::User),
        "case-sensitive find matched a different case",
    )?;
    step("find");
    Ok(())
}

// ---- 6. context -------------------------------------------------------------------------------------------

fn ctx_items(other: &SessionId) -> Vec<ContextItem> {
    vec![
        ContextItem::File {
            path: "ctx/small.txt".into(),
        },
        ContextItem::Folder { path: "ctx".into() },
        ContextItem::Image {
            path: Some("ctx/pic.png".into()),
            data_base64: None,
            mime: "image/png".into(),
        },
        ContextItem::Text {
            title: Some("note".into()),
            text: "pasted".into(),
        },
        ContextItem::Session {
            session_id: other.clone(),
        },
    ]
}

/// The last six block types of the prompt whose user text is `user`.
fn tail_types(env: &Env, agent: &str, user: &str) -> R<Vec<String>> {
    let entry = env
        .log_entries()
        .into_iter()
        .rev()
        .filter(|e| e["event"] == "prompt" && e["agent"] == agent)
        .find(|e| {
            e["blocks"]
                .as_array()
                .and_then(|b| b.last())
                .and_then(|b| b.as_str())
                == Some(user)
        })
        .ok_or(format!("prompt {user} not logged"))?;
    let types: Vec<String> = entry["blockTypes"]
        .as_array()
        .ok_or("no blockTypes")?
        .iter()
        .filter_map(|t| t.as_str().map(str::to_owned))
        .collect();
    Ok(types[types.len().saturating_sub(6)..].to_vec())
}

#[allow(clippy::too_many_lines)]
async fn context(env: &Env, gate: &Gate, other: &SessionId) -> R<()> {
    let ws = &env.workspace;
    let sa = create(gate, ws, A, "ctx a").await?;
    let sb = create(gate, ws, B, "ctx b").await?;
    let ta = api(
        "send ctx a",
        send_ctx(gate, &sa, "ctx prompt a", ctx_items(other)).await,
    )?;
    gate.completed(&sa, &ta).await?;
    let tb = api(
        "send ctx b",
        send_ctx(gate, &sb, "ctx prompt b", ctx_items(other)).await,
    )?;
    gate.completed(&sb, &tb).await?;
    let a_types = tail_types(env, A, "ctx prompt a")?;
    check(
        a_types == ["resource", "resource_link", "image", "text", "text", "text"],
        format!("fake-a block types {a_types:?}"),
    )?;
    let b_types = tail_types(env, B, "ctx prompt b")?;
    check(
        b_types
            == [
                "resource_link",
                "resource_link",
                "text",
                "text",
                "text",
                "text",
            ],
        format!("fake-b block types {b_types:?}"),
    )?;
    let stored = gate.turn(&sa, &ta).await?;
    check(stored.context.len() == 5, "turn context not stored")?;

    let eleven: Vec<ContextItem> = (0..11)
        .map(|i| ContextItem::Text {
            title: None,
            text: format!("t{i}"),
        })
        .collect();
    for (what, items) in [
        ("11 items", eleven),
        (
            "6 MB image",
            vec![ContextItem::Image {
                path: Some("big.png".into()),
                data_base64: None,
                mime: "image/png".into(),
            }],
        ),
        (
            "missing file",
            vec![ContextItem::File {
                path: "ctx/missing.txt".into(),
            }],
        ),
    ] {
        let result = send_ctx(gate, &sa, "rejected", items).await;
        check(
            matches!(result, Err(ApiError::InvalidArgument(_))),
            format!("{what} accepted: {result:?}"),
        )?;
    }
    let kinds = |rows: &[harness_core_api::ResolvedItem]| -> Vec<BlockKind> {
        rows.iter().map(|r| r.block_kind).collect()
    };
    let ra = api(
        "resolve a",
        gate.hub.resolve_context(sa.clone(), ctx_items(other)).await,
    )?;
    check(
        ra.iter().all(|r| r.ok)
            && kinds(&ra)
                == [
                    BlockKind::Resource,
                    BlockKind::ResourceLink,
                    BlockKind::Image,
                    BlockKind::Text,
                    BlockKind::Text,
                ],
        format!("resolve a {ra:?}"),
    )?;
    let rb = api(
        "resolve b",
        gate.hub.resolve_context(sb.clone(), ctx_items(other)).await,
    )?;
    check(
        kinds(&rb)
            == [
                BlockKind::ResourceLink,
                BlockKind::ResourceLink,
                BlockKind::Unsupported,
                BlockKind::Text,
                BlockKind::Text,
            ],
        format!("resolve b {rb:?}"),
    )?;
    let files = api(
        "mentions files",
        gate.hub
            .mention_candidates(MentionQuery {
                session_id: sa.clone(),
                query: "small".into(),
                kinds: vec![MentionKind::File],
                limit: 10,
            })
            .await,
    )?;
    check(
        files.iter().any(|m| {
            matches!(&m.item, ContextItem::File { path } if norm(path).ends_with("ctx/small.txt"))
        }),
        format!("file mention missing: {files:?}"),
    )?;
    let sessions = api(
        "mentions sessions",
        gate.hub
            .mention_candidates(MentionQuery {
                session_id: sa.clone(),
                query: "rewind".into(),
                kinds: vec![MentionKind::Session],
                limit: 10,
            })
            .await,
    )?;
    check(
        sessions
            .iter()
            .any(|m| matches!(&m.item, ContextItem::Session { session_id } if session_id == other)),
        format!("session mention missing: {sessions:?}"),
    )?;
    step("context");
    Ok(())
}

// ---- 7. commands ------------------------------------------------------------------------------------------

async fn background(env: &Env, gate: &Gate) -> R<()> {
    let s = create(gate, &env.workspace, A, "background").await?;
    run_turn(gate, &s, "[[bg]] start").await?;
    let finished = |e: &harness_core_api::HubEvent| {
        e.session_id.as_ref() == Some(&s)
            && matches!(&e.kind, EventKind::BackgroundTasksChanged { tasks }
                if tasks.iter().any(|t| t.name == "sleep" && t.state == "completed" && t.finished_at.is_some()))
    };
    gate.wait_event("BackgroundTasksChanged completed", finished)
        .await?;
    let deadline = Instant::now() + Duration::from_secs(20);
    let autonomous = loop {
        let list = turns(gate, &s).await?;
        if let Some(turn) = list.iter().find(|t| {
            t.origin
                .as_ref()
                .and_then(|o| o.background.as_ref())
                .is_some()
                && t.status == TurnStatus::Completed
        }) {
            break turn.clone();
        }
        if Instant::now() > deadline {
            return Err(format!("no autonomous turn: {list:?}"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    check(
        autonomous.assistant_text == "autonomous ok"
            && autonomous.user_text == "Background task finished: gate sleep (exit code 0)",
        format!("autonomous turn {autonomous:?}"),
    )?;
    let detail = api("open_session", gate.hub.open_session(s.clone()).await)?;
    let task = detail
        .background_tasks
        .iter()
        .find(|t| t.name == "sleep")
        .ok_or("background task missing from the snapshot")?;
    check(
        task.summary == "exit code 0" && task.task_type == "local_bash",
        format!("task {task:?}"),
    )?;
    let stopped = gate
        .hub
        .stop_background_task(s.clone(), task.id.clone())
        .await;
    check(
        matches!(stopped, Err(ApiError::NotFound(_))),
        format!("stop of a finished task: {stopped:?}"),
    )?;
    step("background");
    Ok(())
}

async fn commands(env: &Env, gate: &Gate) -> R<()> {
    let s = create(gate, &env.workspace, A, "commands").await?;
    let before = api("list_commands", gate.hub.list_commands(s.clone()).await)?;
    let source_of = |list: &harness_core_api::CommandList, name: &str| {
        list.commands
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.source)
    };
    check(
        source_of(&before, "review") == Some(CommandSource::Workspace)
            && source_of(&before, "ws-only") == Some(CommandSource::Workspace)
            && source_of(&before, "user-only") == Some(CommandSource::User),
        format!(
            "config commands before the agent spoke: {:?}",
            before.commands
        ),
    )?;
    run_turn(gate, &s, "[[commands]] list").await?;
    gate.wait_event(
        "CommandsChanged",
        |e| matches!(&e.kind, EventKind::CommandsChanged { agent } if agent.as_str() == A),
    )
    .await?;
    let after = api("list_commands", gate.hub.list_commands(s.clone()).await)?;
    let review = after
        .commands
        .iter()
        .find(|c| c.name == "review")
        .ok_or("review missing")?;
    check(
        after.known
            && review.source == CommandSource::Agent
            && review.description == "Agent review"
            && after.commands.iter().filter(|c| c.name == "review").count() == 1
            && source_of(&after, "compact") == Some(CommandSource::Agent)
            && source_of(&after, "ws-only") == Some(CommandSource::Workspace)
            && source_of(&after, "user-only") == Some(CommandSource::User),
        format!("merged commands {:?}", after.commands),
    )?;
    check(
        source_of(&after, "gate-skill") == Some(CommandSource::Harness)
            && after
                .commands
                .iter()
                .any(|c| c.name == "gate-skill" && c.description == "Gate skill"),
        format!("harness skill missing: {:?}", after.commands),
    )?;
    run_turn(gate, &s, "/gate-skill hello there").await?;
    let expanded = env
        .log_entries()
        .into_iter()
        .filter(|e| e["event"] == "prompt" && e["agent"] == A)
        .filter_map(|e| e["blocks"].as_array()?.last()?.as_str().map(str::to_owned))
        .any(|last| {
            last.starts_with("<harness-skill name=\"gate-skill\"")
                && last.contains("SKILL-BODY-7731")
                && last.ends_with("for this request: hello there")
        });
    check(expanded, "harness skill was not expanded in the prompt")?;
    step("commands");
    Ok(())
}

// ---- 8. plan / usage / tool content -----------------------------------------------------------------------

async fn plan_usage_tools(env: &Env, gate: &Gate) -> R<()> {
    let s = create(gate, &env.workspace, A, "plan").await?;
    let turn = run_turn(
        gate,
        &s,
        "[[think pondering deeply]] [[plan A|B|C]] [[usage 50 1000]] [[tools]] p1",
    )
    .await?;
    let mine = |e: &harness_core_api::HubEvent| e.session_id.as_ref() == Some(&s);
    gate.wait_event("ThoughtDelta", |e| {
        mine(e) && matches!(&e.kind, EventKind::ThoughtDelta { text, .. } if text.contains("pondering deeply"))
    })
    .await?;
    gate.wait_event("PlanUpdated", |e| {
        mine(e) && matches!(&e.kind, EventKind::PlanUpdated { entries, .. } if entries.len() == 3)
    })
    .await?;
    gate.wait_event("UsageUpdated", |e| {
        mine(e)
            && matches!(&e.kind, EventKind::UsageUpdated { usage, .. } if usage.used == 50 && usage.size == 1000)
    })
    .await?;
    gate.wait_event("tool content", |e| {
        mine(e)
            && matches!(&e.kind, EventKind::ToolCallUpdated { tool, .. } if tool.content.len() == 3)
    })
    .await?;
    let detail = api("open_session", gate.hub.open_session(s.clone()).await)?;
    let view = detail
        .turns
        .iter()
        .find(|t| t.id == turn)
        .ok_or("turn missing")?;
    let usage = view.usage.as_ref().ok_or("prompt usage missing")?;
    let tool = view.tool_calls.first().ok_or("tool missing")?;
    check(
        view.plan.len() == 3
            && view.plan[0].status == "completed"
            && usage.total == 120
            && usage.cached_read == Some(7)
            && detail.context_usage.as_ref().is_some_and(|u| u.used == 50)
            && detail.plan.len() == 3
            && tool.content.len() == 3
            && tool.locations.len() == 1
            && tool.raw_output.is_some(),
        "plan / usage / tool content not in the snapshot",
    )?;
    step("plan_usage_tools");
    Ok(())
}

// ---- 9. permissions ---------------------------------------------------------------------------------------

async fn permissions(env: &Env, gate: &Gate) -> R<()> {
    let s = create(gate, &env.workspace, A, "permissions").await?;
    let turn = gate.send(&s, "[[permission all]] q", None).await?;
    let event = gate
        .wait_event("PermissionRequested", |e| {
            e.session_id.as_ref() == Some(&s)
                && matches!(&e.kind, EventKind::PermissionRequested { .. })
        })
        .await?;
    let EventKind::PermissionRequested { request } = event.kind else {
        return Err("unexpected event".into());
    };
    let kinds: Vec<PermissionOptionKind> = request.options.iter().map(|o| o.kind).collect();
    check(
        kinds
            == [
                PermissionOptionKind::AllowOnce,
                PermissionOptionKind::AllowAlways,
                PermissionOptionKind::RejectOnce,
                PermissionOptionKind::RejectAlways,
            ],
        format!("options {kinds:?}"),
    )?;
    api(
        "respond",
        gate.hub
            .respond_permission(s.clone(), request.request_id, "deny-always".into())
            .await,
    )?;
    let view = gate.completed(&s, &turn).await?;
    check(
        view.permissions.iter().any(|(_, d)| {
            matches!(d, PermissionDecision::Selected { option_id, kind: PermissionOptionKind::RejectAlways } if option_id == "deny-always")
        }) && view.assistant_text.contains("denied:deny-always"),
        format!("decision not recorded: {:?}", view.permissions),
    )?;
    step("permissions");
    Ok(())
}

// ---- 10. command search -----------------------------------------------------------------------------------

async fn command_search(env: &Env, gate: &Gate) -> R<()> {
    let s = create(gate, &env.workspace, A, "Settings research").await?;
    let actions = vec![
        ActionEntry {
            id: "new-session".into(),
            title: "New session".into(),
            keywords: vec!["create".into()],
        },
        ActionEntry {
            id: "open-settings".into(),
            title: "Open settings".into(),
            keywords: vec!["preferences".into()],
        },
        ActionEntry {
            id: "theme".into(),
            title: "Toggle theme".into(),
            // 关键词与标题同为词前缀匹配时，关键词扣 50 分：标题命中排前。
            keywords: vec!["appearance settings".into()],
        },
    ];
    let result = api(
        "command_search",
        gate.hub
            .command_search(CommandSearch {
                query: "settings".into(),
                workspace_path: Some(env.workspace()),
                scopes: vec![
                    SearchScope::Actions,
                    SearchScope::Sessions,
                    SearchScope::Files,
                ],
                actions,
                limit: 20,
            })
            .await,
    )?;
    let ids: Vec<&str> = result.actions.iter().map(|a| a.id.as_str()).collect();
    check(
        ids.first() == Some(&"open-settings")
            && ids.contains(&"theme")
            && !ids.contains(&"new-session"),
        format!("action ranking {ids:?}"),
    )?;
    check(
        result.sessions.iter().any(|h| h.summary.id == s),
        "session hit missing",
    )?;
    check(
        result.files_available
            && result
                .files
                .iter()
                .any(|f| norm(&f.path) == "settings-notes.md"),
        format!("file hits {:?}", result.files),
    )?;
    step("command_search");
    Ok(())
}

// ---- 11. import -------------------------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn import_fixtures(root: &Path, workspace: &str) -> R<()> {
    let zcode = root.join("zcode");
    let legacy = json!({
        "meta": {"taskId": "task-1", "title": "Legacy task", "workspacePath": workspace,
                 "createdAt": 1000, "updatedAt": 2000},
        "messages": [
            {"role": "user", "content": "hello legacy"},
            {"role": "assistant", "content": [{"type": "text", "text": "hi"}], "tools": [{"title": "Read file"}]},
            {"role": "user", "content": "second"},
            {"role": "assistant", "content": "done"}
        ]
    });
    write(
        &zcode.join("v2/sessions/ws1/task-1.json"),
        legacy.to_string(),
    )?;
    write(
        &zcode.join("v2/sessions/ws1/task-2.deleted.json"),
        legacy.to_string(),
    )?;
    let db = zcode.join("cli/db/db.sqlite");
    io(
        "mkdir db",
        std::fs::create_dir_all(db.parent().ok_or("db parent")?),
    )?;
    let c = rusqlite::Connection::open(&db).map_err(|e| format!("fixture db: {e}"))?;
    let dir = workspace.replace('\'', "''");
    c.execute_batch(&format!(
        "CREATE TABLE session(id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT,
            slug TEXT NOT NULL, directory TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL,
            time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, time_archived INTEGER);
         CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL, data TEXT NOT NULL, sequence INTEGER);
         CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL, sequence INTEGER);
         INSERT INTO session VALUES('cli-1','p',NULL,'s','{dir}','CLI session','1',10,30,NULL);
         INSERT INTO session VALUES('cli-archived','p',NULL,'s','{dir}','Old','1',10,20,15);
         INSERT INTO message VALUES('m1','cli-1',11,11,'{{\"role\":\"user\"}}',1);
         INSERT INTO message VALUES('m2','cli-1',12,12,'{{\"role\":\"assistant\"}}',2);
         INSERT INTO part VALUES('p1','m1','cli-1',11,11,'{{\"type\":\"text\",\"text\":\"cli question\"}}',1);
         INSERT INTO part VALUES('p2','m2','cli-1',12,12,'{{\"type\":\"text\",\"text\":\"cli answer\"}}',1);"
    ))
    .map_err(|e| format!("fixture db: {e}"))?;
    drop(c);
    let lines = [
        json!({"type": "user", "cwd": workspace, "timestamp": "2026-01-01T00:00:00Z",
               "message": {"role": "user", "content": "fix the bug<system-reminder>noise</system-reminder>"}}),
        json!({"type": "assistant", "timestamp": "2026-01-01T00:00:05Z", "message": {"content": [
            {"type": "text", "text": "Fixed"}, {"type": "tool_use", "name": "Read", "input": {}}]}}),
    ];
    let jsonl = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let claude = root.join("claude");
    write(&claude.join("projects/-ws/abc-123.jsonl"), jsonl)?;
    write(&claude.join("settings.json"), "{\"must\":\"not be read\"}")?;
    Ok(())
}

/// Path → (bytes, mtime) of every file under `root`.
fn fingerprint(root: &Path) -> R<BTreeMap<PathBuf, (Vec<u8>, std::time::SystemTime)>> {
    let mut out = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in io("read_dir", std::fs::read_dir(&dir))? {
            let path = io("entry", entry)?.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let meta = io("metadata", std::fs::metadata(&path))?;
                let modified = io("mtime", meta.modified())?;
                out.insert(path.clone(), (io("read", std::fs::read(&path))?, modified));
            }
        }
    }
    Ok(out)
}

async fn import(chat: &Chat, gate: &Gate) -> R<()> {
    let root = &chat.import_root;
    if root.exists() {
        io("rm import fixtures", std::fs::remove_dir_all(root))?;
    }
    import_fixtures(root, &chat.env.workspace())?;
    let before = fingerprint(root)?;
    let scan = api(
        "import_scan",
        gate.hub
            .import_scan(ImportScan {
                sources: Vec::new(),
            })
            .await,
    )?;
    let mut ids: Vec<&str> = scan.iter().map(|c| c.external_id.as_str()).collect();
    ids.sort_unstable();
    check(
        ids == ["abc-123", "cli-1", "task-1"] && scan.iter().all(|c| !c.imported),
        format!("scan {ids:?}"),
    )?;
    let zcode = api(
        "import zcode",
        gate.hub
            .import_sessions(ImportRequest {
                source: ImportSource::Zcode,
                external_ids: vec!["task-1".into(), "cli-1".into()],
            })
            .await,
    )?;
    let claude = api(
        "import claude",
        gate.hub
            .import_sessions(ImportRequest {
                source: ImportSource::ClaudeCode,
                external_ids: vec!["abc-123".into()],
            })
            .await,
    )?;
    check(
        zcode.imported.len() == 2 && claude.imported.len() == 1,
        format!("imported {zcode:?} {claude:?}"),
    )?;
    let again = api(
        "import again",
        gate.hub
            .import_sessions(ImportRequest {
                source: ImportSource::Zcode,
                external_ids: vec!["task-1".into()],
            })
            .await,
    )?;
    check(
        again.imported.is_empty() && again.skipped.len() == 1,
        "re-import was not deduplicated",
    )?;
    let rescan = api(
        "rescan",
        gate.hub
            .import_scan(ImportScan {
                sources: Vec::new(),
            })
            .await,
    )?;
    check(
        rescan.iter().all(|c| c.imported),
        "rescan does not mark imported",
    )?;
    let legacy = zcode
        .imported
        .iter()
        .find(|s| s.imported_from.as_deref() == Some("zcode:task-1"))
        .ok_or("legacy session missing")?;
    let legacy_turns = turns(gate, &legacy.id).await?;
    check(
        legacy_turns.len() == 2
            && legacy_turns
                .iter()
                .all(|t| t.status == TurnStatus::Completed)
            && legacy_turns[0].tool_calls.len() == 1,
        "imported legacy turns",
    )?;
    let cc = &claude.imported[0];
    let cc_turns = turns(gate, &cc.id).await?;
    check(
        cc_turns.len() == 1 && cc_turns[0].user_text == "fix the bug",
        format!(
            "imported claude turn {:?}",
            cc_turns.first().map(|t| &t.user_text)
        ),
    )?;
    check(
        fingerprint(root)? == before,
        "import changed fixture bytes, mtimes or files",
    )?;
    let news_before = chat
        .env
        .log_entries()
        .iter()
        .filter(|e| e["event"] == "new" && e["agent"] == A)
        .count();
    run_turn(gate, &cc.id, "continue imported").await?;
    let prompts = chat.env.prompts(A, "continue imported");
    let blocks = prompts
        .last()
        .ok_or("imported continue prompt not logged")?;
    check(
        blocks
            .iter()
            .any(|b| b.contains("kind=\"seed\"") && b.contains("fix the bug")),
        "continuing an imported session after a failed session/load did not seed its history",
    )?;
    let created: Vec<String> = chat
        .env
        .log_entries()
        .iter()
        .filter(|e| e["event"] == "new" && e["agent"] == A)
        .skip(news_before)
        .filter_map(|e| e["sessionId"].as_str().map(str::to_owned))
        .collect();
    let native = created
        .last()
        .ok_or("no native session created for the imported session")?;
    let lines = [
        json!({"type": "user", "cwd": chat.env.workspace(), "timestamp": "2026-01-02T00:00:00Z",
               "message": {"role": "user", "content": "harness ran this"}}),
        json!({"type": "assistant", "timestamp": "2026-01-02T00:00:05Z", "message": {"content": "ok"}}),
    ];
    let jsonl = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    write(
        &root.join(format!("claude/projects/-ws/{native}.jsonl")),
        jsonl,
    )?;
    let own = api(
        "scan with a harness native session",
        gate.hub
            .import_scan(ImportScan {
                sources: Vec::new(),
            })
            .await,
    )?;
    check(
        !own.iter().any(|c| &c.external_id == native)
            && own.iter().any(|c| c.external_id == "abc-123" && c.imported),
        "import_scan lists a session Harness itself ran, or lost an imported one",
    )?;
    step("import");
    Ok(())
}

// ---- 12. schedules ----------------------------------------------------------------------------------------

async fn turn_count(gate: &Gate, s: &SessionId) -> R<usize> {
    Ok(turns(gate, s).await?.len())
}

async fn wait_scheduled(gate: &Gate, s: &SessionId, schedule: &str, within: Duration) -> R<TurnId> {
    let deadline = Instant::now() + within;
    loop {
        let found = turns(gate, s)
            .await?
            .into_iter()
            .find(|t| t.origin.as_ref().is_some_and(|o| o.schedule_id == schedule));
        if let Some(turn) = found {
            return Ok(turn.id);
        }
        if Instant::now() > deadline {
            return Err(format!("schedule {schedule} did not run within {within:?}"));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[allow(clippy::too_many_lines)]
async fn schedules(chat: &Chat, gate: Gate) -> R<()> {
    let s = create(&gate, &chat.env.workspace, A, "scheduled").await?;
    check(
        !api("schedules_enabled", gate.hub.schedules_enabled().await)?,
        "scheduler enabled by default",
    )?;
    check(
        api("list_schedules", gate.hub.list_schedules().await)?.is_empty(),
        "schedules exist by default",
    )?;
    let created = api(
        "create_schedule",
        gate.hub
            .create_schedule(NewSchedule {
                title: "every second".into(),
                prompt: "scheduled ping".into(),
                target: ScheduleTarget::Session {
                    session_id: s.clone(),
                },
                kind: ScheduleKind::Interval { seconds: 1 },
                enabled: true,
            })
            .await,
    )?;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    check(
        turn_count(&gate, &s).await? == 0,
        "a schedule ran while the scheduler was off",
    )?;
    api("enable", gate.hub.set_schedules_enabled(true).await)?;
    let first = wait_scheduled(&gate, &s, &created.id, Duration::from_secs(3)).await?;
    api(
        "pause schedule",
        gate.hub
            .update_schedule(UpdateSchedule {
                id: created.id.clone(),
                title: Some("renamed".into()),
                enabled: Some(false),
                ..UpdateSchedule::default()
            })
            .await,
    )?;
    gate.finished(&s, &first).await?;
    let run = api(
        "run now",
        gate.hub.run_schedule_now(created.id.clone()).await,
    )?;
    let view = gate.completed(&run.session_id, &run.turn_id).await?;
    check(
        view.origin
            .as_ref()
            .is_some_and(|o| o.schedule_title == "renamed"),
        "run-now turn has no origin",
    )?;
    api("delete", gate.hub.delete_schedule(created.id.clone()).await)?;
    check(
        !api("list", gate.hub.list_schedules().await)?
            .iter()
            .any(|p| p.id == created.id),
        "schedule not deleted",
    )?;
    // 错过的运行不补跑：停 hub 超过一个周期后重启，重启时不立即运行。
    let restart = create(&gate, &chat.env.workspace, A, "restart").await?;
    let periodic = api(
        "create periodic",
        gate.hub
            .create_schedule(NewSchedule {
                title: "every 2 s".into(),
                prompt: "periodic".into(),
                target: ScheduleTarget::Session {
                    session_id: restart.clone(),
                },
                kind: ScheduleKind::Interval { seconds: 2 },
                enabled: true,
            })
            .await,
    )?;
    let turn = wait_scheduled(&gate, &restart, &periodic.id, Duration::from_secs(4)).await?;
    gate.finished(&restart, &turn).await?;
    api("shutdown", gate.hub.shutdown().await)?;
    drop(gate);
    tokio::time::sleep(Duration::from_secs(5)).await;
    let gate = super::phase9::start(config(chat)?).await?;
    let started = Instant::now();
    let count = turn_count(&gate, &restart).await?;
    tokio::time::sleep(Duration::from_millis(700)).await;
    check(
        turn_count(&gate, &restart).await? == count && started.elapsed() < Duration::from_secs(2),
        "missed runs were caught up after the restart",
    )?;
    let listed = api("list", gate.hub.list_schedules().await)?;
    let next = listed
        .iter()
        .find(|p| p.id == periodic.id)
        .and_then(|p| p.next_run)
        .ok_or("periodic schedule lost its next run")?;
    check(
        next >= super::now_secs() * 1000 - 1000,
        "next run was not rescheduled from the restart",
    )?;
    api("disable", gate.hub.set_schedules_enabled(false).await)?;
    api(
        "delete periodic",
        gate.hub.delete_schedule(periodic.id).await,
    )?;
    step("schedules");
    api("final shutdown", gate.hub.shutdown().await)
}
