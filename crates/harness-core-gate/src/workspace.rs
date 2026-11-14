//! Round 8.2 scenarios (spec phase-8-core.md §15.5): workspace services on an isolated fixture repository,
//! turn diffs, and the API 0.2.0 additions.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use harness_agent_hub::{Hub, HubConfig};
use harness_core_api::diff::{DiffLineKind, FileDiffStatus};
use harness_core_api::workspace::{
    GitChangeKind, SearchText, TerminalCreate, WorkspaceEventKind, WorkspaceEventStream,
    WorkspaceServices,
};
use harness_core_api::{EventKind, HarnessCore, ListSessions, SessionId, TurnSegment, TurnStatus};
use harness_workspace::{Workspace, WorkspaceConfig};

use crate::{check, step, Env, Gate, A, R};

const EVENT_TIMEOUT: Duration = Duration::from_secs(20);

pub(super) fn git(cwd: &Path, args: &[&str]) -> R<String> {
    let mut command = std::process::Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000);
    }
    let out = command
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn write(path: &Path, text: &str) -> R<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))
}

/// A fresh git repository under the isolated home (local identity, no hooks, no autocrlf).
fn fixture(env: &Env) -> R<PathBuf> {
    let root = env.home.join("fixture");
    if root.exists() {
        std::fs::remove_dir_all(&root).map_err(|e| format!("rm fixture: {e}"))?;
    }
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
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
    write(&root.join("README.md"), "hello\nworld\n")?;
    write(
        &root.join("src/lib.rs"),
        "fn alpha() {}\n// NEEDLE one\nfn beta() {}\n",
    )?;
    write(&root.join("docs/guide.md"), "Guide\nneedle two\n")?;
    git(&root, &["add", "-A"])?;
    git(&root, &["commit", "-q", "-m", "initial"])?;
    Ok(root)
}

async fn next_event(
    events: &mut WorkspaceEventStream,
    what: &str,
    mut find: impl FnMut(&WorkspaceEventKind) -> bool,
) -> R<WorkspaceEventKind> {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, events.recv()).await {
            Ok(Some(event)) if find(&event.kind) => return Ok(event.kind),
            Ok(Some(_)) => {}
            Ok(None) => return Err(format!("event stream closed waiting for {what}")),
            Err(_) => return Err(format!("timeout waiting for {what}")),
        }
    }
}

pub(crate) async fn run(env: &Env, gate: &Gate) -> R<()> {
    let root = fixture(env)?;
    let config = WorkspaceConfig {
        watch_debounce: Duration::from_millis(50),
        ..WorkspaceConfig::default()
    };
    let ws = Workspace::open(root.clone(), config.clone())
        .await
        .map_err(|e| format!("open: {e}"))?;
    let result = services(env, &ws, &root, &config).await;
    let shutdown = ws.shutdown().await.map_err(|e| e.to_string());
    result?;
    shutdown?;
    turn_diff(env, gate, &root).await?;
    api_8_2(env, gate).await
}

async fn services(env: &Env, ws: &Workspace, root: &Path, config: &WorkspaceConfig) -> R<()> {
    // ---- lazy start ---------------------------------------------------------------------------------
    let stats = ws.stats();
    check(
        stats.terminals == 0
            && stats.watches == 0
            && stats.searches_running == 0
            && !stats.index_built
            && !stats.pty_started
            && !stats.watcher_started,
        format!("services started eagerly: {stats:?}"),
    )?;
    step("workspace_lazy");

    // ---- read + containment -------------------------------------------------------------------------
    let file = ws
        .read_file("README.md".into(), 0)
        .await
        .map_err(|e| format!("read: {e}"))?;
    check(
        file.text == "hello\nworld\n" && file.path == "README.md" && !file.binary,
        format!("read_file: {file:?}"),
    )?;
    let nested = ws
        .read_file("src\\lib.rs".into(), 5)
        .await
        .map_err(|e| format!("read nested: {e}"))?;
    check(
        nested.path == "src/lib.rs" && nested.truncated && nested.text.len() == 5,
        format!("read_file limit: {nested:?}"),
    )?;
    check(
        ws.read_file("../outside.txt".into(), 0).await.is_err(),
        "path escape (..) was not rejected",
    )?;
    let outside = env.home.join("outside.txt");
    write(&outside, "secret")?;
    check(
        ws.read_file(outside.to_string_lossy().into_owned(), 0)
            .await
            .is_err(),
        "absolute path outside the root was not rejected",
    )?;
    step("workspace_read");

    // ---- file search ----------------------------------------------------------------------------------
    let hits = ws
        .search_files("lib".into(), 20)
        .await
        .map_err(|e| format!("search_files: {e}"))?;
    check(
        hits.iter().any(|h| h.path == "src/lib.rs" && !h.is_dir),
        format!("search_files: {hits:?}"),
    )?;
    check(ws.stats().index_built, "index not built after a search")?;
    step("search_files");
    text_search(env, ws, config).await?;
    git_scenarios(ws, root).await?;
    git_commit(ws, root).await?;
    terminal(ws).await?;
    watch(ws, root).await
}

async fn text_search(env: &Env, ws: &Workspace, config: &WorkspaceConfig) -> R<()> {
    // ---- text search: progress before result, regex, case, cancel -------------------------------------
    let mut events = ws.subscribe();
    let found = ws
        .search_text(SearchText {
            search_id: "s1".into(),
            query: "needle".into(),
            is_regex: false,
            case_sensitive: false,
            max_results: 0,
        })
        .await
        .map_err(|e| format!("search_text: {e}"))?;
    check(
        found.match_count == 2 && found.file_count == 2,
        format!("search_text counts: {found:?}"),
    )?;
    check(
        found
            .matches
            .iter()
            .any(|m| m.path == "src/lib.rs" && m.line == 2 && m.text.contains("NEEDLE")),
        format!("search_text matches: {:?}", found.matches),
    )?;
    let progress = events
        .try_recv()
        .ok_or("no SearchProgress before the result")?;
    check(
        matches!(&progress.kind, WorkspaceEventKind::SearchProgress { search_id, .. } if search_id == "s1"),
        format!("first event was not progress: {progress:?}"),
    )?;
    let sensitive = ws
        .search_text(SearchText {
            search_id: "s2".into(),
            query: r"needle\s+two".into(),
            is_regex: true,
            case_sensitive: true,
            max_results: 0,
        })
        .await
        .map_err(|e| format!("regex search: {e}"))?;
    check(
        sensitive.match_count == 1
            && sensitive.matches.first().map(|m| m.path.as_str()) == Some("docs/guide.md"),
        format!("regex search: {sensitive:?}"),
    )?;
    check(
        ws.search_text(SearchText {
            search_id: "s3".into(),
            query: "(".into(),
            is_regex: true,
            case_sensitive: false,
            max_results: 0,
        })
        .await
        .is_err(),
        "invalid regex was accepted",
    )?;
    search_cancel(env, config).await?;
    check(ws.stats().searches_running == 0, "search left registered")?;
    step("search_text");
    Ok(())
}

/// Cancel a running search (large non-repo tree so the scan is still running when the first batch arrives).
async fn search_cancel(env: &Env, config: &WorkspaceConfig) -> R<()> {
    let big = env.home.join("search-big");
    if !big.exists() {
        let line = "match-me lorem ipsum dolor sit amet consectetur\n".repeat(400);
        for i in 0..400 {
            write(&big.join(format!("d{}/f{i}.txt", i % 20)), &line)?;
        }
    }
    let ws = Workspace::open(big, config.clone())
        .await
        .map_err(|e| format!("open big: {e}"))?;
    let status = ws
        .git_status()
        .await
        .map_err(|e| format!("git_status non-repo: {e}"))?;
    check(
        !status.is_repository,
        "search-big unexpectedly inside a repository (GIT_CEILING_DIRECTORIES missing?)",
    )?;
    let mut events = ws.subscribe();
    let runner = ws.clone();
    let task = tokio::spawn(async move {
        runner
            .search_text(SearchText {
                search_id: "cancel-me".into(),
                query: "match-me".into(),
                is_regex: false,
                case_sensitive: false,
                max_results: 1_000_000,
            })
            .await
    });
    next_event(&mut events, "first progress of the big search", |k| {
        matches!(k, WorkspaceEventKind::SearchProgress { search_id, .. } if search_id == "cancel-me")
    })
    .await?;
    ws.cancel_search("cancel-me".into())
        .await
        .map_err(|e| e.to_string())?;
    let outcome = task.await.map_err(|e| e.to_string())?;
    let shutdown = ws.shutdown().await;
    check(
        matches!(&outcome, Err(e) if e.to_string().contains("cancelled")),
        format!(
            "cancelled search returned {:?}",
            outcome.map(|r| r.match_count)
        ),
    )?;
    shutdown.map_err(|e| e.to_string())
}

async fn git_scenarios(ws: &Workspace, root: &Path) -> R<()> {
    let status = ws
        .git_status()
        .await
        .map_err(|e| format!("git_status: {e}"))?;
    check(
        status.is_repository
            && status.git_available
            && status.branch.as_deref() == Some("main")
            && status.staged.is_empty()
            && status.unstaged.is_empty(),
        format!("clean status: {status:?}"),
    )?;
    write(&root.join("README.md"), "hello\nthere\nworld\n")?;
    write(&root.join("new.txt"), "brand new\n")?;
    let status = ws.git_status().await.map_err(|e| e.to_string())?;
    check(
        status
            .unstaged
            .iter()
            .any(|c| c.path == "README.md" && c.kind == GitChangeKind::Modified)
            && status
                .unstaged
                .iter()
                .any(|c| c.path == "new.txt" && c.kind == GitChangeKind::Untracked),
        format!("dirty status: {status:?}"),
    )?;
    step("git_status");

    let diff = ws
        .git_file_diff("README.md".into(), false)
        .await
        .map_err(|e| format!("git_file_diff: {e}"))?;
    let added: Vec<_> = diff
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .filter(|l| l.kind == DiffLineKind::Added)
        .collect();
    check(
        diff.path == "README.md"
            && diff.added == 1
            && diff.removed == 0
            && added.len() == 1
            && added[0].text == "there"
            && added[0].new_line == Some(2),
        format!("worktree diff: {diff:?}"),
    )?;
    let empty = ws
        .git_file_diff("README.md".into(), true)
        .await
        .map_err(|e| e.to_string())?;
    check(
        empty.hunks.is_empty(),
        format!("staged diff before staging: {empty:?}"),
    )?;
    step("git_file_diff");

    ws.git_stage(vec!["README.md".into(), "new.txt".into()])
        .await
        .map_err(|e| format!("git_stage: {e}"))?;
    let status = ws.git_status().await.map_err(|e| e.to_string())?;
    check(
        status.staged.iter().any(|c| c.path == "README.md")
            && status.staged.iter().any(|c| c.path == "new.txt")
            && status.unstaged.is_empty(),
        format!("after stage: {status:?}"),
    )?;
    let staged = ws
        .git_file_diff("README.md".into(), true)
        .await
        .map_err(|e| e.to_string())?;
    check(staged.added == 1, format!("staged diff: {staged:?}"))?;
    ws.git_unstage(vec!["new.txt".into()])
        .await
        .map_err(|e| format!("git_unstage: {e}"))?;
    let status = ws.git_status().await.map_err(|e| e.to_string())?;
    check(
        status.staged.iter().any(|c| c.path == "README.md")
            && !status.staged.iter().any(|c| c.path == "new.txt")
            && status.unstaged.iter().any(|c| c.path == "new.txt"),
        format!("after unstage: {status:?}"),
    )?;
    check(
        ws.git_stage(vec!["../escape".into()]).await.is_err(),
        "stage outside the root was accepted",
    )?;
    step("git_stage_unstage");
    Ok(())
}

async fn git_commit(ws: &Workspace, root: &Path) -> R<()> {
    check(
        ws.git_commit("  ".into()).await.is_err(),
        "empty commit message accepted",
    )?;
    let commit = ws
        .git_commit("gate: second commit".into())
        .await
        .map_err(|e| format!("git_commit: {e}"))?;
    check(
        commit.hash.len() >= 40 && commit.summary == "gate: second commit",
        format!("commit: {commit:?}"),
    )?;
    check(
        git(root, &["rev-parse", "HEAD"])? == commit.hash,
        "commit hash differs from HEAD",
    )?;
    check(
        ws.git_commit("nothing".into()).await.is_err(),
        "commit with nothing staged accepted",
    )?;
    git(root, &["branch", "feature-x"])?;
    let branches = ws
        .git_branches()
        .await
        .map_err(|e| format!("git_branches: {e}"))?;
    check(
        branches.iter().any(|b| b.name == "main" && b.current)
            && branches.iter().any(|b| b.name == "feature-x" && !b.current),
        format!("branches: {branches:?}"),
    )?;
    // Leave the fixture clean for the turn-diff scenario.
    std::fs::remove_file(root.join("new.txt")).map_err(|e| e.to_string())?;
    step("git_commit");
    Ok(())
}

fn shell() -> (String, String, String) {
    if cfg!(windows) {
        (
            "cmd.exe".into(),
            "echo PTY_%OS%_OK\r\n".into(),
            "PTY_Windows_NT_OK".into(),
        )
    } else {
        (
            "/bin/sh".into(),
            "echo PTY_$((40+2))_OK\n".into(),
            "PTY_42_OK".into(),
        )
    }
}

async fn terminal(ws: &Workspace) -> R<()> {
    let (shell, echo, marker) = shell();
    let newline = if cfg!(windows) { "\r\n" } else { "\n" };
    let mut events = ws.subscribe();
    let first = ws
        .terminal_create(TerminalCreate {
            cols: 100,
            rows: 30,
            cwd: None,
            shell: Some(shell.clone()),
        })
        .await
        .map_err(|e| format!("terminal_create: {e}"))?;
    check(
        ws.stats().pty_started && ws.stats().terminals == 1,
        "pty stats",
    )?;
    ws.terminal_write(first.id.clone(), echo)
        .await
        .map_err(|e| format!("terminal_write: {e}"))?;
    let mut output = String::new();
    let id = first.id.clone();
    next_event(&mut events, "echo marker", |k| match k {
        WorkspaceEventKind::TerminalOutput { id: from, data } if from == &id => {
            output.push_str(data);
            output.contains(&marker)
        }
        _ => false,
    })
    .await?;
    ws.terminal_resize(first.id.clone(), 120, 40)
        .await
        .map_err(|e| format!("terminal_resize: {e}"))?;
    check(
        ws.terminal_resize(first.id.clone(), 0, 10).await.is_err(),
        "zero-size resize accepted",
    )?;
    ws.terminal_write(first.id.clone(), format!("exit 7{newline}"))
        .await
        .map_err(|e| e.to_string())?;
    let exited = next_event(
        &mut events,
        "terminal exit",
        |k| matches!(k, WorkspaceEventKind::TerminalExited { id: from, .. } if from == &id),
    )
    .await?;
    check(
        matches!(exited, WorkspaceEventKind::TerminalExited { code: 7, .. }),
        format!("exit event: {exited:?}"),
    )?;
    check(
        ws.stats().terminals == 0,
        "exited terminal still registered",
    )?;
    terminal_close(ws, &mut events, shell).await?;
    step("terminal");
    Ok(())
}

/// Close of a live terminal: no events after close, child gone.
async fn terminal_close(
    ws: &Workspace,
    events: &mut harness_core_api::workspace::WorkspaceEventStream,
    shell: String,
) -> R<()> {
    let second = ws
        .terminal_create(TerminalCreate {
            cols: 80,
            rows: 24,
            cwd: Some("src".into()),
            shell: Some(shell),
        })
        .await
        .map_err(|e| format!("terminal_create 2: {e}"))?;
    check(
        Path::new(&second.cwd).ends_with("src"),
        format!("terminal cwd: {}", second.cwd),
    )?;
    if let Some(pid) = second.pid {
        println!("AGENT_PID {pid}");
    }
    ws.terminal_close(second.id.clone())
        .await
        .map_err(|e| format!("terminal_close: {e}"))?;
    let closed_at = Instant::now();
    while let Some(event) = events.try_recv() {
        // Anything queued before the close returned is allowed; nothing may follow.
        let _ = event;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    while let Some(event) = events.try_recv() {
        check(
            !matches!(&event.kind, WorkspaceEventKind::TerminalOutput { id, .. } | WorkspaceEventKind::TerminalExited { id, .. } if id == &second.id),
            format!("event after close: {event:?}"),
        )?;
    }
    if let Some(pid) = second.pid {
        while crate::pid_alive(pid) {
            check(
                closed_at.elapsed() < Duration::from_secs(5),
                format!("terminal child {pid} survived close"),
            )?;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    check(
        ws.terminal_write(second.id.clone(), "x".into())
            .await
            .is_err(),
        "write to a closed terminal accepted",
    )?;
    check(
        ws.stats().terminals == 0,
        "closed terminal still registered",
    )?;
    Ok(())
}

async fn watch(ws: &Workspace, root: &Path) -> R<()> {
    let mut events = ws.subscribe();
    let watch_id = ws
        .watch_start()
        .await
        .map_err(|e| format!("watch_start: {e}"))?;
    check(
        ws.stats().watcher_started && ws.stats().watches == 1,
        "watch stats",
    )?;
    // The OS watcher arms asynchronously; write until the change is seen (bounded by the event timeout).
    let target = root.join("docs").join("watched.txt");
    let deadline = Instant::now() + EVENT_TIMEOUT;
    let mut seen = false;
    let mut round = 0u32;
    while !seen && Instant::now() < deadline {
        round += 1;
        write(&target, &format!("change {round}\n"))?;
        let wait_until = Instant::now() + Duration::from_millis(700);
        while Instant::now() < wait_until {
            let left = wait_until.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, events.recv()).await {
                Ok(Some(event)) => {
                    if let WorkspaceEventKind::FilesChanged {
                        watch_id: from,
                        paths,
                        rescan,
                    } = &event.kind
                    {
                        if from == &watch_id
                            && (*rescan || paths.iter().any(|p| p == "docs/watched.txt"))
                        {
                            seen = true;
                            break;
                        }
                    }
                }
                Ok(None) => return Err("event stream closed during watch".into()),
                Err(_) => break,
            }
        }
    }
    check(seen, "no FilesChanged event for docs/watched.txt")?;
    ws.watch_stop(watch_id.clone())
        .await
        .map_err(|e| format!("watch_stop: {e}"))?;
    check(ws.stats().watches == 0, "watch still registered after stop")?;
    check(
        ws.watch_stop(watch_id).await.is_err(),
        "second watch_stop accepted",
    )?;
    std::fs::remove_file(&target).map_err(|e| e.to_string())?;
    step("watch");
    Ok(())
}

async fn turn_diff(env: &Env, gate: &Gate, root: &Path) -> R<()> {
    let summary = gate
        .hub
        .create_session(harness_core_api::CreateSession {
            workspace_path: root.to_string_lossy().into_owned(),
            workspace_identity: None,
            title: Some("turn diff".into()),
            agent: Some(harness_core_api::AgentId::new(A)),
            worktree: None,
        })
        .await
        .map_err(|e| format!("create_session: {e}"))?;
    let session = summary.id;
    let edit = gate
        .send(
            &session,
            "edit please [[say before-edit]] [[write notes/agent.txt AGENT-LINE]] [[say after-edit]]",
            None,
        )
        .await?;
    let view = gate.completed(&session, &edit).await?;
    check(
        root.join("notes/agent.txt").exists(),
        "fake agent did not write the file",
    )?;
    let diff = gate
        .hub
        .turn_diff(session.clone(), edit.clone())
        .await
        .map_err(|e| format!("turn_diff: {e}"))?;
    check(
        diff.available && !diff.truncated && diff.files.len() == 1,
        format!("turn diff: {diff:?}"),
    )?;
    let file = &diff.files[0];
    check(
        file.path == "notes/agent.txt"
            && file.status == FileDiffStatus::Added
            && file.added == 1
            && file
                .hunks
                .iter()
                .flat_map(|h| h.lines.iter())
                .any(|l| l.kind == DiffLineKind::Added && l.text == "AGENT-LINE"),
        format!("turn diff file: {file:?}"),
    )?;
    // The user's index is never touched by snapshots.
    check(
        git(root, &["diff", "--cached", "--name-only"])?.is_empty(),
        "snapshot changed the user's index",
    )?;

    // segments: text → tool → text (API 0.2.0)
    let kinds: Vec<&str> = view
        .segments
        .iter()
        .map(|s| match s {
            TurnSegment::Text { .. } => "text",
            TurnSegment::Tool { .. } => "tool",
            TurnSegment::Steer { .. } => "steer",
        })
        .collect();
    let tool_at = kinds.iter().position(|k| *k == "tool");
    check(
        tool_at.is_some_and(|i| {
            i > 0 && kinds[..i].contains(&"text") && kinds[i + 1..].contains(&"text")
        }),
        format!("segments: {:?}", view.segments),
    )?;
    if let Some(TurnSegment::Text { text }) = view.segments.first() {
        check(
            text.contains("before-edit") && !text.contains("after-edit"),
            format!("first text segment: {text:?}"),
        )?;
    }

    let quiet = gate.send(&session, "no edits this time", None).await?;
    gate.completed(&session, &quiet).await?;
    let none = gate
        .hub
        .turn_diff(session.clone(), quiet)
        .await
        .map_err(|e| e.to_string())?;
    check(
        none.available && none.files.is_empty(),
        format!("turn without edits: {none:?}"),
    )?;

    // Non-repository workspace (the gate's default workspace) → available: false.
    let plain = gate.session(env, A, "non repo").await?;
    let plain_turn = gate.send(&plain, "plain [[write x.txt X]]", None).await?;
    gate.completed(&plain, &plain_turn).await?;
    let unavailable = gate
        .hub
        .turn_diff(plain, plain_turn)
        .await
        .map_err(|e| e.to_string())?;
    check(
        !unavailable.available && unavailable.files.is_empty() && unavailable.reason.is_some(),
        format!("non-repo turn diff: {unavailable:?}"),
    )?;
    let _ = std::fs::remove_file(env.workspace.join("x.txt"));
    step("turn_diff");
    Ok(())
}

async fn api_8_2(env: &Env, gate: &Gate) -> R<()> {
    agents_file(env).await?;
    running_and_agents_used(gate, env).await?;

    // ---- local time label ---------------------------------------------------------------------------
    let now = crate::now_secs();
    let today = harness_core_api::time::local_time_label_at(now, now);
    let older = harness_core_api::time::local_time_label_at(now - 3 * 86_400, now);
    let is_clock = |s: &str| {
        s.len() == 5 && s.as_bytes()[2] == b':' && s.bytes().filter(u8::is_ascii_digit).count() == 4
    };
    check(is_clock(&today), format!("today label: {today:?}"))?;
    check(
        older.len() == 11 && older.as_bytes()[2] == b'-' && is_clock(&older[6..]),
        format!("older label: {older:?}"),
    )?;
    step("api_8_2");
    Ok(())
}

/// `<home>/agents.json` applied by `Hub::start` (second hub in its own home).
async fn agents_file(env: &Env) -> R<()> {
    let home = env.home.join("agents-home");
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    let program = env.exe("harness-fake-agent");
    let file = serde_json::json!({
        "replaceBuiltins": false,
        "agents": [{
            "id": "fake-d",
            "displayName": "Fake D (agents.json)",
            "program": program.to_string_lossy(),
            "args": ["--name", "fake-d"],
            "dialect": "generic"
        }]
    });
    write(&home.join("agents.json"), &file.to_string())?;
    let mut config: HubConfig = env.config();
    config.home = home;
    config.load_agents_file = true;
    let hub = Hub::start(config)
        .await
        .map_err(|e| format!("hub with agents.json: {e}"))?;
    let agents = hub.list_agents().await.map_err(|e| e.to_string());
    let fallback = hub.get_fallback_order().await.map_err(|e| e.to_string());
    hub.shutdown().await.map_err(|e| e.to_string())?;
    let agents = agents?;
    check(
        agents
            .iter()
            .any(|a| a.id.as_str() == "fake-d" && a.display_name == "Fake D (agents.json)")
            && agents.iter().any(|a| a.id.as_str() == A),
        format!(
            "agents.json not applied: {:?}",
            agents.iter().map(|a| a.id.as_str()).collect::<Vec<_>>()
        ),
    )?;
    check(
        fallback?.last().map(harness_core_api::AgentId::as_str) == Some("fake-d"),
        "agents.json agent not appended to the fallback order",
    )?;
    Ok(())
}

/// Running text in the snapshot, then `agents_used` after a manual swap.
async fn running_and_agents_used(gate: &Gate, env: &Env) -> R<()> {
    let session = gate.session(env, A, "running snapshot").await?;
    let turn = gate.send(&session, "stream [[slow 60]]", None).await?;
    gate.wait_event("third delta of the running turn", |e| {
        matches!(&e.kind, EventKind::MessageDelta { turn_id, text, .. } if turn_id == &turn && text.starts_with("chunk-2"))
    })
    .await?;
    let running = gate.turn(&session, &turn).await?;
    check(
        running.status == TurnStatus::Running && running.assistant_text.contains("chunk-2"),
        format!(
            "running snapshot: {:?} {:?}",
            running.status, running.assistant_text
        ),
    )?;
    gate.hub
        .stop(session.clone())
        .await
        .map_err(|e| e.to_string())?;
    gate.finished(&session, &turn).await?;

    // ---- agents_used --------------------------------------------------------------------------------
    let switch = gate.send(&session, "on b", Some(crate::B)).await?;
    gate.completed(&session, &switch).await?;
    let list = gate
        .hub
        .list_sessions(ListSessions::default())
        .await
        .map_err(|e| e.to_string())?;
    let row = list
        .iter()
        .find(|s| s.id == session)
        .ok_or("session missing from list")?;
    check(
        row.agents_used
            .iter()
            .map(harness_core_api::AgentId::as_str)
            .collect::<Vec<_>>()
            == [A, crate::B],
        format!("agents_used: {:?}", row.agents_used),
    )?;
    let detail = gate
        .hub
        .open_session(session.clone())
        .await
        .map_err(|e| e.to_string())?;
    check(
        detail.summary.agents_used == row.agents_used,
        "agents_used differs between list and open",
    )?;
    let _: &SessionId = &session;
    Ok(())
}
