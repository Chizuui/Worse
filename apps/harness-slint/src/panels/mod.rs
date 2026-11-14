//! Side Pane panels (diff, terminal, git, search) — owned by agent A (round 8.2, spec phase-8-ui.md §10).
//! Contract with agent B: `.work/phase8/TOKENS_READY.md` ("Panel contract").
//!
//! Ownership: the Slint globals in `ui/panels/globals.slint` are written only by [`view`] (UI thread); every
//! value they show is computed by one `harness-panels` worker thread ([`worker`]), which owns the
//! `harness_workspace::Workspace` (git, search, PTY, watcher) and the terminal emulator. UI callbacks only send
//! [`Cmd`]s. The worker thread, the workspace and every child process start lazily on the first command, so a
//! closed Side Pane costs no thread, process or index (spec §10 lazy start).

mod diffmodel;
mod term;
mod view;
mod worker;

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use harness_core_api::{EventKind, HarnessCore, RecvError, SessionId, TurnId};
use slint::ComponentHandle;
use tokio::sync::mpsc;

use crate::ui::{AppWindow, PanelDiff, PanelGit, PanelSearch, PanelTerminal};

/// Reply channel of a [`GitCall`]; a dropped receiver (picker closed) is not an error.
pub type GitReply<T> = tokio::sync::oneshot::Sender<Result<T, harness_core_api::ApiError>>;

/// Branch switcher, commit-all, push and commit graph calls (phase-9-parity.md §9.2.5 R19), run by the panels
/// worker (the single owner of the workspace) and answered on their own channel. After `Switch`, `Commit` and
/// `Push` the worker refreshes the Git panel itself.
// B 的 9.2 UI 构造这些调用；在其接入前不算死代码错误。
#[derive(Debug)]
pub enum GitCall {
    Branches(GitReply<harness_core_api::workspace::GitBranches>),
    /// `SwitchOutcome::Blocked { files }` when local changes would be overwritten (nothing changed).
    Switch {
        branch: String,
        create: bool,
        reply: GitReply<harness_core_api::workspace::SwitchOutcome>,
    },
    /// `include_unstaged` stages every change of the workspace folder first.
    Commit {
        message: String,
        include_unstaged: bool,
        reply: GitReply<harness_core_api::workspace::CommitResult>,
    },
    /// `None` = current branch / its upstream's remote (else `origin`); sets the upstream when missing.
    Push {
        remote: Option<String>,
        branch: Option<String>,
        progress: Option<mpsc::UnboundedSender<harness_core_api::workspace::PushProgress>>,
        reply: GitReply<harness_core_api::workspace::PushResult>,
    },
    /// Commit graph page; `limit <= harness_workspace::LOG_PAGE_MAX`, `skip + limit <= LOG_DEPTH_MAX`.
    Log {
        skip: u32,
        limit: u32,
        reply: GitReply<harness_core_api::workspace::GitLogPage>,
    },
}

/// Runs a git call on the panels worker (starts it lazily).
pub fn git(call: GitCall) {
    send(Cmd::Git(call));
}

/// Requests from the UI (and the hub recorder) to the panels worker.
#[derive(Debug)]
pub(crate) enum Cmd {
    GitRefresh,
    Git(GitCall),
    Stage(String),
    Unstage(String),
    StageAll,
    UnstageAll,
    Commit(String),
    OpenDiff {
        path: String,
        staged: bool,
    },
    DiffSelect(usize),
    ShowTurn,
    DiffRefresh,
    Search {
        query: String,
        files: bool,
        regex: bool,
        case_sensitive: bool,
    },
    SearchOpen(usize),
    SearchClosePreview,
    TermKey {
        text: String,
        ctrl: bool,
        alt: bool,
    },
    TermResize {
        cols: u16,
        rows: u16,
    },
    TermScroll(i32),
    TermRestart,
    TermClose,
    /// A turn finished (from the hub recorder); refreshes a shown turn diff.
    TurnFinished,
    Shutdown,
}

/// Hub-side facts the worker reads when it shows a turn diff (written by the recorder and `session_opened`).
#[derive(Default)]
pub(crate) struct HubLink {
    pub core: Option<Arc<dyn HarnessCore>>,
    /// Session shown in the chat (from `session_opened`), preferred for "Turn changes".
    pub active_session: Option<SessionId>,
    /// Last finished turn per run of the app: (session, turn).
    pub last_turn: Option<(SessionId, TurnId)>,
}

#[derive(Default)]
struct State {
    window: Option<slint::Weak<AppWindow>>,
    tx: Option<mpsc::UnboundedSender<Cmd>>,
    thread: Option<JoinHandle<()>>,
    shut_down: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    window: None,
    tx: None,
    thread: None,
    shut_down: false,
});

static HUB: Mutex<Option<HubLink>> = Mutex::new(None);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs `f` on the hub link (created on first use).
pub(crate) fn with_hub<R>(f: impl FnOnce(&mut HubLink) -> R) -> R {
    let mut hub = lock(&HUB);
    f(hub.get_or_insert_with(HubLink::default))
}

/// Sends `cmd` to the worker, starting it on first use. Ignored after `teardown`.
fn send(cmd: Cmd) {
    let mut state = lock(&STATE);
    if state.shut_down {
        return;
    }
    if state.tx.is_none() {
        let Some(window) = state.window.clone() else {
            return;
        };
        let (tx, rx) = mpsc::unbounded_channel();
        match std::thread::Builder::new()
            .name("harness-panels".into())
            .spawn(move || worker::run(window, rx))
        {
            Ok(handle) => {
                state.thread = Some(handle);
                state.tx = Some(tx);
            }
            Err(error) => {
                eprintln!("harness-slint: cannot start the panels thread: {error}");
                return;
            }
        }
    }
    if let Some(tx) = &state.tx {
        let _ = tx.send(cmd);
    }
}

/// Sends `cmd` only when the worker already runs (hub notifications must not start it).
fn send_if_running(cmd: Cmd) {
    let state = lock(&STATE);
    if let Some(tx) = &state.tx {
        let _ = tx.send(cmd);
    }
}

/// Called once on the UI thread after the window and `bridge::install`; wires panel globals/callbacks.
pub fn install(window: &AppWindow) {
    lock(&STATE).window = Some(window.as_weak());

    let git = window.global::<PanelGit>();
    git.on_refresh(|| send(Cmd::GitRefresh));
    git.on_stage(|path| send(Cmd::Stage(path.into())));
    git.on_unstage(|path| send(Cmd::Unstage(path.into())));
    git.on_stage_all(|| send(Cmd::StageAll));
    git.on_unstage_all(|| send(Cmd::UnstageAll));
    let weak = window.as_weak();
    git.on_commit(move || {
        let message = weak
            .upgrade()
            .map(|w| w.global::<PanelGit>().get_message().to_string())
            .unwrap_or_default();
        send(Cmd::Commit(message));
    });
    git.on_open_diff(|path, staged| {
        send(Cmd::OpenDiff {
            path: path.into(),
            staged,
        });
    });

    let diff = window.global::<PanelDiff>();
    diff.on_select_file(|index| send(Cmd::DiffSelect(usize::try_from(index).unwrap_or(0))));
    diff.on_show_turn(|| send(Cmd::ShowTurn));
    diff.on_refresh(|| send(Cmd::DiffRefresh));

    let search = window.global::<PanelSearch>();
    let weak = window.as_weak();
    search.on_run(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let search = window.global::<PanelSearch>();
        send(Cmd::Search {
            query: search.get_query().to_string(),
            files: search.get_mode() != "content",
            regex: search.get_regex(),
            case_sensitive: search.get_case_sensitive(),
        });
    });
    search.on_open(|index| send(Cmd::SearchOpen(usize::try_from(index).unwrap_or(0))));
    search.on_close_preview(|| send(Cmd::SearchClosePreview));

    let terminal = window.global::<PanelTerminal>();
    terminal.on_key(|text, ctrl, alt| {
        send(Cmd::TermKey {
            text: text.into(),
            ctrl,
            alt,
        });
    });
    terminal.on_resized(|cols, rows| {
        let clamp = |v: i32| u16::try_from(v.clamp(2, 1000)).unwrap_or(2);
        send(Cmd::TermResize {
            cols: clamp(cols),
            rows: clamp(rows),
        });
    });
    terminal.on_scroll(|lines| send(Cmd::TermScroll(lines)));
    terminal.on_restart(|| send(Cmd::TermRestart));
    terminal.on_close(|| send(Cmd::TermClose));
}

/// Hook for agent B (`core/hub.rs`, after `Hub::start`): gives the panels the hub for turn diffs. Must be
/// called inside B's tokio runtime: it spawns a recorder there that remembers the last finished turn.
#[allow(dead_code)] // Wired by agent B (ui-requests.md); until then the Diff panel reports "no hub".
pub fn attach_core(core: Arc<dyn HarnessCore>) {
    let mut events = core.subscribe();
    with_hub(|hub| hub.core = Some(core));
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    if let (EventKind::TurnFinished { turn_id, .. }, Some(session)) =
                        (&event.kind, &event.session_id)
                    {
                        let entry = (session.clone(), turn_id.clone());
                        with_hub(|hub| hub.last_turn = Some(entry));
                        send_if_running(Cmd::TurnFinished);
                    }
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
    });
}

/// Hook for agent B: the chat now shows `session` (its last finished turn becomes the "Turn changes" target).
#[allow(dead_code)] // Wired by agent B (ui-requests.md).
pub fn session_opened(session: SessionId) {
    with_hub(|hub| hub.active_session = Some(session));
}

/// Stops the worker (kills terminals, stops the watcher, cancels searches) and joins it. Call after the event
/// loop returned; idempotent.
pub fn teardown() {
    trace("teardown: begin");
    let thread = {
        let mut state = lock(&STATE);
        state.shut_down = true;
        if let Some(tx) = state.tx.take() {
            let _ = tx.send(Cmd::Shutdown);
        }
        state.thread.take()
    };
    if let Some(thread) = thread {
        if thread.join().is_err() {
            eprintln!("harness-slint: panels thread panicked");
        }
    }
    trace("teardown: joined");
}

/// Shutdown diagnostics on stderr when `HARNESS_PANELS_TRACE=1` (gates).
pub(crate) fn trace(what: &str) {
    if std::env::var_os("HARNESS_PANELS_TRACE").is_some_and(|v| v == "1") {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        eprintln!("harness-panels: {ms} {what}");
    }
}
