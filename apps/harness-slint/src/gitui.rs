//! Git operations of 9.2B round 3 (spec phase-9-parity.md §7.4 "Git"; inventory GIT-007..GIT-011): branch
//! switcher, commit dialog, push dialog and commit graph (`ui/panels/gitops.slint`).
//!
//! Ownership: the panels worker owns the repository and answers each [`GitCall`] on its own oneshot; it refreshes
//! the Git panel (`PanelGit`) itself after Switch / Commit / Push. This module is the only writer of `GitUi`; the
//! graph rows and the lane state live here on the UI thread and are dropped when the dialog closes.
//!
//! ```text
//! GitUi callback ──▶ gitui::ask ──GitCall{…, reply}──▶ panels worker (owns Workspace, git CLI)
//!                        │                                 │ Switch/Commit/Push → refresh PanelGit itself
//!   spawn_local(await) ◀─┴──────────── reply (oneshot) ◀───┘
//!        └─ generation check (dialog reopened? drop the stale page) → write GitUi
//! push progress: worker ──PushProgress (unbounded)──▶ spawn_local loop ──▶ GitUi.result-kind = "pushing"
//! ```

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::rc::Rc;

use harness_core_api::workspace::{
    CommitResult, GitBranches, GitLogPage, GitLogRow, PushProgress, PushResult, SwitchOutcome,
};
use harness_core_api::ApiError;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};

use crate::panels::{git, GitCall, GitReply};
use crate::ui::{AppWindow, BranchRow, ChatUi, GitUi, GraphRow, PanelGit};

/// Commits per graph page ("Load more" fetches the next one).
const PAGE: u32 = 100;
const LANE_W: u32 = 14;
const ROW_H: u32 = 28;
const MID: u32 = ROW_H / 2;
/// Line ends one pixel beyond the row (the Slint viewbox starts at -1) so rows join without a seam.
const TOP: i32 = -1;
const BOTTOM: u32 = ROW_H + 1;

#[derive(Default)]
struct State {
    window: Option<Weak<AppWindow>>,
    /// Bumped on every open / close; a reply of an older generation is dropped.
    generation: u64,
    rows: Vec<GraphRow>,
    skip: u32,
    /// Lane → hash of the commit the line in that lane runs down to (carried across pages).
    open_lanes: BTreeMap<u32, String>,
    /// Full text of the last error (Copy error).
    error: String,
    /// Every branch of the last `Branches` reply; `GitUi.branches` shows those matching the filter.
    branches: Vec<BranchRow>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn sv(text: &str) -> SharedString {
    SharedString::from(text)
}

fn int(value: u64) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn window() -> Option<AppWindow> {
    STATE.with(|s| s.borrow().window.as_ref().and_then(Weak::upgrade))
}

fn generation() -> u64 {
    STATE.with(|s| s.borrow().generation)
}

/// Sends a git call; `done` runs on the UI thread with the reply unless the dialog was reopened or closed since.
fn ask<T: 'static>(
    make: impl FnOnce(GitReply<T>) -> GitCall,
    done: impl FnOnce(&AppWindow, Result<T, ApiError>) + 'static,
) {
    let asked = generation();
    let (reply, answer) = tokio::sync::oneshot::channel();
    git(make(reply));
    let _ = slint::spawn_local(async move {
        let result = answer.await.unwrap_or(Err(ApiError::ShutDown));
        if generation() != asked {
            return;
        }
        if let Some(w) = window() {
            done(&w, result);
        }
    });
}

fn set_result(ui: &GitUi<'_>, kind: &str, a: &str, b: &str) {
    ui.set_result_kind(sv(kind));
    ui.set_result_a(sv(a));
    ui.set_result_b(sv(b));
}

fn fail(w: &AppWindow, error: &ApiError) {
    let text = error.to_string();
    let ui = w.global::<GitUi>();
    ui.set_busy(false);
    ui.set_pushing(false);
    set_result(&ui, "error", &text, "");
    STATE.with(|s| s.borrow_mut().error = text);
}

fn open(w: &AppWindow, which: &str) {
    let ui = w.global::<GitUi>();
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.generation += 1;
        // 关闭或切换对话框时释放提交图数据（spec：大数据按需加载、关闭即释放）。
        s.rows = Vec::new();
        s.skip = 0;
        s.open_lanes.clear();
    });
    ui.set_graph(ModelRc::default());
    ui.set_graph_more(false);
    ui.set_graph_selected(-1);
    ui.set_busy(false);
    // 旧一代的回复会被丢弃，进行中的推送状态也随之复位，否则 Push 按钮会一直禁用。
    ui.set_pushing(false);
    ui.set_dialog(sv(which));
    if which.is_empty() {
        ui.set_branches(ModelRc::default());
        STATE.with(|s| s.borrow_mut().branches = Vec::new());
        return;
    }
    set_result(&ui, "", "", "");
    match which {
        "branches" => {
            ui.set_filter(SharedString::new());
            ui.set_blocked(SharedString::new());
            ui.set_blocked_branch(SharedString::new());
            load_branches();
        }
        "push" => load_branches(),
        "commit" => ui.set_include_unstaged(true),
        "graph" => load_graph(),
        _ => {}
    }
}

fn load_branches() {
    if let Some(w) = window() {
        w.global::<GitUi>().set_busy(true);
    }
    ask(
        GitCall::Branches,
        |w, result: Result<GitBranches, ApiError>| {
            let b = match result {
                Ok(b) => b,
                Err(e) => return fail(w, &e),
            };
            let ui = w.global::<GitUi>();
            let rows: Vec<BranchRow> = b
                .branches
                .iter()
                .map(|r| BranchRow {
                    name: sv(&r.name),
                    current: r.current,
                    upstream: sv(r.upstream.as_deref().unwrap_or_default()),
                })
                .collect();
            STATE.with(|s| s.borrow_mut().branches = rows);
            show_branches(w);
            ui.set_current(sv(b.current.as_deref().unwrap_or_default()));
            ui.set_upstream(sv(b.upstream.as_deref().unwrap_or_default()));
            ui.set_ahead(int(b.ahead));
            ui.set_behind(int(b.behind));
            ui.set_busy(false);
        },
    );
}

/// Shows the branches whose name contains the filter (case-insensitive).
fn show_branches(w: &AppWindow) {
    let ui = w.global::<GitUi>();
    let filter = ui.get_filter().to_lowercase();
    let rows: Vec<BranchRow> = STATE.with(|s| {
        s.borrow()
            .branches
            .iter()
            .filter(|b| filter.is_empty() || b.name.to_lowercase().contains(&filter))
            .cloned()
            .collect()
    });
    ui.set_branches(ModelRc::from(Rc::new(VecModel::from(rows))));
}

fn switch_branch(w: &AppWindow, branch: &str, create: bool) {
    let branch = branch.trim().to_string();
    if branch.is_empty() {
        return;
    }
    // 输入的分支名已存在时按切换处理，不再尝试创建（git 会报 "already exists"）。
    let exists = STATE.with(|s| {
        s.borrow()
            .branches
            .iter()
            .any(|b| b.name.as_str() == branch)
    });

    let ui = w.global::<GitUi>();
    ui.set_busy(true);
    ui.set_blocked(SharedString::new());
    let asked = branch.clone();
    ask(
        |reply| GitCall::Switch {
            branch,
            create: create && !exists,
            reply,
        },
        move |w, result: Result<SwitchOutcome, ApiError>| {
            let ui = w.global::<GitUi>();
            ui.set_busy(false);
            match result {
                Ok(SwitchOutcome::Switched { branch }) => {
                    open(w, "");
                    set_result(&w.global::<GitUi>(), "switched", &branch, "");
                }
                Ok(SwitchOutcome::Blocked { files }) => {
                    ui.set_blocked(sv(&files.join("\n")));
                    ui.set_blocked_branch(sv(&asked));
                }
                Err(e) => fail(w, &e),
            }
        },
    );
}

fn commit(w: &AppWindow, push_after: bool) {
    let message = w.global::<PanelGit>().get_message().trim().to_string();
    if message.is_empty() {
        return;
    }
    let ui = w.global::<GitUi>();
    let include_unstaged = ui.get_include_unstaged();
    let changed = commit_paths(w, include_unstaged);
    let gate = crate::security::verdict(&changed);
    if matches!(gate, crate::security::gate::Verdict::Block(_)) {
        let text = gate.notice().unwrap_or_default();
        return fail(w, &ApiError::Conflict(text));
    }
    // `warn` and `skipped` both commit, and both say why in the result line.
    let warned = gate.notice();
    w.global::<PanelGit>().set_notice(SharedString::new());
    ui.set_busy(true);
    set_result(&ui, "", "", "");
    ask(
        |reply| GitCall::Commit {
            message,
            include_unstaged,
            reply,
        },
        move |w, result: Result<CommitResult, ApiError>| {
            let r = match result {
                Ok(r) => r,
                Err(e) => return fail(w, &e),
            };
            w.global::<PanelGit>().set_message(SharedString::new());
            let short: String = r.hash.chars().take(7).collect();
            open(w, if push_after { "push" } else { "" });
            set_result(&w.global::<GitUi>(), "committed", &short, &r.summary);
            if let Some(text) = &warned {
                w.global::<PanelGit>().set_notice(sv(text));
            }
            if push_after {
                push(w);
            }
        },
    );
}

/// Paths the commit covers: the staged rows, plus the unstaged ones when `include_unstaged` is on.
fn commit_paths(w: &AppWindow, include_unstaged: bool) -> Vec<String> {
    let git = w.global::<PanelGit>();
    let mut paths: Vec<String> = git
        .get_staged()
        .iter()
        .map(|row| row.path.to_string())
        .collect();
    if include_unstaged {
        paths.extend(git.get_unstaged().iter().map(|row| row.path.to_string()));
    }
    paths
}

fn push(w: &AppWindow) {
    let ui = w.global::<GitUi>();
    if ui.get_pushing() {
        return;
    }
    ui.set_pushing(true);
    let (progress, mut phases) = tokio::sync::mpsc::unbounded_channel();
    let asked = generation();
    let _ = slint::spawn_local(async move {
        while let Some(phase) = phases.recv().await {
            if generation() != asked {
                continue;
            }
            if let (PushProgress::Started { remote, branch }, Some(w)) = (phase, window()) {
                set_result(&w.global::<GitUi>(), "pushing", &branch, &remote);
            }
        }
    });
    ask(
        |reply| GitCall::Push {
            remote: None,
            branch: None,
            progress: Some(progress),
            reply,
        },
        |w, result: Result<PushResult, ApiError>| {
            let r = match result {
                Ok(r) => r,
                Err(e) => return fail(w, &e),
            };
            let ui = w.global::<GitUi>();
            ui.set_pushing(false);
            ui.set_upstream(sv(&format!("{}/{}", r.remote, r.branch)));
            ui.set_ahead(int(r.ahead));
            ui.set_behind(int(r.behind));
            let kind = if r.set_upstream {
                "pushed-upstream"
            } else {
                "pushed"
            };
            set_result(&ui, kind, &r.branch, &r.remote);
        },
    );
}

fn load_graph() {
    let skip = STATE.with(|s| s.borrow().skip);
    let limit = PAGE.min(harness_workspace::LOG_DEPTH_MAX.saturating_sub(skip));
    if limit == 0 {
        return;
    }
    if let Some(w) = window() {
        w.global::<GitUi>().set_busy(true);
    }
    ask(
        |reply| GitCall::Log { skip, limit, reply },
        move |w, result: Result<GitLogPage, ApiError>| {
            let page = match result {
                Ok(page) => page,
                Err(e) => return fail(w, &e),
            };
            let ui = w.global::<GitUi>();
            let (rows, more) = STATE.with(|s| {
                let mut s = s.borrow_mut();
                let fetched = u32::try_from(page.rows.len()).unwrap_or(u32::MAX);
                for row in &page.rows {
                    let graph_row = graph_row(&mut s.open_lanes, row);
                    s.rows.push(graph_row);
                }
                s.skip = s.skip.saturating_add(fetched);
                let more = page.has_more && s.skip < harness_workspace::LOG_DEPTH_MAX;
                (s.rows.clone(), more)
            });
            ui.set_graph(ModelRc::from(Rc::new(VecModel::from(rows))));
            ui.set_graph_more(more);
            ui.set_busy(false);
        },
    );
}

/// One graph row: lines from the lanes that reach this commit, pass-through lines, and lines to each parent's lane.
fn graph_row(open: &mut BTreeMap<u32, String>, row: &GitLogRow) -> GraphRow {
    let x = |lane: u32| lane * LANE_W + LANE_W / 2;
    let dot = x(row.lane);
    let mut path = String::new();
    let mut width = row.lanes.max(row.lane + 1);
    for (lane, hash) in open.iter() {
        width = width.max(lane + 1);
        let lx = x(*lane);
        if *hash == row.hash {
            let _ = write!(path, "M {lx} {TOP} L {dot} {MID} ");
        } else {
            let _ = write!(path, "M {lx} {TOP} L {lx} {BOTTOM} ");
        }
    }
    open.retain(|_, hash| *hash != row.hash);
    for (i, parent) in row.parents.iter().enumerate() {
        let lane = row.parent_lanes.get(i).copied().unwrap_or(row.lane);
        width = width.max(lane + 1);
        let _ = write!(path, "M {dot} {MID} L {} {BOTTOM} ", x(lane));
        open.insert(lane, parent.clone());
    }
    let short = |h: &str| h.chars().take(7).collect::<String>();
    GraphRow {
        hash: sv(&row.hash),
        short: sv(&short(&row.hash)),
        subject: sv(&row.subject),
        author: sv(&row.author),
        date: sv(&crate::settings::date_of(row.time.saturating_mul(1000))),
        refs: sv(&row.refs.join(", ")),
        parents: sv(&row
            .parents
            .iter()
            .map(|p| short(p))
            .collect::<Vec<_>>()
            .join(" ")),
        lane: i32::try_from(row.lane).unwrap_or(0),
        lanes: i32::try_from(width).unwrap_or(1),
        path: sv(path.trim_end()),
    }
}

/// Connects the `GitUi` callbacks (UI thread).
pub fn install(app: &AppWindow) {
    STATE.with(|s| s.borrow_mut().window = Some(app.as_weak()));
    let ui = app.global::<GitUi>();
    ui.on_filter_edited(|_| {
        if let Some(w) = window() {
            show_branches(&w);
        }
    });
    ui.on_open(|which| {
        if let Some(w) = window() {
            open(&w, &which);
        }
    });
    ui.on_switch_branch(|branch, create| {
        if let Some(w) = window() {
            switch_branch(&w, &branch, create);
        }
    });
    ui.on_commit(|push_after| {
        if let Some(w) = window() {
            commit(&w, push_after);
        }
    });
    ui.on_push(|| {
        if let Some(w) = window() {
            push(&w);
        }
    });
    ui.on_load_more(load_graph);
    ui.on_copy_result(|| {
        if let Some(w) = window() {
            let text = STATE.with(|s| s.borrow().error.clone());
            w.global::<ChatUi>().invoke_copy(sv(&text));
        }
    });
    ui.on_copy_hash(|hash| {
        if let Some(w) = window() {
            w.global::<ChatUi>().invoke_copy(hash);
        }
    });
}
