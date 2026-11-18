//! Live code view (Side Pane "Live" tab) and the file / line-range facts of tool rows.
//!
//! While a turn runs, every tool call that reads or edits a file becomes the "latest" target. With follow on,
//! the Live view shows it: a window of the file around the lines the agent read or wrote, those lines marked.
//! The file is read from disk only while the Live tab is on screen (`LiveUi.active`); otherwise only the target
//! is remembered. Agents send an edit as one old/new pair (no keystrokes), so the view marks the written lines
//! once and lets the highlight settle; nothing is animated character by character.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use harness_core_api::{ToolContent, ToolLocation};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::ui::{AppWindow, LiveFile, LiveLine, LiveUi};

/// Lines shown above and below the focus line (the view is a window of the file, not the whole file).
const BEFORE: usize = 80;
const AFTER: usize = 200;
/// Larger files are not shown (the view says so).
const MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Files remembered in the turn trail.
const TRAIL: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Focus {
    /// The agent read lines `from..=to` (1-based); `to == 0` means "around `from`".
    Read { from: u32, to: u32 },
    /// The agent wrote `new` (replacing `old` when known); `line` is the agent's own location, if any.
    Edit {
        new: String,
        old: Option<String>,
        line: u32,
    },
    /// The user opened the file from a chat chip.
    Open { line: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    path: PathBuf,
    focus: Focus,
}

#[derive(Default)]
struct State {
    workspace: PathBuf,
    /// What the view shows.
    shown: Option<Target>,
    /// The agent's most recent target (shown again when follow is turned back on).
    latest: Option<Target>,
    trail: Vec<PathBuf>,
    epoch: i32,
    open_tick: i32,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::default();
}

/// Line range and file of a tool row, for the chip on the row ("L120–180" or "+3 −1").
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RowFile {
    pub path: String,
    pub line: i32,
    pub range: String,
}

/// File and line range a tool call touched, from its content and locations only (no disk access, so loading a
/// long session stays cheap).
pub(crate) fn row_file(
    kind: Option<&str>,
    content: &[ToolContent],
    locations: &[ToolLocation],
) -> RowFile {
    for item in content {
        if let ToolContent::Diff {
            path,
            old_text,
            new_text,
        } = item
        {
            let added = new_text.lines().count();
            let removed = old_text.as_deref().map_or(0, |o| o.lines().count());
            let line = locations
                .iter()
                .find(|l| l.path == *path)
                .and_then(|l| l.line)
                .unwrap_or(0);
            let range = if line > 0 && added > 0 {
                span(line, line + u32::try_from(added).unwrap_or(1) - 1)
            } else {
                format!("+{added} −{removed}")
            };
            return RowFile {
                path: path.clone(),
                line: i32::try_from(line).unwrap_or(0),
                range,
            };
        }
    }
    let Some(location) = locations.first() else {
        return RowFile::default();
    };
    let reading = kind.is_none_or(|k| k == "read");
    let (from, to) = read_span(content, location.line);
    let range = match (reading, from, to) {
        (true, f, t) if f > 0 && t >= f => span(f, t),
        (_, f, _) if f > 0 => format!("L{f}"),
        _ => String::new(),
    };
    RowFile {
        path: location.path.clone(),
        line: i32::try_from(from).unwrap_or(0),
        range,
    }
}

fn span(from: u32, to: u32) -> String {
    if to > from {
        format!("L{from}–{to}")
    } else {
        format!("L{from}")
    }
}

/// Lines a read covered: the numbered lines of its output (`   120→text` or `  120\ttext`), else the location
/// line and the number of output lines.
fn read_span(content: &[ToolContent], line: Option<u32>) -> (u32, u32) {
    let mut first = 0u32;
    let mut last = 0u32;
    let mut count = 0u32;
    for item in content {
        if let ToolContent::Text { text } = item {
            for row in text.lines() {
                count += 1;
                let trimmed = row.trim_start();
                let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
                if digits.is_empty() || digits.len() > 7 {
                    continue;
                }
                let rest = &trimmed[digits.len()..];
                if !(rest.starts_with('→') || rest.starts_with('\t')) {
                    continue;
                }
                if let Ok(n) = digits.parse::<u32>() {
                    if first == 0 {
                        first = n;
                    }
                    last = last.max(n);
                }
            }
        }
    }
    if first > 0 {
        return (first, last);
    }
    match line {
        Some(l) if l > 0 => (l, if count > 1 { l + count - 1 } else { l }),
        _ if count > 0 => (1, count),
        _ => (0, 0),
    }
}

/// Callbacks of the Live view. `workspace` resolves relative tool paths.
pub(crate) fn install(window: &AppWindow) {
    let live = window.global::<LiveUi>();
    let weak = window.as_weak();
    live.on_open(move |path, line| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let target = STATE.with(|cell| {
            let mut s = cell.borrow_mut();
            let path = resolve(&s.workspace, &path);
            let line = u32::try_from(line).unwrap_or(0);
            // The agent's own target for this file carries the exact lines; prefer it.
            let focus = match &s.latest {
                Some(t) if t.path == path => t.focus.clone(),
                _ => Focus::Open { line },
            };
            let target = Target { path, focus };
            s.shown = Some(target.clone());
            s.open_tick += 1;
            target
        });
        let tick = STATE.with(|cell| cell.borrow().open_tick);
        window.global::<LiveUi>().set_follow(false);
        window.global::<LiveUi>().set_view("code".into());
        render(&window, Some(&target));
        window.global::<LiveUi>().set_open_tick(tick);
    });
    let weak = window.as_weak();
    live.on_set_follow(move |follow| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        window.global::<LiveUi>().set_follow(follow);
        if follow {
            let latest = STATE.with(|cell| {
                let mut s = cell.borrow_mut();
                let latest = s.latest.clone();
                s.shown.clone_from(&latest);
                latest
            });
            render(&window, latest.as_ref());
        }
    });
    let weak = window.as_weak();
    live.on_refresh(move || {
        if let Some(window) = weak.upgrade() {
            let shown = STATE.with(|cell| cell.borrow().shown.clone());
            render(&window, shown.as_ref());
        }
    });
}

/// Brings the Live tab to the front without changing what it shows (the Screen view asks for this when the
/// agent starts using the computer).
pub(crate) fn request_open(window: &AppWindow) {
    let tick = STATE.with(|cell| {
        let mut s = cell.borrow_mut();
        s.open_tick += 1;
        s.open_tick
    });
    window.global::<LiveUi>().set_open_tick(tick);
}

/// The session's workspace changed (relative tool paths resolve against it).
pub(crate) fn set_workspace(workspace: &Path) {
    STATE.with(|cell| cell.borrow_mut().workspace = workspace.to_path_buf());
}

/// A new user turn: the trail starts over.
pub(crate) fn new_turn(window: &AppWindow) {
    STATE.with(|cell| cell.borrow_mut().trail.clear());
    window
        .global::<LiveUi>()
        .set_trail(ModelRc::new(VecModel::<LiveFile>::default()));
}

/// The session was left or switched: nothing is shown until the agent touches a file again.
pub(crate) fn clear(window: &AppWindow) {
    STATE.with(|cell| {
        let mut s = cell.borrow_mut();
        s.shown = None;
        s.latest = None;
        s.trail.clear();
    });
    render(window, None);
}

/// A running tool call reported content or locations: remember it as the latest target and, when following,
/// show it.
pub(crate) fn touch(
    window: &AppWindow,
    kind: Option<&str>,
    content: &[ToolContent],
    locations: &[ToolLocation],
) {
    let Some(target) =
        STATE.with(|cell| target_of(&cell.borrow().workspace, kind, content, locations))
    else {
        return;
    };
    let follow = window.global::<LiveUi>().get_follow();
    // A read reports twice (started, finished) with the same span and does not change the file: the
    // view already shows it.
    let same = STATE.with(|cell| {
        let mut s = cell.borrow_mut();
        let same = matches!(target.focus, Focus::Read { .. }) && s.shown.as_ref() == Some(&target);
        s.trail.retain(|p| *p != target.path);
        s.trail.push(target.path.clone());
        let extra = s.trail.len().saturating_sub(TRAIL);
        s.trail.drain(..extra);
        s.latest = Some(target.clone());
        if follow {
            s.shown = Some(target.clone());
        }
        same
    });
    if follow {
        // Following the agent: it is in a file now, not at the screen.
        window.global::<LiveUi>().set_view("code".into());
    }
    if follow && !same {
        render(window, Some(&target));
    } else {
        set_trail(window);
    }
}

fn target_of(
    workspace: &Path,
    kind: Option<&str>,
    content: &[ToolContent],
    locations: &[ToolLocation],
) -> Option<Target> {
    for item in content {
        if let ToolContent::Diff {
            path,
            old_text,
            new_text,
        } = item
        {
            let line = locations
                .iter()
                .find(|l| l.path == *path)
                .and_then(|l| l.line)
                .unwrap_or(0);
            return Some(Target {
                path: resolve(workspace, path),
                focus: Focus::Edit {
                    new: new_text.clone(),
                    old: old_text.clone(),
                    line,
                },
            });
        }
    }
    let location = locations.first()?;
    if kind.is_some_and(|k| !matches!(k, "read" | "edit" | "search")) {
        return None;
    }
    let (from, to) = read_span(content, location.line);
    Some(Target {
        path: resolve(workspace, &location.path),
        focus: Focus::Read { from, to },
    })
}

fn resolve(workspace: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() || workspace.as_os_str().is_empty() {
        p.to_path_buf()
    } else {
        workspace.join(p)
    }
}

fn display(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn set_trail(window: &AppWindow) {
    let (trail, shown) = STATE.with(|cell| {
        let s = cell.borrow();
        (s.trail.clone(), s.shown.as_ref().map(|t| t.path.clone()))
    });
    let rows: Vec<LiveFile> = trail
        .iter()
        .map(|p| LiveFile {
            path: p.to_string_lossy().as_ref().into(),
            name: p
                .file_name()
                .map_or_else(|| p.to_string_lossy(), |n| n.to_string_lossy())
                .as_ref()
                .into(),
            current: shown.as_ref() == Some(p),
        })
        .collect();
    window
        .global::<LiveUi>()
        .set_trail(ModelRc::new(VecModel::from(rows)));
}

/// Fills `LiveUi` for `target` (only while the Live tab is on screen; otherwise only the trail moves).
fn render(window: &AppWindow, target: Option<&Target>) {
    set_trail(window);
    let live = window.global::<LiveUi>();
    if !live.get_active() {
        return;
    }
    let Some(target) = target else {
        live.set_path(SharedString::new());
        live.set_name(SharedString::new());
        live.set_action(SharedString::new());
        live.set_note(SharedString::new());
        live.set_lines(ModelRc::new(VecModel::<LiveLine>::default()));
        return;
    };
    let workspace = STATE.with(|cell| cell.borrow().workspace.clone());
    let shown_path = display(&workspace, &target.path);
    let name = target
        .path
        .file_name()
        .map_or_else(|| shown_path.clone(), |n| n.to_string_lossy().into_owned());
    let view = build(&target.path, &target.focus);
    live.set_path(shown_path.as_str().into());
    live.set_name(name.as_str().into());
    live.set_action(view.action.into());
    live.set_note(view.note.as_str().into());
    live.set_cols(
        i32::try_from(view.lines.iter().map(|l| cells(&l.text)).max().unwrap_or(0)).unwrap_or(0),
    );
    live.set_lines(ModelRc::new(VecModel::from(view.lines)));
    live.set_focus(i32::try_from(view.focus).unwrap_or(0));
    let epoch = STATE.with(|cell| {
        let mut s = cell.borrow_mut();
        s.epoch = s.epoch.wrapping_add(1);
        s.epoch
    });
    live.set_epoch(epoch);
}

struct View {
    action: &'static str,
    note: String,
    lines: Vec<LiveLine>,
    /// Index into `lines` of the first marked line.
    focus: usize,
}

fn build(path: &Path, focus: &Focus) -> View {
    let text = match std::fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_BYTES => {
            return note_only("file too large to show");
        }
        Ok(_) => std::fs::read(path)
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned()),
        Err(_) => None,
    };
    let lang = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match (text, focus) {
        // Edit not on disk (yet): show the incoming lines on their own, marked "about to change".
        (None, Focus::Edit { new, .. }) => snippet(new, &lang, "file not on disk yet"),
        (None, _) => note_only("file not found"),
        (Some(text), focus) => {
            let file: Vec<&str> = text.lines().collect();
            let (action, from, to, note) = match focus {
                Focus::Read { from, to } => {
                    let to = if *to >= *from { *to } else { *from };
                    (
                        "read",
                        *from as usize,
                        to as usize,
                        lines_note(*from as usize, to as usize),
                    )
                }
                Focus::Open { line } => ("open", *line as usize, *line as usize, String::new()),
                Focus::Edit { new, old, line } => match find_block(&file, new) {
                    Some(start) => {
                        let end = start + new.lines().count().max(1) - 1;
                        ("edit", start, end, lines_note(start, end))
                    }
                    None => match old.as_deref().and_then(|o| find_block(&file, o)) {
                        Some(start) => {
                            let end =
                                start + old.as_deref().map_or(1, |o| o.lines().count().max(1)) - 1;
                            ("pending", start, end, lines_note(start, end))
                        }
                        None if *line > 0 => {
                            ("edit", *line as usize, *line as usize, String::new())
                        }
                        None => return snippet(new, &lang, "not found in the file"),
                    },
                },
            };
            window_view(&file, &lang, action, from, to, note)
        }
    }
}

/// Width of a line in character cells (wide East Asian characters take two).
fn cells(text: &str) -> usize {
    text.chars()
        .map(|c| if u32::from(c) >= 0x1100 { 2 } else { 1 })
        .sum()
}

fn lines_note(from: usize, to: usize) -> String {
    if from == 0 {
        String::new()
    } else if to > from {
        format!("lines {from}–{to}")
    } else {
        format!("line {from}")
    }
}

fn note_only(note: &str) -> View {
    View {
        action: "open",
        note: note.to_owned(),
        lines: Vec::new(),
        focus: 0,
    }
}

/// 1-based line where `block` first appears in `file`, all of its lines in order. A block of blank lines
/// only matches nothing (it would match almost anywhere).
fn find_block(file: &[&str], block: &str) -> Option<usize> {
    let want: Vec<&str> = block.lines().collect();
    if want.iter().all(|l| l.trim().is_empty()) {
        return None;
    }
    file.windows(want.len())
        .position(|w| w == want.as_slice())
        .map(|i| i + 1)
}

fn line_row(no: usize, text: &str, lang: &str, mark: i32) -> LiveLine {
    let shown = crate::markdown::expand_tabs(text);
    let light = crate::highlight::styled(&shown, lang, false);
    let dark = crate::highlight::styled(&shown, lang, true);
    let highlighted = light.is_some() && dark.is_some();
    LiveLine {
        no: i32::try_from(no).unwrap_or(0),
        text: shown.as_ref().into(),
        code_light: light.unwrap_or_default(),
        code_dark: dark.unwrap_or_default(),
        highlighted,
        mark,
    }
}

fn window_view(
    file: &[&str],
    lang: &str,
    action: &'static str,
    from: usize,
    to: usize,
    note: String,
) -> View {
    let from = from.clamp(1, file.len().max(1));
    let to = to.clamp(from, file.len().max(from));
    let start = from.saturating_sub(BEFORE).max(1);
    let end = (to + AFTER).min(file.len());
    let mark = match action {
        "edit" => 2,
        "pending" => 3,
        "read" => 1,
        _ => 0,
    };
    let lines = (start..=end)
        .map(|no| {
            let m = if (from..=to).contains(&no) && mark != 0 {
                mark
            } else {
                0
            };
            line_row(no, file.get(no - 1).copied().unwrap_or_default(), lang, m)
        })
        .collect();
    View {
        action,
        note,
        lines,
        focus: from - start,
    }
}

fn snippet(new: &str, lang: &str, note: &str) -> View {
    View {
        action: "pending",
        note: note.to_owned(),
        lines: new
            .lines()
            .enumerate()
            .map(|(i, l)| line_row(i + 1, l, lang, 3))
            .collect(),
        focus: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(t: &str) -> ToolContent {
        ToolContent::Text { text: t.to_owned() }
    }

    #[test]
    fn read_span_uses_numbered_output() {
        let out = text("   120→fn a() {\n   121→}\n   122→");
        assert_eq!(read_span(&[out], Some(120)), (120, 122));
        let tabs = text("    7\tlet x = 1;\n    8\tlet y = 2;");
        assert_eq!(read_span(&[tabs], None), (7, 8));
    }

    #[test]
    fn read_span_falls_back_to_location_and_line_count() {
        assert_eq!(read_span(&[text("a\nb\nc")], Some(40)), (40, 42));
        assert_eq!(read_span(&[text("a\nb")], None), (1, 2));
        assert_eq!(read_span(&[], None), (0, 0));
    }

    #[test]
    fn row_file_for_edit_and_read() {
        let diff = ToolContent::Diff {
            path: "src/lib.rs".into(),
            old_text: Some("a".into()),
            new_text: "b\nc".into(),
        };
        let loc = |line| ToolLocation {
            path: "src/lib.rs".into(),
            line,
        };
        assert_eq!(
            row_file(Some("edit"), &[diff.clone()], &[loc(Some(10))]).range,
            "L10–11"
        );
        assert_eq!(row_file(Some("edit"), &[diff], &[]).range, "+2 −1");
        let read = row_file(Some("read"), &[text("   5→x\n   6→y")], &[loc(None)]);
        assert_eq!(
            (read.path.as_str(), read.line, read.range.as_str()),
            ("src/lib.rs", 5, "L5–6")
        );
        assert_eq!(
            row_file(Some("execute"), &[text("ok")], &[]),
            RowFile::default()
        );
    }

    #[test]
    fn find_block_matches_whole_block() {
        let file = ["fn a() {", "    x();", "}", "fn b() {", "    x();", "}"];
        assert_eq!(find_block(&file, "fn b() {\n    x();"), Some(4));
        assert_eq!(find_block(&file, "    x();\n}"), Some(2));
        assert_eq!(find_block(&file, "missing"), None);
        assert_eq!(find_block(&file, "\n\n"), None);
    }

    #[test]
    fn window_marks_the_focus_lines() {
        let file: Vec<String> = (1..=500).map(|n| format!("line {n}")).collect();
        let refs: Vec<&str> = file.iter().map(String::as_str).collect();
        let view = window_view(&refs, "", "read", 200, 202, String::new());
        assert_eq!(view.lines.first().map(|l| l.no), Some(120));
        assert_eq!(view.lines.len(), 402 - 120 + 1);
        assert_eq!(view.lines[view.focus].no, 200);
        let marked: Vec<i32> = view
            .lines
            .iter()
            .filter(|l| l.mark == 1)
            .map(|l| l.no)
            .collect();
        assert_eq!(marked, vec![200, 201, 202]);
    }
}
