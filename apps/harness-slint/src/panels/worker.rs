//! The `harness-panels` worker: one current-thread tokio runtime that owns the workspace services, the panel
//! state and the terminal emulator. Single writer of panel state; results of spawned operations come back as
//! [`Done`] messages and are applied only when their sequence number is still the latest (stale results of a
//! superseded refresh / search are dropped, never raced).

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use harness_core_api::diff::FileDiff;
use harness_core_api::workspace::{
    GitChangeKind, GitFileChange, GitSection, GitStatus, SearchText, TerminalCreate,
    WorkspaceEventKind, WorkspaceEventStream, WorkspaceServices,
};
use harness_core_api::{ApiError, SessionId, TurnId, TurnStatus};
use harness_workspace::{Workspace, WorkspaceConfig};
use slint::Weak;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::diffmodel::{self, file_view};
use super::term::{self, Emulator};
use super::view::{self, DiffView, GitFile, GitView, Hit, TermFrame, TermStatus};
use super::{with_hub, Cmd};
use crate::ui::AppWindow;

/// Terminal / search repaint coalescing (one frame).
const FRAME: Duration = Duration::from_millis(16);
/// Git refresh debounce after watcher events.
const GIT_DEBOUNCE: Duration = Duration::from_millis(300);
const MAX_HITS: usize = 500;
const FILE_HITS: u32 = 200;
const PREVIEW_BEFORE: u64 = 10;
const PREVIEW_AFTER: u64 = 60;
const PREVIEW_FILE_LINES: u64 = 200;

/// Results of spawned operations.
enum Done {
    Git(u64, Result<GitStatus, ApiError>),
    Action {
        result: Result<String, String>,
        clear_message: bool,
    },
    /// A git call changed the repository: refresh the Git panel quietly.
    Changed,
    Diff(u64, DiffSource, Result<Vec<FileDiff>, String>),
    Files(u64, Result<Vec<Hit>, ApiError>),
    Text(u64, Result<(Vec<Hit>, u64, u64, bool), ApiError>),
    Preview(u64, String, Result<Vec<(u64, String, bool)>, String>),
}

/// What the Diff panel currently shows (for refresh).
#[derive(Debug, Clone)]
enum DiffSource {
    Git {
        path: String,
        staged: bool,
    },
    Turn {
        session: SessionId,
        turn: TurnId,
        label: String,
    },
}

/// Ordered terminal I/O (writes and resizes must reach the PTY in UI order).
enum TermOp {
    Write(String, String),
    Resize(String, u16, u16),
}

struct Terminal {
    id: Option<String>,
    emu: Emulator,
    /// Rows last sent to the UI.
    shown: Vec<term::Line>,
    status: TermStatus,
    dirty: bool,
}

struct Worker {
    window: Weak<AppWindow>,
    /// `None` = no session or folder open yet: the panels open nothing.
    root: Option<PathBuf>,
    ws: Option<Workspace>,
    events: Option<WorkspaceEventStream>,
    done_tx: mpsc::UnboundedSender<Done>,
    term_tx: Option<mpsc::UnboundedSender<TermOp>>,
    seq: u64,
    git_seq: u64,
    git: Option<GitStatus>,
    watch: Option<String>,
    git_due: Option<Instant>,
    diff_seq: u64,
    diff_source: Option<DiffSource>,
    diff_files: Vec<FileDiff>,
    diff_current: usize,
    search_seq: u64,
    search_id: Option<String>,
    hits: Vec<Hit>,
    search_files: bool,
    search_dirty: bool,
    preview_seq: u64,
    size: Option<(u16, u16)>,
    terminal: Option<Terminal>,
    frame_due: Option<Instant>,
}

/// Per-worker cache of content checks (spec `file-safety.md` §4).
fn content_cache() -> &'static harness_workspace::filetype::ContentCache {
    static CACHE: OnceLock<harness_workspace::filetype::ContentCache> = OnceLock::new();
    CACHE.get_or_init(harness_workspace::filetype::ContentCache::new)
}

/// Warning line for a workspace-relative path, joined with any existing notice.
fn with_content_warning(root: Option<&PathBuf>, path: &str, notice: String) -> String {
    let Some(root) = root else {
        return notice;
    };
    let check = content_cache().inspect(&root.join(path));
    let Some(warning) = check.warning else {
        return notice;
    };
    if notice.is_empty() {
        warning
    } else {
        format!("{notice} · {warning}")
    }
}

/// Thread body: builds the runtime and runs the worker until `Cmd::Shutdown` / the channel closes.
pub(crate) fn run(
    window: Weak<AppWindow>,
    root: Option<PathBuf>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("harness-slint: cannot start the panels runtime: {error}");
            return;
        }
    };
    runtime.block_on(async move {
        let (done_tx, mut done_rx) = mpsc::unbounded_channel();
        // 修复：没有打开会话/文件夹时不再回退到当前目录（否则面板显示无关文件夹的 Git 状态）。
        let root = root.or_else(|| {
            std::env::var_os("HARNESS_WORKSPACE")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        });
        let mut worker = Worker {
            window,
            root,
            ws: None,
            events: None,
            done_tx,
            term_tx: None,
            seq: 0,
            git_seq: 0,
            git: None,
            watch: None,
            git_due: None,
            diff_seq: 0,
            diff_source: None,
            diff_files: Vec::new(),
            diff_current: 0,
            search_seq: 0,
            search_id: None,
            hits: Vec::new(),
            search_files: true,
            search_dirty: false,
            preview_seq: 0,
            size: None,
            terminal: None,
            frame_due: None,
        };
        loop {
            let frame_due = worker.frame_due;
            let git_due = worker.git_due;
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(Cmd::Shutdown) | None => break,
                    Some(cmd) => worker.command(cmd).await,
                },
                Some(done) = done_rx.recv() => worker.done(done),
                event = next_event(&mut worker.events) => match event {
                    Some(kind) => worker.event(kind),
                    None => worker.events = None,
                },
                () = sleep_until(frame_due) => worker.flush(),
                () = sleep_until(git_due) => {
                    worker.git_due = None;
                    worker.git_refresh(false);
                    worker.refresh_git_diff();
                }
            }
        }
        super::trace("worker: loop ended");
        worker.shutdown().await;
        super::trace("worker: workspace shut down");
    });
    drop(runtime);
    super::trace("worker: runtime dropped");
}

async fn next_event(events: &mut Option<WorkspaceEventStream>) -> Option<WorkspaceEventKind> {
    match events {
        Some(stream) => stream.recv().await.map(|e| e.kind),
        None => std::future::pending().await,
    }
}

async fn sleep_until(due: Option<Instant>) {
    match due {
        Some(due) => tokio::time::sleep_until(due).await,
        None => std::future::pending().await,
    }
}

fn change_code(kind: GitChangeKind) -> (&'static str, &'static str) {
    match kind {
        GitChangeKind::Modified => ("M", "modified"),
        GitChangeKind::Added => ("A", "added"),
        GitChangeKind::Deleted => ("D", "deleted"),
        GitChangeKind::Renamed => ("R", "renamed"),
        GitChangeKind::Untracked => ("U", "untracked"),
        GitChangeKind::Conflicted => ("C", "conflicted"),
    }
}

fn git_file(change: &GitFileChange) -> GitFile {
    let (code, kind) = change_code(change.kind);
    GitFile {
        path: change.path.clone(),
        code,
        kind,
        added: change.added,
        removed: change.removed,
        staged: change.section == GitSection::Staged,
    }
}

fn error_text(error: &ApiError) -> String {
    match error {
        ApiError::InvalidArgument(text) | ApiError::Internal(text) => text.clone(),
        other => other.to_string(),
    }
}

impl Worker {
    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Opens the workspace on first use (no thread, process or index before a panel asks for one).
    async fn workspace(&mut self) -> Option<Workspace> {
        if let Some(ws) = &self.ws {
            return Some(ws.clone());
        }
        let Some(root) = self.root.clone() else {
            let text = "Open a folder or a session first".to_owned();
            view::git_result(&self.window, text.clone(), true, false);
            view::term_status(
                &self.window,
                TermStatus {
                    error: text,
                    ..TermStatus::default()
                },
            );
            return None;
        };
        match Workspace::open(root, WorkspaceConfig::default()).await {
            Ok(ws) => {
                self.events = Some(ws.subscribe());
                self.ws = Some(ws.clone());
                Some(ws)
            }
            Err(error) => {
                let text = format!("Workspace unavailable: {}", error_text(&error));
                view::git_result(&self.window, text.clone(), true, false);
                view::search(&self.window, Vec::new(), false, String::new(), text.clone());
                view::term_status(
                    &self.window,
                    TermStatus {
                        error: text,
                        ..TermStatus::default()
                    },
                );
                None
            }
        }
    }

    /// 修复：面板（Git/终端/搜索/Diff）一直停在启动时的工作区；打开另一个文件夹的会话后切换到该会话的工作区。
    async fn set_root(&mut self, root: PathBuf) {
        if self.root.as_ref() == Some(&root) {
            return;
        }
        // 之前没有工作区时，已打开的面板显示过“先打开文件夹”，现在也要刷新。
        let was_open = self.ws.is_some() || self.root.is_none();
        let had_terminal = self.terminal.is_some() || (self.root.is_none() && self.size.is_some());
        // 关闭旧工作区（终端、监视器、搜索），再清空依赖它的面板状态。
        self.shutdown().await;
        self.root = Some(root);
        self.events = None;
        self.watch = None;
        self.git = None;
        self.git_due = None;
        self.diff_source = None;
        self.diff_files.clear();
        self.diff_current = 0;
        self.search_id = None;
        self.hits.clear();
        self.terminal = None;
        self.frame_due = None;
        view::diff(&self.window, DiffView::default());
        view::search(
            &self.window,
            Vec::new(),
            false,
            String::new(),
            String::new(),
        );
        if !was_open {
            return;
        }
        let Some(ws) = self.workspace().await else {
            return;
        };
        match ws.watch_start().await {
            Ok(id) => self.watch = Some(id),
            Err(error) => eprintln!("harness-slint: watcher unavailable: {error}"),
        }
        self.git_refresh(true);
        if had_terminal {
            self.term_start(&ws).await;
        }
    }

    async fn command(&mut self, cmd: Cmd) {
        if let Cmd::SetRoot(root) = cmd {
            self.set_root(root).await;
            return;
        }
        let Some(ws) = self.workspace().await else {
            return;
        };
        match cmd {
            Cmd::GitRefresh => {
                if self.watch.is_none() {
                    // The watcher starts with the git panel and then stays on (spec §10): it keeps the
                    // status and a shown working-tree diff fresh.
                    match ws.watch_start().await {
                        Ok(id) => self.watch = Some(id),
                        Err(error) => eprintln!("harness-slint: watcher unavailable: {error}"),
                    }
                }
                self.git_refresh(true);
            }
            Cmd::Stage(path) => self.git_action(&ws, vec![path], true),
            Cmd::Unstage(path) => self.git_action(&ws, vec![path], false),
            Cmd::StageAll => {
                let paths = self
                    .git
                    .as_ref()
                    .map(|g| g.unstaged.iter().map(|c| c.path.clone()).collect());
                self.git_action(&ws, paths.unwrap_or_default(), true);
            }
            Cmd::UnstageAll => {
                let paths = self
                    .git
                    .as_ref()
                    .map(|g| g.staged.iter().map(|c| c.path.clone()).collect());
                self.git_action(&ws, paths.unwrap_or_default(), false);
            }
            Cmd::Commit(message) => self.git_commit(&ws, message),
            Cmd::Git(call) => self.git_call(&ws, call),
            Cmd::OpenDiff { path, staged } => self.load_diff(DiffSource::Git { path, staged }),
            Cmd::DiffSelect(index) => {
                if index < self.diff_files.len() {
                    self.diff_current = index;
                    self.render_diff();
                }
            }
            Cmd::ShowTurn => self.show_turn().await,
            Cmd::DiffRefresh => match self.diff_source.clone() {
                Some(source) => self.load_diff(source),
                None => self.show_turn().await,
            },
            Cmd::TurnFinished => {
                if matches!(self.diff_source, Some(DiffSource::Turn { .. })) {
                    self.show_turn().await;
                }
            }
            Cmd::Search {
                query,
                files,
                regex,
                case_sensitive,
            } => self.search(&ws, query, files, regex, case_sensitive),
            Cmd::SearchOpen(index) => self.open_hit(&ws, index),
            Cmd::SearchClosePreview => {
                self.preview_seq = self.next_seq();
                view::preview(&self.window, String::new(), Vec::new());
            }
            Cmd::TermKey { text, ctrl, alt } => self.term_key(&text, ctrl, alt),
            Cmd::TermResize { cols, rows } => self.term_resize(&ws, cols, rows).await,
            Cmd::TermScroll(lines) => {
                if let Some(terminal) = &mut self.terminal {
                    terminal.emu.scroll(lines);
                    terminal.dirty = true;
                    self.schedule_frame();
                }
            }
            Cmd::TermRestart => {
                self.term_kill(&ws).await;
                self.term_start(&ws).await;
            }
            Cmd::TermClose => {
                self.term_kill(&ws).await;
                if let Some(terminal) = &mut self.terminal {
                    terminal.status.running = false;
                    terminal.status.exited = true;
                    terminal.status.exit_code = -1;
                    view::term_status(&self.window, terminal.status.clone());
                }
            }
            Cmd::Shutdown | Cmd::SetRoot(_) => {}
        }
    }

    // ---- git --------------------------------------------------------------------------------------------------

    fn git_refresh(&mut self, show_loading: bool) {
        let Some(ws) = self.ws.clone() else {
            return;
        };
        self.git_seq = self.next_seq();
        let seq = self.git_seq;
        if show_loading {
            view::git_loading(&self.window, true);
        }
        let done = self.done_tx.clone();
        tokio::spawn(async move {
            let _ = done.send(Done::Git(seq, ws.git_status().await));
        });
    }

    fn git_action(&mut self, ws: &Workspace, paths: Vec<String>, stage: bool) {
        if paths.is_empty() {
            return;
        }
        let ws = ws.clone();
        let done = self.done_tx.clone();
        view::git_loading(&self.window, true);
        tokio::spawn(async move {
            let count = paths.len();
            let result = if stage {
                ws.git_stage(paths).await
            } else {
                ws.git_unstage(paths).await
            };
            let verb = if stage { "Staged" } else { "Unstaged" };
            let result = result
                .map(|()| format!("{verb} {count} file(s)"))
                .map_err(|e| error_text(&e));
            let _ = done.send(Done::Action {
                result,
                clear_message: false,
            });
        });
    }

    fn git_commit(&mut self, ws: &Workspace, message: String) {
        let ws = ws.clone();
        let done = self.done_tx.clone();
        view::git_loading(&self.window, true);
        tokio::spawn(async move {
            let result = ws
                .git_commit(message)
                .await
                .map(|c| {
                    format!(
                        "Committed {} {}",
                        c.hash.chars().take(7).collect::<String>(),
                        c.summary
                    )
                })
                .map_err(|e| error_text(&e));
            let clear_message = result.is_ok();
            let _ = done.send(Done::Action {
                result,
                clear_message,
            });
        });
    }

    /// R19 calls: each runs in its own task and answers on its channel; changes refresh the Git panel.
    fn git_call(&mut self, ws: &Workspace, call: super::GitCall) {
        use super::GitCall;
        let ws = ws.clone();
        let done = self.done_tx.clone();
        tokio::spawn(async move {
            let changed = match call {
                GitCall::Branches(reply) => {
                    let _ = reply.send(ws.git_branch_list().await);
                    false
                }
                GitCall::Log { skip, limit, reply } => {
                    let _ = reply.send(ws.git_log(skip, limit).await);
                    false
                }
                GitCall::Switch {
                    branch,
                    create,
                    reply,
                } => {
                    let _ = reply.send(ws.git_switch(branch, create).await);
                    true
                }
                GitCall::Commit {
                    message,
                    include_unstaged,
                    reply,
                } => {
                    let _ = reply.send(ws.git_commit_with(message, include_unstaged).await);
                    true
                }
                GitCall::Push {
                    remote,
                    branch,
                    progress,
                    reply,
                } => {
                    let report = move |p| {
                        if let Some(tx) = &progress {
                            let _ = tx.send(p);
                        }
                    };
                    let _ = reply.send(ws.git_push(remote, branch, &report).await);
                    true
                }
            };
            if changed {
                let _ = done.send(Done::Changed);
            }
        });
    }

    fn apply_git(&mut self, status: GitStatus) {
        let sync = [(status.ahead, '↑'), (status.behind, '↓')]
            .iter()
            .filter(|(n, _)| *n > 0)
            .map(|(n, arrow)| format!("{arrow}{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        let branch = match (&status.branch, status.detached) {
            (Some(branch), false) => branch.clone(),
            (Some(branch), true) => format!("({branch})"),
            (None, _) => String::new(),
        };
        let view = GitView {
            is_repo: status.is_repository,
            branch,
            sync,
            staged: status.staged.iter().map(git_file).collect(),
            unstaged: status.unstaged.iter().map(git_file).collect(),
            branches: Vec::new(),
        };
        self.git = Some(status);
        view::git_status(&self.window, view);
    }

    /// A shown working-tree diff follows the files (watcher / git action).
    fn refresh_git_diff(&mut self) {
        if let Some(source @ DiffSource::Git { .. }) = self.diff_source.clone() {
            self.load_diff(source);
        }
    }

    // ---- diff -------------------------------------------------------------------------------------------------

    fn load_diff(&mut self, source: DiffSource) {
        let Some(ws) = self.ws.clone() else {
            return;
        };
        self.diff_seq = self.next_seq();
        let seq = self.diff_seq;
        view::diff_loading(&self.window);
        let done = self.done_tx.clone();
        match source.clone() {
            DiffSource::Git { path, staged } => {
                tokio::spawn(async move {
                    let result = ws
                        .git_file_diff(path, staged)
                        .await
                        .map(|d| vec![d])
                        .map_err(|e| error_text(&e));
                    let _ = done.send(Done::Diff(seq, source, result));
                });
            }
            DiffSource::Turn { session, turn, .. } => {
                let Some(core) = with_hub(|hub| hub.core.clone()) else {
                    self.diff_notice("Turn diffs need the agent hub (not attached).");
                    return;
                };
                tokio::spawn(async move {
                    let result = match core.turn_diff(session, turn).await {
                        Ok(diff) if diff.available => Ok(diff.files),
                        Ok(diff) => Err(diff
                            .reason
                            .unwrap_or_else(|| "No snapshot for this turn".into())),
                        Err(error) => Err(error_text(&error)),
                    };
                    let _ = done.send(Done::Diff(seq, source, result));
                });
            }
        }
    }

    /// Diff of the last finished turn: of the session shown in the chat when known, else of this run.
    async fn show_turn(&mut self) {
        let (core, active, last) = with_hub(|hub| {
            (
                hub.core.clone(),
                hub.active_session.clone(),
                hub.last_turn.clone(),
            )
        });
        let Some(core) = core else {
            self.diff_notice("Turn diffs need the agent hub (not attached).");
            return;
        };
        let mut target = last;
        if let Some(session) = active {
            if let Ok(detail) = core.open_session(session.clone()).await {
                let finished = detail.turns.iter().rev().find(|t| {
                    matches!(
                        t.status,
                        TurnStatus::Completed | TurnStatus::Cancelled | TurnStatus::Failed
                    )
                });
                if let Some(turn) = finished {
                    target = Some((session, turn.id.clone()));
                }
            }
        }
        let Some((session, turn)) = target else {
            self.diff_notice("No finished turn yet.");
            return;
        };
        let label = format!("Turn {}", turn.0.chars().take(8).collect::<String>());
        self.load_diff(DiffSource::Turn {
            session,
            turn,
            label,
        });
    }

    fn diff_notice(&mut self, text: &str) {
        self.diff_seq = self.next_seq();
        self.diff_source = None;
        self.diff_files.clear();
        view::diff(
            &self.window,
            DiffView {
                source: "turn",
                title: "Turn changes".into(),
                notice: text.into(),
                ..DiffView::default()
            },
        );
    }

    fn render_diff(&self) {
        let (source, title, subtitle) = match &self.diff_source {
            Some(DiffSource::Git { path, staged }) => (
                "git",
                path.clone(),
                if *staged { "Staged" } else { "Working tree" }.to_owned(),
            ),
            Some(DiffSource::Turn { label, .. }) => {
                ("turn", "Turn changes".to_owned(), label.clone())
            }
            None => ("", String::new(), String::new()),
        };
        let added = self.diff_files.iter().map(|f| u64::from(f.added)).sum();
        let removed = self.diff_files.iter().map(|f| u64::from(f.removed)).sum();
        let files = self
            .diff_files
            .iter()
            .map(|f| {
                (
                    f.path.clone(),
                    diffmodel::status_code(f.status),
                    u64::from(f.added),
                    u64::from(f.removed),
                )
            })
            .collect();
        let mut out = DiffView {
            source,
            title,
            subtitle,
            added,
            removed,
            files,
            current_file: self.diff_current,
            ..DiffView::default()
        };
        match self.diff_files.get(self.diff_current) {
            None => out.notice = "No changes.".into(),
            Some(file) => {
                let fv = file_view(file);
                out.notice = match fv.notice {
                    Some("binary") => "Binary file — no text diff.".into(),
                    Some(_) => "No textual changes.".into(),
                    None if fv.cut => format!("Diff cut at {} rows.", diffmodel::MAX_ROWS),
                    None => String::new(),
                };
                out.rows = fv.rows;
                out.pairs = fv.pairs;
                out.notice = with_content_warning(
                    self.root.as_ref(),
                    &file.path,
                    std::mem::take(&mut out.notice),
                );
            }
        }
        view::diff(&self.window, out);
    }

    // ---- search -----------------------------------------------------------------------------------------------

    fn search(
        &mut self,
        ws: &Workspace,
        query: String,
        files: bool,
        regex: bool,
        case_sensitive: bool,
    ) {
        self.search_seq = self.next_seq();
        let seq = self.search_seq;
        if let Some(previous) = self.search_id.take() {
            let ws = ws.clone();
            tokio::spawn(async move {
                let _ = ws.cancel_search(previous).await;
            });
        }
        self.hits.clear();
        self.search_files = files;
        if query.trim().is_empty() {
            view::search(
                &self.window,
                Vec::new(),
                false,
                String::new(),
                String::new(),
            );
            return;
        }
        view::search(&self.window, Vec::new(), true, String::new(), String::new());
        let ws = ws.clone();
        let done = self.done_tx.clone();
        if files {
            tokio::spawn(async move {
                let result = ws.search_files(query, FILE_HITS).await.map(|hits| {
                    hits.into_iter()
                        .map(|h| Hit {
                            path: h.path,
                            is_dir: h.is_dir,
                            ..Hit::default()
                        })
                        .collect()
                });
                let _ = done.send(Done::Files(seq, result));
            });
        } else {
            let search_id = format!("panel-{seq}");
            self.search_id = Some(search_id.clone());
            tokio::spawn(async move {
                let request = SearchText {
                    search_id,
                    query,
                    is_regex: regex,
                    case_sensitive,
                    max_results: u32::try_from(MAX_HITS).unwrap_or(500),
                };
                let result = ws.search_text(request).await.map(|r| {
                    let hits = r
                        .matches
                        .into_iter()
                        .map(|m| Hit {
                            path: m.path,
                            line: m.line,
                            column: m.column,
                            text: m.text,
                            is_dir: false,
                        })
                        .collect();
                    (hits, r.match_count, r.file_count, r.truncated)
                });
                let _ = done.send(Done::Text(seq, result));
            });
        }
    }

    fn open_hit(&mut self, ws: &Workspace, index: usize) {
        let Some(hit) = self.hits.get(index).cloned() else {
            return;
        };
        if hit.is_dir {
            return;
        }
        self.preview_seq = self.next_seq();
        let seq = self.preview_seq;
        let ws = ws.clone();
        let done = self.done_tx.clone();
        tokio::spawn(async move {
            let result = match ws.read_file(hit.path.clone(), 0).await {
                Ok(file) if file.binary => Err("Binary file".to_owned()),
                Ok(file) => {
                    let (first, last) = if hit.line > 0 {
                        (
                            hit.line.saturating_sub(PREVIEW_BEFORE).max(1),
                            hit.line + PREVIEW_AFTER,
                        )
                    } else {
                        (1, PREVIEW_FILE_LINES)
                    };
                    Ok(file
                        .text
                        .lines()
                        .zip(1_u64..)
                        .skip_while(|(_, no)| *no < first)
                        .take_while(|(_, no)| *no <= last)
                        .map(|(text, no)| (no, text.replace('\t', "    "), no == hit.line))
                        .collect())
                }
                Err(error) => Err(error_text(&error)),
            };
            let _ = done.send(Done::Preview(seq, hit.path, result));
        });
    }

    fn search_summary(&self, done: Option<(u64, u64, bool)>) -> String {
        let mut out = match done {
            Some((matches, files, truncated)) => {
                let more = if truncated { "+" } else { "" };
                format!("{matches}{more} matches in {files} files")
            }
            None if self.search_files => format!("{} files", self.hits.len()),
            None => format!("{} matches…", self.hits.len()),
        };
        if !self.search_files {
            return out;
        }
        if let Some(warning) = self.content_summary() {
            out.push_str(" · ");
            out.push_str(&warning);
        }
        out
    }

    /// `n files have content that does not match the name`, over the visible file hits.
    fn content_summary(&self) -> Option<String> {
        let root = self.root.as_ref()?;
        let mut suspect = 0_u32;
        for hit in self.hits.iter().filter(|h| !h.is_dir) {
            if content_cache()
                .inspect(&root.join(&hit.path))
                .warning
                .is_some()
            {
                suspect += 1;
            }
        }
        (suspect > 0).then(|| {
            if suspect == 1 {
                "1 file has content that does not match its name".to_owned()
            } else {
                format!("{suspect} files have content that does not match their names")
            }
        })
    }

    // ---- terminal ---------------------------------------------------------------------------------------------

    /// First size from the panel = the panel was opened: start the shell (lazy).
    async fn term_resize(&mut self, ws: &Workspace, cols: u16, rows: u16) {
        self.size = Some((cols, rows));
        match &mut self.terminal {
            None => self.term_start(ws).await,
            Some(terminal) => {
                terminal.emu.resize(cols, rows);
                terminal.dirty = true;
                if let (Some(id), Some(tx)) = (&terminal.id, &self.term_tx) {
                    let _ = tx.send(TermOp::Resize(id.clone(), cols, rows));
                }
                self.schedule_frame();
            }
        }
    }

    async fn term_start(&mut self, ws: &Workspace) {
        let (cols, rows) = self.size.unwrap_or((80, 24));
        if self.term_tx.is_none() {
            let (tx, mut rx) = mpsc::unbounded_channel::<TermOp>();
            let ws = ws.clone();
            tokio::spawn(async move {
                while let Some(op) = rx.recv().await {
                    let result = match op {
                        TermOp::Write(id, data) => ws.terminal_write(id, data).await,
                        TermOp::Resize(id, cols, rows) => ws.terminal_resize(id, cols, rows).await,
                    };
                    if let Err(error) = result {
                        // The shell may just have exited; the exit event reports that.
                        let _ = error;
                    }
                }
            });
            self.term_tx = Some(tx);
        }
        // Created inline (not spawned): the id is known before the loop handles the shell's first output, so
        // no early output can be dropped as "unknown terminal".
        let created = ws
            .terminal_create(TerminalCreate {
                cols,
                rows,
                cwd: None,
                shell: None,
            })
            .await;
        let (id, status) = match created {
            Ok(info) => (
                Some(info.id),
                TermStatus {
                    running: true,
                    title: shell_name(&info.shell),
                    ..TermStatus::default()
                },
            ),
            Err(error) => (
                None,
                TermStatus {
                    error: format!("Cannot start a shell: {}", error_text(&error)),
                    ..TermStatus::default()
                },
            ),
        };
        view::term_status(&self.window, status.clone());
        self.terminal = Some(Terminal {
            id,
            emu: Emulator::new(cols, rows),
            shown: Vec::new(),
            status,
            dirty: true,
        });
        self.schedule_frame();
    }
    async fn term_kill(&mut self, ws: &Workspace) {
        if let Some(id) = self.terminal.as_mut().and_then(|t| t.id.take()) {
            let _ = ws.terminal_close(id).await;
        }
    }

    fn term_key(&mut self, text: &str, ctrl: bool, alt: bool) {
        let Some(terminal) = &mut self.terminal else {
            return;
        };
        let (Some(id), Some(tx)) = (&terminal.id, &self.term_tx) else {
            return;
        };
        if let Some(bytes) = term::key_bytes(text, ctrl, alt, terminal.emu.app_cursor()) {
            terminal.emu.scroll_to_bottom();
            terminal.dirty = true;
            let _ = tx.send(TermOp::Write(id.clone(), bytes));
            self.schedule_frame();
        }
    }

    fn schedule_frame(&mut self) {
        if self.frame_due.is_none() {
            self.frame_due = Some(Instant::now() + FRAME);
        }
    }

    /// Repaints the dirty terminal rows and the streaming search hits (at most once per frame).
    fn flush(&mut self) {
        self.frame_due = None;
        if let Some(terminal) = &mut self.terminal {
            if terminal.dirty {
                terminal.dirty = false;
                let screen = terminal.emu.screen();
                let full = terminal.shown.len() != screen.lines.len();
                let changed = screen
                    .lines
                    .iter()
                    .enumerate()
                    .filter(|(i, line)| full || terminal.shown.get(*i) != Some(*line))
                    .map(|(i, line)| (i, line.clone()))
                    .collect();
                let rows = screen.lines.len();
                terminal.shown = screen.lines;
                let cursor = screen.cursor.filter(|_| terminal.status.running);
                view::term_frame(
                    &self.window,
                    TermFrame {
                        rows,
                        changed,
                        cursor,
                    },
                );
            }
        }
        if self.search_dirty {
            self.search_dirty = false;
            view::search(
                &self.window,
                self.hits.clone(),
                true,
                self.search_summary(None),
                String::new(),
            );
        }
    }

    // ---- results and events -----------------------------------------------------------------------------------

    fn done(&mut self, done: Done) {
        match done {
            Done::Git(seq, result) if seq == self.git_seq => match result {
                Ok(status) => self.apply_git(status),
                Err(error) => view::git_result(&self.window, error_text(&error), true, false),
            },
            Done::Action {
                result,
                clear_message,
            } => {
                match result {
                    Ok(text) => view::git_result(&self.window, text, false, clear_message),
                    Err(text) => view::git_result(&self.window, text, true, false),
                }
                self.git_refresh(false);
                self.refresh_git_diff();
            }
            Done::Changed => {
                self.git_refresh(false);
                self.refresh_git_diff();
            }
            Done::Diff(seq, source, result) if seq == self.diff_seq => {
                self.diff_source = Some(source);
                self.diff_current = 0;
                match result {
                    Ok(files) => {
                        self.diff_files = files;
                        self.render_diff();
                    }
                    Err(reason) => {
                        self.diff_files.clear();
                        let mut out = DiffView {
                            source: if matches!(self.diff_source, Some(DiffSource::Turn { .. })) {
                                "turn"
                            } else {
                                "git"
                            },
                            title: "Turn changes".into(),
                            notice: reason,
                            ..DiffView::default()
                        };
                        if let Some(DiffSource::Git { path, .. }) = &self.diff_source {
                            out.title.clone_from(path);
                        }
                        view::diff(&self.window, out);
                    }
                }
            }
            Done::Files(seq, result) if seq == self.search_seq => match result {
                Ok(hits) => {
                    self.hits = hits;
                    view::search(
                        &self.window,
                        self.hits.clone(),
                        false,
                        self.search_summary(None),
                        String::new(),
                    );
                }
                Err(error) => view::search(
                    &self.window,
                    Vec::new(),
                    false,
                    String::new(),
                    error_text(&error),
                ),
            },
            Done::Text(seq, result) if seq == self.search_seq => {
                self.search_id = None;
                self.search_dirty = false;
                match result {
                    Ok((hits, matches, files, truncated)) => {
                        self.hits = hits;
                        self.hits.truncate(MAX_HITS);
                        view::search(
                            &self.window,
                            self.hits.clone(),
                            false,
                            self.search_summary(Some((matches, files, truncated))),
                            String::new(),
                        );
                    }
                    Err(error) => view::search(
                        &self.window,
                        self.hits.clone(),
                        false,
                        String::new(),
                        error_text(&error),
                    ),
                }
            }
            Done::Preview(seq, path, result) if seq == self.preview_seq => match result {
                Ok(lines) => view::preview(&self.window, path, lines),
                Err(text) => view::preview(&self.window, path, vec![(0, text, false)]),
            },
            // Stale result of a superseded request.
            _ => {}
        }
    }

    fn event(&mut self, kind: WorkspaceEventKind) {
        match kind {
            WorkspaceEventKind::TerminalOutput { id, data } => {
                let Some(terminal) = &mut self.terminal else {
                    return;
                };
                if terminal.id.as_deref() != Some(id.as_str()) {
                    return;
                }
                let replies = terminal.emu.feed(&data);
                terminal.dirty = true;
                if let Some(tx) = &self.term_tx {
                    for reply in replies {
                        let _ = tx.send(TermOp::Write(id.clone(), reply));
                    }
                }
                self.schedule_frame();
            }
            WorkspaceEventKind::TerminalExited { id, code } => {
                let Some(terminal) = &mut self.terminal else {
                    return;
                };
                if terminal.id.as_deref() != Some(id.as_str()) {
                    return;
                }
                terminal.id = None;
                terminal.dirty = true;
                terminal.status.running = false;
                terminal.status.exited = true;
                terminal.status.exit_code = code;
                view::term_status(&self.window, terminal.status.clone());
                self.schedule_frame();
            }
            WorkspaceEventKind::FilesChanged { watch_id, .. } => {
                if self.watch.as_deref() == Some(watch_id.as_str()) {
                    self.git_due = Some(Instant::now() + GIT_DEBOUNCE);
                }
            }
            WorkspaceEventKind::WatchClosed { watch_id, .. } => {
                if self.watch.as_deref() == Some(watch_id.as_str()) {
                    self.watch = None;
                }
            }
            WorkspaceEventKind::SearchProgress { search_id, matches } => {
                if self.search_id.as_deref() != Some(search_id.as_str())
                    || self.hits.len() >= MAX_HITS
                {
                    return;
                }
                let room = MAX_HITS - self.hits.len();
                self.hits
                    .extend(matches.into_iter().take(room).map(|m| Hit {
                        path: m.path,
                        line: m.line,
                        column: m.column,
                        text: m.text,
                        is_dir: false,
                    }));
                self.search_dirty = true;
                self.schedule_frame();
            }
        }
    }

    async fn shutdown(&mut self) {
        self.term_tx = None;
        if let Some(ws) = self.ws.take() {
            if let Err(error) = ws.shutdown().await {
                eprintln!("harness-slint: panels shutdown: {error}");
            }
        }
    }
}

/// `C:\…\pwsh.exe` → `pwsh`.
fn shell_name(shell: &str) -> String {
    let name = shell.rsplit(['\\', '/']).next().unwrap_or(shell);
    name.strip_suffix(".exe").unwrap_or(name).to_owned()
}
