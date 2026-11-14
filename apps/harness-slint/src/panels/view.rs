//! UI-thread side of the panels: the only writer of the panel globals. The worker sends plain (`Send`) data;
//! conversion to Slint models happens here inside `upgrade_in_event_loop`.

use std::rc::Rc;

use slint::{Color, ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};

use super::diffmodel::{Pair, Row};
use super::term;
use crate::ui::{
    AppWindow, DiffFileRow, DiffPair, DiffRow, GitRow, PanelDiff, PanelGit, PanelSearch,
    PanelTerminal, PreviewLine, SearchHit, TermLine, TermRun,
};

fn s(text: &str) -> SharedString {
    SharedString::from(text)
}

fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::from(Rc::new(VecModel::from(items)))
}

fn int(value: u64) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Runs `f` on the UI thread with the window (dropped silently once the window is gone).
fn on_ui(window: &Weak<AppWindow>, f: impl FnOnce(&AppWindow) + Send + 'static) {
    let _ = window.upgrade_in_event_loop(move |w| f(&w));
}

// ---- Git --------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub(crate) struct GitFile {
    pub path: String,
    pub code: &'static str,
    pub kind: &'static str,
    pub added: u64,
    pub removed: u64,
    pub staged: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct GitView {
    pub is_repo: bool,
    pub branch: String,
    pub sync: String,
    pub staged: Vec<GitFile>,
    pub unstaged: Vec<GitFile>,
    pub branches: Vec<String>,
}

fn git_row(file: &GitFile) -> GitRow {
    let (dir, name) = file
        .path
        .rsplit_once('/')
        .map_or(("", file.path.as_str()), |(d, n)| (d, n));
    GitRow {
        path: s(&file.path),
        name: s(name),
        dir: s(dir),
        code: s(file.code),
        kind: s(file.kind),
        added: int(file.added),
        removed: int(file.removed),
        staged: file.staged,
    }
}

pub(crate) fn git_loading(window: &Weak<AppWindow>, loading: bool) {
    on_ui(window, move |w| w.global::<PanelGit>().set_loading(loading));
}

pub(crate) fn git_status(window: &Weak<AppWindow>, view: GitView) {
    on_ui(window, move |w| {
        let git = w.global::<PanelGit>();
        git.set_is_repo(view.is_repo);
        git.set_branch(s(&view.branch));
        git.set_sync(s(&view.sync));
        git.set_staged(model(view.staged.iter().map(git_row).collect()));
        git.set_unstaged(model(view.unstaged.iter().map(git_row).collect()));
        let rows: Vec<_> = view
            .staged
            .iter()
            .chain(view.unstaged.iter())
            .map(git_row)
            .collect();
        git.set_total_added(rows.iter().map(|r| r.added).sum());
        git.set_total_removed(rows.iter().map(|r| r.removed).sum());
        git.set_branches(model(view.branches.iter().map(|b| s(b)).collect()));
        git.set_error(SharedString::new());
        git.set_loading(false);
        git.set_loaded(true);
    });
}

/// Result line of an action (`error` = false) or an error; `clear_message` empties the commit box.
pub(crate) fn git_result(window: &Weak<AppWindow>, text: String, error: bool, clear_message: bool) {
    on_ui(window, move |w| {
        let git = w.global::<PanelGit>();
        if error {
            git.set_error(s(&text));
            git.set_notice(SharedString::new());
        } else {
            git.set_notice(s(&text));
            git.set_error(SharedString::new());
        }
        if clear_message {
            git.set_message(SharedString::new());
        }
        git.set_loading(false);
        git.set_loaded(true);
    });
}

// ---- Diff -------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub(crate) struct DiffView {
    pub source: &'static str,
    pub title: String,
    pub subtitle: String,
    pub added: u64,
    pub removed: u64,
    pub rows: Vec<Row>,
    pub pairs: Vec<Pair>,
    /// (path, status, added, removed) — shown as a file list when there is more than one file.
    pub files: Vec<(String, &'static str, u64, u64)>,
    pub current_file: usize,
    pub notice: String,
}

pub(crate) fn diff_loading(window: &Weak<AppWindow>) {
    on_ui(window, |w| w.global::<PanelDiff>().set_loading(true));
}

pub(crate) fn diff(window: &Weak<AppWindow>, view: DiffView) {
    on_ui(window, move |w| {
        let diff = w.global::<PanelDiff>();
        diff.set_source(s(view.source));
        diff.set_title(s(&view.title));
        diff.set_subtitle(s(&view.subtitle));
        diff.set_added(int(view.added));
        diff.set_removed(int(view.removed));
        diff.set_rows(model(
            view.rows
                .iter()
                .map(|r| DiffRow {
                    kind: r.kind,
                    old_no: s(&r.old_no),
                    new_no: s(&r.new_no),
                    text: s(&r.text),
                })
                .collect(),
        ));
        diff.set_pairs(model(
            view.pairs
                .iter()
                .map(|p| DiffPair {
                    hunk: p.hunk,
                    header: s(&p.header),
                    left_kind: p.left_kind,
                    left_no: s(&p.left_no),
                    left_text: s(&p.left_text),
                    right_kind: p.right_kind,
                    right_no: s(&p.right_no),
                    right_text: s(&p.right_text),
                })
                .collect(),
        ));
        diff.set_files(model(
            view.files
                .iter()
                .map(|(path, status, added, removed)| DiffFileRow {
                    path: s(path),
                    status: s(status),
                    added: int(*added),
                    removed: int(*removed),
                })
                .collect(),
        ));
        diff.set_current_file(i32::try_from(view.current_file).unwrap_or(0));
        diff.set_notice(s(&view.notice));
        diff.set_loading(false);
    });
}

// ---- Search -----------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub(crate) struct Hit {
    pub path: String,
    pub line: u64,
    pub column: u64,
    pub text: String,
    pub is_dir: bool,
}

fn search_hit(hit: &Hit) -> SearchHit {
    let (dir, name) = hit
        .path
        .rsplit_once('/')
        .map_or(("", hit.path.as_str()), |(d, n)| (d, n));
    SearchHit {
        path: s(&hit.path),
        name: s(name),
        dir: s(dir),
        line: int(hit.line),
        column: int(hit.column),
        text: s(hit.text.trim()),
        is_dir: hit.is_dir,
    }
}

pub(crate) fn search(
    window: &Weak<AppWindow>,
    hits: Vec<Hit>,
    busy: bool,
    summary: String,
    error: String,
) {
    on_ui(window, move |w| {
        let search = w.global::<PanelSearch>();
        search.set_hits(model(hits.iter().map(search_hit).collect()));
        search.set_busy(busy);
        search.set_summary(s(&summary));
        search.set_error(s(&error));
    });
}

/// Read-only preview: `(line number, text, is the hit line)`; empty `path` closes it.
pub(crate) fn preview(window: &Weak<AppWindow>, path: String, lines: Vec<(u64, String, bool)>) {
    on_ui(window, move |w| {
        let search = w.global::<PanelSearch>();
        search.set_preview(model(
            lines
                .iter()
                .map(|(no, text, hit)| PreviewLine {
                    no: int(*no),
                    text: s(text),
                    hit: *hit,
                })
                .collect(),
        ));
        search.set_preview_path(s(&path));
    });
}

// ---- Terminal ---------------------------------------------------------------------------------------------------

/// Changed rows of the terminal screen since the last frame.
#[derive(Debug, Default)]
pub(crate) struct TermFrame {
    pub rows: usize,
    pub changed: Vec<(usize, term::Line)>,
    pub cursor: Option<(usize, usize)>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TermStatus {
    pub running: bool,
    pub exited: bool,
    pub exit_code: i64,
    pub title: String,
    pub error: String,
}

fn rgb((r, g, b): (u8, u8, u8)) -> Color {
    Color::from_rgb_u8(r, g, b)
}

fn term_line(line: &term::Line) -> TermLine {
    TermLine {
        text: s(&line.text),
        runs: model(
            line.runs
                .iter()
                .map(|run| TermRun {
                    col: i32::from(run.col),
                    cells: i32::from(run.cells),
                    text: s(&run.text),
                    fg: run.fg,
                    fg_rgb: rgb(run.fg_rgb),
                    bg: run.bg,
                    bg_rgb: rgb(run.bg_rgb),
                    bold: run.bold,
                    italic: run.italic,
                    underline: run.underline,
                })
                .collect(),
        ),
    }
}

pub(crate) fn term_frame(window: &Weak<AppWindow>, frame: TermFrame) {
    on_ui(window, move |w| {
        let terminal = w.global::<PanelTerminal>();
        let current = terminal.get_lines();
        // Same row count: patch only the changed rows of the existing model (no full rebuild per frame).
        let patched = current
            .as_any()
            .downcast_ref::<VecModel<TermLine>>()
            .filter(|m| m.row_count() == frame.rows)
            .map(|m| {
                for (index, line) in &frame.changed {
                    m.set_row_data(*index, term_line(line));
                }
            })
            .is_some();
        if !patched {
            let mut lines = vec![TermLine::default(); frame.rows];
            for (index, line) in &frame.changed {
                if let Some(slot) = lines.get_mut(*index) {
                    *slot = term_line(line);
                }
            }
            terminal.set_lines(model(lines));
        }
        match frame.cursor {
            Some((row, col)) => {
                terminal.set_cursor_row(i32::try_from(row).unwrap_or(0));
                terminal.set_cursor_col(i32::try_from(col).unwrap_or(0));
                terminal.set_cursor_visible(true);
            }
            None => terminal.set_cursor_visible(false),
        }
    });
}

pub(crate) fn term_status(window: &Weak<AppWindow>, status: TermStatus) {
    on_ui(window, move |w| {
        let terminal = w.global::<PanelTerminal>();
        terminal.set_running(status.running);
        terminal.set_exited(status.exited);
        terminal.set_exit_code(i32::try_from(status.exit_code).unwrap_or(-1));
        terminal.set_title(s(&status.title));
        terminal.set_error(s(&status.error));
        if !status.running {
            terminal.set_cursor_visible(false);
        }
    });
}
