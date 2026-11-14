//! Phase 9.2 follow-up gate steps (phase-9-parity.md §9.2.5): queue edit / reorder / run-now / steer, pickers and
//! skill / subagent mentions, typed failures, the file-change `applied` flag, git branch / switch / commit / push /
//! log, and the projects-tree data.

use std::path::Path;

use harness_core_api::workspace::{PushProgress, SwitchOutcome, WorkspaceServices as _};
use harness_core_api::{
    ApiError, ApplyFileChanges, ChangeDirection, EventKind, FailureKind, HarnessCore, ListProjects,
    ListSessions, MentionKind, MentionQuery, Retry, RetryHint, SendMessage, TurnId, TurnOutcome,
};

use super::{api, create, io, run_turn, step, write, Chat, Gate, A, B, C, R};
use crate::check;

/// Runs steps 13–18.
pub(super) async fn run(chat: &Chat, gate: &Gate) -> R<()> {
    queue_ops(chat, gate).await?;
    pickers(chat, gate).await?;
    failures(chat, gate).await?;
    applied_flag(chat, gate).await?;
    git_ops(chat).await?;
    projects_tree(gate).await
}

fn queue_of(gate: &Gate, session: &harness_core_api::SessionId) -> Option<Vec<TurnId>> {
    gate.events().into_iter().rev().find_map(|e| match e.kind {
        EventKind::QueueChanged { queue } if e.session_id.as_ref() == Some(session) => {
            Some(queue.into_iter().map(|q| q.turn_id).collect())
        }
        _ => None,
    })
}

// ---- 13. queue ops ----------------------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
async fn queue_ops(chat: &Chat, gate: &Gate) -> R<()> {
    let env = &chat.env;
    let s = create(gate, &env.workspace, A, "queue").await?;
    let running = gate.send(&s, "[[slow 60]] long", None).await?;
    gate.wait_event(
        "queue: first turn started",
        |e| matches!(&e.kind, EventKind::TurnStarted { turn_id, .. } if turn_id == &running),
    )
    .await?;
    let q1 = gate.send(&s, "q-one", None).await?;
    let q2 = gate.send(&s, "q-two", None).await?;
    let q3 = gate.send(&s, "q-three", None).await?;
    api(
        "edit_queued",
        gate.hub
            .edit_queued(s.clone(), q2.clone(), "q-two edited".into())
            .await,
    )?;
    let empty = gate
        .hub
        .edit_queued(s.clone(), q2.clone(), "  ".into())
        .await;
    check(
        matches!(empty, Err(ApiError::InvalidArgument(_))),
        format!("empty edit: {empty:?}"),
    )?;
    let started = gate
        .hub
        .edit_queued(s.clone(), running.clone(), "x".into())
        .await;
    check(
        matches!(started, Err(ApiError::Conflict(_))),
        format!("editing the running turn: {started:?}"),
    )?;
    api(
        "reorder_queued",
        gate.hub
            .reorder_queued(s.clone(), vec![q3.clone(), q1.clone(), q2.clone()])
            .await,
    )?;
    let partial = gate
        .hub
        .reorder_queued(s.clone(), vec![q1.clone(), q2.clone()])
        .await;
    check(
        matches!(partial, Err(ApiError::Conflict(_))),
        format!("reorder without every turn: {partial:?}"),
    )?;
    api(
        "run_queued_now",
        gate.hub.run_queued_now(s.clone(), q2.clone()).await,
    )?;
    let steered = api(
        "steer",
        gate.hub
            .steer(SendMessage {
                session_id: s.clone(),
                text: "steered".into(),
                agent: None,
                client_message_id: None,
                context: Vec::new(),
            })
            .await,
    )?;
    let expected = vec![steered.turn_id.clone(), q2.clone(), q3.clone(), q1.clone()];
    check(
        queue_of(gate, &s) == Some(expected.clone()),
        format!(
            "queue after ops {:?}, want {expected:?}",
            queue_of(gate, &s)
        ),
    )?;
    // 正在运行的回合不受影响：它照常完成，之后按新顺序逐个运行（串行准入）。
    gate.completed(&s, &running).await?;
    gate.completed(&s, &q1).await?;
    let order: Vec<TurnId> = gate
        .events()
        .into_iter()
        .filter(|e| e.session_id.as_ref() == Some(&s))
        .filter_map(|e| match e.kind {
            EventKind::TurnStarted {
                turn_id,
                attempt: 1,
                ..
            } => Some(turn_id),
            _ => None,
        })
        .collect();
    let mut want = vec![running];
    want.extend(expected);
    check(
        order == want,
        format!("start order {order:?}, want {want:?}"),
    )?;
    check(
        !env.prompts(A, "q-two edited").is_empty() && env.prompts(A, "q-two").is_empty(),
        "the agent did not get the edited text",
    )?;
    step("queue_ops");
    Ok(())
}

// ---- 14. pickers and skill / subagent mentions ------------------------------------------------------------

async fn pickers(chat: &Chat, gate: &Gate) -> R<()> {
    let ws = &chat.env.workspace;
    write(
        &ws.join(".claude")
            .join("skills")
            .join("gate-skill")
            .join("SKILL.md"),
        "---\nname: gate-skill\ndescription: Gate skill\n---\nDo it.\n",
    )?;
    write(
        &ws.join(".claude").join("agents").join("gate-helper.md"),
        "---\nname: gate-helper\ndescription: Gate helper agent\n---\nHelp.\n",
    )?;
    let s = create(gate, ws, A, "pickers").await?;
    let found = api(
        "mentions",
        gate.hub
            .mention_candidates(MentionQuery {
                session_id: s.clone(),
                query: "gate".into(),
                kinds: vec![MentionKind::Skill, MentionKind::Subagent],
                limit: 20,
            })
            .await,
    )?;
    let skill = found
        .iter()
        .find(|c| c.kind == MentionKind::Skill && c.label == "gate-skill");
    let helper = found
        .iter()
        .find(|c| c.kind == MentionKind::Subagent && c.label == "gate-helper");
    check(
        skill.is_some_and(|c| c.insert_text.is_none() && c.detail == "Gate skill")
            && helper.is_some_and(|c| c.insert_text.as_deref() == Some("@agent-gate-helper")),
        format!("skill / subagent candidates {found:?}"),
    )?;
    let all = api(
        "mentions (all kinds)",
        gate.hub
            .mention_candidates(MentionQuery {
                session_id: s.clone(),
                query: String::new(),
                kinds: Vec::new(),
                limit: 50,
            })
            .await,
    )?;
    let first_config = all
        .iter()
        .position(|c| matches!(c.kind, MentionKind::Skill | MentionKind::Subagent));
    let last_other = all
        .iter()
        .rposition(|c| !matches!(c.kind, MentionKind::Skill | MentionKind::Subagent));
    check(
        first_config.is_some() && first_config > last_other,
        "skills / subagents do not rank after files, folders and sessions",
    )?;
    // 模式 / 模型 / 力度：选择先存为会话偏好，下一次提示前生效。
    run_turn(gate, &s, "warm up").await?;
    api(
        "set_mode",
        gate.hub.set_mode(s.clone(), "plan".into()).await,
    )?;
    api(
        "set_config_option",
        gate.hub
            .set_config_option(s.clone(), "effort".into(), "high".into())
            .await,
    )?;
    let modes = api("list_modes", gate.hub.list_modes(s.clone()).await)?;
    let config = api(
        "list_config_options",
        gate.hub.list_config_options(s.clone()).await,
    )?;
    let effort = config.options.iter().find(|o| o.id == "effort");
    check(
        modes.current.as_deref() == Some("plan")
            && modes.known
            && effort.and_then(|o| o.current.as_deref()) == Some("high"),
        format!("picker values {modes:?} {effort:?}"),
    )?;
    check(
        super::saw(gate, |k| {
            matches!(k, EventKind::SessionOptionsChanged { .. })
        }),
        "SessionOptionsChanged not seen",
    )?;
    // 下一次提示前生效：fake agent 的 [[options]] 报告会话当前值。
    let options = run_turn(gate, &s, "[[options]]").await?;
    let view = gate.completed(&s, &options).await?;
    check(
        view.assistant_text.contains("mode:plan,") && view.assistant_text.contains("effort:high"),
        format!("next prompt options {:?}", view.assistant_text),
    )?;
    step("pickers");
    Ok(())
}

// ---- 15. typed failures -----------------------------------------------------------------------------------

async fn failed_turn(
    gate: &Gate,
    session: &harness_core_api::SessionId,
    text: &str,
) -> R<(TurnId, harness_core_api::Failure)> {
    let turn = gate.send(session, text, None).await?;
    let view = gate.finished(session, &turn).await?;
    check(
        view.status == harness_core_api::TurnStatus::Failed,
        format!("{text}: turn ended {:?}", view.status),
    )?;
    let failure_at = gate
        .event_index(|k| matches!(k, EventKind::TurnFailure { turn_id, .. } if turn_id == &turn))
        .ok_or(format!("{text}: no TurnFailure"))?;
    let finished_at = gate
        .event_index(|k| {
            matches!(k, EventKind::TurnFinished { turn_id, outcome: TurnOutcome::Failed { .. }, .. }
                if turn_id == &turn)
        })
        .ok_or(format!("{text}: no failed TurnFinished"))?;
    check(
        failure_at < finished_at,
        format!("{text}: TurnFailure after TurnFinished"),
    )?;
    gate.events()
        .into_iter()
        .find_map(|e| match e.kind {
            EventKind::TurnFailure {
                turn_id, failure, ..
            } if turn_id == turn => Some((turn_id, failure)),
            _ => None,
        })
        .ok_or_else(|| format!("{text}: failure missing"))
}

async fn failures(chat: &Chat, gate: &Gate) -> R<()> {
    let env = &chat.env;
    let s = create(gate, &env.workspace, B, "failures").await?;
    let (_, auth) = failed_turn(gate, &s, "[[fail auth]]").await?;
    check(
        auth.kind == FailureKind::Auth && auth.retry == RetryHint::Login,
        format!("auth failure {auth:?}"),
    )?;
    let (_, other) = failed_turn(gate, &s, "[[fail other]]").await?;
    check(
        other.kind == FailureKind::Other
            && other.retry == RetryHint::Retry
            && other
                .details
                .as_deref()
                .is_some_and(|d| d.contains("code -32603") && d.contains("fake failure")),
        format!("other failure {other:?}"),
    )?;
    let (_, transient) = failed_turn(gate, &s, "[[fail transient 7]]").await?;
    check(
        transient.kind == FailureKind::Transient
            && transient.retry == RetryHint::After { secs: Some(7) },
        format!("transient failure {transient:?}"),
    )?;
    let (_, exited) = failed_turn(gate, &s, "[[fail exit]]").await?;
    check(
        exited.kind == FailureKind::ProcessExited
            && exited.retry == RetryHint::Retry
            && exited
                .trace
                .as_deref()
                .is_some_and(|t| t.contains("simulated crash")),
        format!("exit failure {exited:?}"),
    )?;
    // 进程退出后下一回合重新拉起 agent，照常完成。
    run_turn(gate, &s, "after crash").await?;
    let flag = env.state.join(format!("{C}.initfail"));
    write(&flag, "1")?;
    let c = create(gate, &env.workspace, C, "init failure").await?;
    let init = failed_turn(gate, &c, "hello").await;
    io("rm initfail", std::fs::remove_file(&flag))?;
    let (init_turn, init) = init?;
    check(
        init.kind == FailureKind::InitFailed && init.retry == RetryHint::Retry,
        format!("init failure {init:?}"),
    )?;
    // 重试同一回合（RetryInit { session } 的 hub 路径）：标志已删除，回合应照常完成。
    let retried = api(
        "retry_turn",
        gate.hub
            .retry_turn(Retry {
                session_id: c.clone(),
                turn_id: init_turn,
                agent: None,
                restore_files: false,
            })
            .await,
    )?;
    gate.completed(&c, &retried.turn_id).await?;
    // 快照的 event_seq 覆盖此前本会话的全部事件（客户端据此丢弃已包含在快照里的旧事件）。
    let detail = api("open_session", gate.hub.open_session(c.clone()).await)?;
    let last = gate
        .events()
        .into_iter()
        .filter(|e| e.session_id.as_ref() == Some(&c))
        .map(|e| e.seq)
        .max()
        .unwrap_or(0);
    check(
        detail.event_seq >= last && last > 0,
        format!(
            "snapshot event_seq {} < last session event {last}",
            detail.event_seq
        ),
    )?;
    step("failures");
    Ok(())
}

// ---- 16. applied flag -------------------------------------------------------------------------------------

async fn applied_flag(chat: &Chat, gate: &Gate) -> R<()> {
    let s = create(gate, &chat.repo, A, "applied").await?;
    let turn = run_turn(gate, &s, "[[write notes/c.txt cee]]").await?;
    let plan = |direction| {
        gate.hub
            .plan_file_changes(s.clone(), turn.clone(), direction)
    };
    let undo = api("plan undo", plan(ChangeDirection::Undo).await)?;
    let reapply = api("plan reapply", plan(ChangeDirection::Reapply).await)?;
    check(
        undo.available && !undo.applied && reapply.applied,
        format!(
            "fresh turn: undo.applied {} reapply.applied {}",
            undo.applied, reapply.applied
        ),
    )?;
    api(
        "apply undo",
        gate.hub
            .apply_file_changes(ApplyFileChanges {
                session_id: s.clone(),
                turn_id: turn.clone(),
                direction: ChangeDirection::Undo,
                paths: None,
                include_unsafe: false,
            })
            .await,
    )?;
    let undo = api("plan undo (after)", plan(ChangeDirection::Undo).await)?;
    let reapply = api("plan reapply (after)", plan(ChangeDirection::Reapply).await)?;
    check(
        undo.applied && !reapply.applied,
        format!(
            "after undo: undo.applied {} reapply.applied {}",
            undo.applied, reapply.applied
        ),
    )?;
    step("applied_flag");
    Ok(())
}

// ---- 17. git ops ------------------------------------------------------------------------------------------

fn git(cwd: &Path, args: &[&str]) -> R<String> {
    crate::workspace::git(cwd, args)
}

#[allow(clippy::too_many_lines)]
async fn git_ops(chat: &Chat) -> R<()> {
    let parent = chat.repo.parent().ok_or("repo has no parent")?;
    let root = parent.join("repo92-git");
    let remote = parent.join("repo92-remote.git");
    for dir in [&root, &remote] {
        if dir.exists() {
            io("rm", std::fs::remove_dir_all(dir))?;
        }
        io("mkdir", std::fs::create_dir_all(dir))?;
    }
    git(&remote, &["init", "-q", "--bare"])?;
    git(&root, &["init", "-q", "-b", "main"])?;
    for (key, value) in [
        ("user.name", "Harness Gate"),
        ("user.email", "gate@example.invalid"),
        ("commit.gpgsign", "false"),
        ("core.autocrlf", "false"),
        ("core.hooksPath", "no-hooks"),
    ] {
        git(&root, &["config", key, value])?;
    }
    write(&root.join("shared.txt"), "base\n")?;
    git(&root, &["add", "-A"])?;
    git(&root, &["commit", "-q", "-m", "base"])?;
    let remote_path = remote.to_string_lossy().replace('\\', "/");
    git(&root, &["remote", "add", "origin", &remote_path])?;

    let ws = api(
        "open workspace",
        harness_workspace::Workspace::open(
            root.clone(),
            harness_workspace::WorkspaceConfig::default(),
        )
        .await,
    )?;
    let list = api("branch list", ws.git_branch_list().await)?;
    check(
        list.current.as_deref() == Some("main") && !list.detached && list.upstream.is_none(),
        format!("initial branches {list:?}"),
    )?;
    let created = api("switch -c", ws.git_switch("other".into(), true).await)?;
    check(
        created
            == SwitchOutcome::Switched {
                branch: "other".into(),
            },
        format!("create branch {created:?}"),
    )?;
    write(&root.join("shared.txt"), "other\n")?;
    let committed = api(
        "commit_with (other)",
        ws.git_commit_with("change shared".into(), true).await,
    )?;
    check(!committed.hash.is_empty(), "no commit hash")?;
    api("switch main", ws.git_switch("main".into(), false).await)?;
    write(&root.join("shared.txt"), "local\n")?;
    let blocked = api("switch blocked", ws.git_switch("other".into(), false).await)?;
    check(
        blocked
            == SwitchOutcome::Blocked {
                files: vec!["shared.txt".into()],
            }
            && std::fs::read_to_string(root.join("shared.txt"))
                .ok()
                .as_deref()
                == Some("local\n")
            && api("branch list", ws.git_branch_list().await)?
                .current
                .as_deref()
                == Some("main"),
        format!("blocked switch {blocked:?}"),
    )?;
    git(&root, &["checkout", "--", "shared.txt"])?;
    let nothing = ws.git_commit_with("nothing".into(), false).await;
    check(
        matches!(nothing, Err(ApiError::InvalidArgument(_))),
        format!("commit with nothing staged: {nothing:?}"),
    )?;
    write(&root.join("new.txt"), "new\n")?;
    api(
        "commit_with include_unstaged",
        ws.git_commit_with("add new".into(), true).await,
    )?;
    let tracked = git(&root, &["ls-files", "new.txt"])?;
    check(tracked.trim() == "new.txt", "untracked file not committed")?;
    git(
        &root,
        &["merge", "-q", "--no-ff", "other", "-m", "merge other"],
    )?;

    let progress = std::sync::Mutex::new(Vec::new());
    let report = |p: PushProgress| {
        if let Ok(mut all) = progress.lock() {
            all.push(p);
        }
    };
    let pushed = api("push", ws.git_push(None, None, &report).await)?;
    let phases = progress.lock().map(|p| p.clone()).unwrap_or_default();
    check(
        pushed.remote == "origin"
            && pushed.branch == "main"
            && pushed.set_upstream
            && pushed.ahead == 0
            && pushed.behind == 0
            && matches!(phases.first(), Some(PushProgress::Started { .. }))
            && phases.last() == Some(&PushProgress::Finished),
        format!("push {pushed:?} {phases:?}"),
    )?;
    let list = api("branch list (pushed)", ws.git_branch_list().await)?;
    check(
        list.upstream.as_deref() == Some("origin/main") && list.ahead == 0,
        format!("upstream after push {list:?}"),
    )?;
    write(&root.join("ahead.txt"), "a\n")?;
    api(
        "commit ahead",
        ws.git_commit_with("ahead".into(), true).await,
    )?;
    let list = api("branch list (ahead)", ws.git_branch_list().await)?;
    check(list.ahead == 1, format!("ahead after commit {list:?}"))?;
    let again = api("push again", ws.git_push(None, None, &|_| {}).await)?;
    check(
        !again.set_upstream && again.ahead == 0,
        format!("second push {again:?}"),
    )?;

    let full = api("log", ws.git_log(0, 50).await)?;
    let merge = full
        .rows
        .iter()
        .find(|r| r.subject == "merge other")
        .ok_or("merge commit missing from log")?;
    check(
        merge.parents.len() == 2
            && merge.parent_lanes.len() == 2
            && merge.parent_lanes[0] != merge.parent_lanes[1]
            && full.rows.iter().any(|r| r.lanes >= 2)
            && full
                .rows
                .first()
                .is_some_and(|r| r.refs.iter().any(|x| x.contains("main")))
            && !full.has_more,
        format!("graph rows {:?}", full.rows),
    )?;
    let first = api("log page 1", ws.git_log(0, 2).await)?;
    let second = api("log page 2", ws.git_log(2, 2).await)?;
    check(
        first.has_more
            && first.rows.len() == 2
            && second.rows.first().map(|r| &r.hash) == full.rows.get(2).map(|r| &r.hash)
            && second.rows.first().map(|r| r.lane) == full.rows.get(2).map(|r| r.lane),
        "log pages do not line up",
    )?;
    let _ = ws.shutdown().await;
    step("git_ops");
    Ok(())
}

// ---- 18. projects tree ------------------------------------------------------------------------------------

async fn projects_tree(gate: &Gate) -> R<()> {
    let sessions = api(
        "list_sessions",
        gate.hub.list_sessions(ListSessions::default()).await,
    )?;
    let projects = api(
        "list_projects",
        gate.hub.list_projects(ListProjects::default()).await,
    )?;
    check(
        !sessions.is_empty()
            && sessions.iter().all(|s| {
                !s.workspace_path.is_empty()
                    && projects
                        .iter()
                        .any(|p| p.workspace_key == s.workspace_key && p.sessions > 0)
            }),
        "a session has no project row",
    )?;
    check(projects.len() >= 2, format!("projects {projects:?}"))?;
    step("projects_tree");
    Ok(())
}
