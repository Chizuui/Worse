use std::cell::RefCell;
use std::collections::BTreeSet;

use harness_core_api::{ApiError, ImportCandidate, ImportReport, ImportRequest, ImportSource};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

use crate::core::{ChatCall, Command};
use crate::ui::{AppWindow, ImportRow, ImportUi};

#[derive(Default)]
struct State {
    window: Option<Weak<AppWindow>>,
    candidates: Vec<ImportCandidate>,
    selected: BTreeSet<String>,
    filter: String,
    query: String,
    busy: bool,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn sv(text: &str) -> SharedString {
    SharedString::from(text)
}

fn window() -> Option<AppWindow> {
    STATE.with(|s| s.borrow().window.as_ref().and_then(Weak::upgrade))
}

fn with_ui(f: impl FnOnce(ImportUi<'_>)) {
    if let Some(w) = window() {
        f(w.global::<ImportUi>());
    }
}

fn key_of(candidate: &ImportCandidate) -> String {
    format!("{}:{}", candidate.source.as_str(), candidate.external_id)
}

fn source_label(source: ImportSource) -> &'static str {
    match source {
        ImportSource::ClaudeCode => "Claude Code",
        ImportSource::Codex => "Codex",
        ImportSource::Zcode => "ZCode",
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn ago(ms: i64) -> String {
    if ms <= 0 {
        return "unknown date".into();
    }
    let secs = (now_ms() - ms).max(0) / 1000;
    if secs < 3600 {
        format!("{} min ago", (secs / 60).max(1))
    } else if secs < 86_400 {
        format!("{} h ago", secs / 3600)
    } else {
        format!("{} d ago", secs / 86_400)
    }
}

fn folder_name(path: &str) -> &str {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
}

fn matches(candidate: &ImportCandidate, filter: &str, query: &str) -> bool {
    if filter != "all" && !filter.is_empty() && candidate.source.as_str() != filter {
        return false;
    }
    if query.is_empty() {
        return true;
    }
    candidate.title.to_lowercase().contains(query)
        || candidate.external_id.to_lowercase().contains(query)
        || candidate.workspace_path.to_lowercase().contains(query)
}

/// Keys of the importable sessions the table shows (source filter + search), without duplicates.
fn visible_importable(s: &State) -> BTreeSet<String> {
    let query = s.query.trim().to_lowercase();
    s.candidates
        .iter()
        .filter(|c| !c.imported && matches(c, &s.filter, &query))
        .map(key_of)
        .collect()
}

fn render() {
    let Some(w) = window() else {
        return;
    };
    let ui = w.global::<ImportUi>();
    STATE.with(|cell| {
        let s = cell.borrow();
        let query = s.query.trim().to_lowercase();
        let rows: Vec<ImportRow> = s
            .candidates
            .iter()
            .filter(|c| matches(c, &s.filter, &query))
            .map(|c| {
                let key = key_of(c);
                let folder = if c.workspace_path.is_empty() {
                    "no folder"
                } else {
                    folder_name(&c.workspace_path)
                };
                ImportRow {
                    selected: s.selected.contains(&key),
                    key: sv(&key),
                    title: sv(&c.title),
                    meta: sv(&format!(
                        "{} · {folder} · {} {} · {}",
                        source_label(c.source),
                        c.turn_count,
                        if c.turn_count == 1 { "turn" } else { "turns" },
                        ago(c.updated_at)
                    )),
                    source: sv(source_label(c.source)),
                    imported: c.imported,
                    project: sv(folder),
                    turns: i32::try_from(c.turn_count).unwrap_or(i32::MAX),
                    active: sv(&ago(c.updated_at)),
                }
            })
            .collect();
        let count = |source: ImportSource| {
            i32::try_from(s.candidates.iter().filter(|c| c.source == source).count())
                .unwrap_or(i32::MAX)
        };
        ui.set_count_all(i32::try_from(s.candidates.len()).unwrap_or(i32::MAX));
        ui.set_count_claude(count(ImportSource::ClaudeCode));
        ui.set_count_codex(count(ImportSource::Codex));
        ui.set_count_zcode(count(ImportSource::Zcode));
        ui.set_selected(i32::try_from(s.selected.len()).unwrap_or(i32::MAX));
        let turns: u64 = s
            .candidates
            .iter()
            .filter(|c| s.selected.contains(&key_of(c)))
            .map(|c| u64::from(c.turn_count))
            .sum();
        ui.set_selected_turns(i32::try_from(turns).unwrap_or(i32::MAX));
        ui.set_imported_count(
            i32::try_from(s.candidates.iter().filter(|c| c.imported).count()).unwrap_or(i32::MAX),
        );
        // The switch reflects the rows on screen: on when every visible importable session is selected.
        let visible = visible_importable(&s);
        ui.set_all_selected(!visible.is_empty() && visible.is_subset(&s.selected));
        ui.set_filter(sv(if s.filter.is_empty() {
            "all"
        } else {
            &s.filter
        }));
        ui.set_rows(ModelRc::from(std::rc::Rc::new(VecModel::from(rows))));
    });
}

fn set_busy(text: &str) {
    STATE.with(|s| s.borrow_mut().busy = !text.is_empty());
    with_ui(|ui| ui.set_busy(sv(text)));
}

fn set_status(text: &str, error: bool) {
    with_ui(|ui| {
        ui.set_status(sv(text));
        ui.set_status_error(error);
    });
}

pub fn install(window: &AppWindow) {
    STATE.with(|s| s.borrow_mut().window = Some(window.as_weak()));
    let ui = window.global::<ImportUi>();
    ui.on_set_filter(|id| {
        STATE.with(|s| s.borrow_mut().filter = id.to_string());
        render();
    });
    ui.on_search(|q| {
        STATE.with(|s| s.borrow_mut().query = q.to_string());
        render();
    });
    ui.on_toggle(|key| {
        let key = key.to_string();
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            let importable = s.candidates.iter().any(|c| key_of(c) == key && !c.imported);
            if !s.selected.remove(&key) && importable {
                s.selected.insert(key);
            }
        });
        render();
    });
    ui.on_select_all(|on| {
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            // Acts on the visible rows only, so a search + Select all picks just the matches.
            let visible = visible_importable(&s);
            if on {
                s.selected.extend(visible);
            } else {
                s.selected.retain(|k| !visible.contains(k));
            }
        });
        render();
    });
    ui.on_clear_selection(|| {
        STATE.with(|s| s.borrow_mut().selected.clear());
        render();
    });
    ui.on_refresh(|| {
        set_status("", false);
        scan();
    });
    ui.on_import_selected(import_selected);
}

pub fn open() {
    set_status("", false);
    scan();
}

fn scan() {
    if STATE.with(|s| s.borrow().busy) {
        return;
    }
    set_busy("Looking for sessions…");
    let (reply, answer) = tokio::sync::oneshot::channel();
    crate::bridge::send(Command::Chat(ChatCall::ImportScan(
        harness_core_api::ImportScan::default(),
        reply,
    )));
    let _ = slint::spawn_local(async move {
        let result = answer.await.unwrap_or(Err(ApiError::ShutDown));
        set_busy("");
        match result {
            Ok(list) => {
                STATE.with(|s| {
                    let mut s = s.borrow_mut();
                    let keys: BTreeSet<String> =
                        list.iter().filter(|c| !c.imported).map(key_of).collect();
                    s.selected.retain(|k| keys.contains(k));
                    s.candidates = list;
                });
                render();
            }
            Err(error) => set_status(&error.to_string(), true),
        }
    });
}

fn import_selected() {
    if STATE.with(|s| s.borrow().busy) {
        return;
    }
    let requests: Vec<ImportRequest> = STATE.with(|s| {
        let s = s.borrow();
        [
            ImportSource::ClaudeCode,
            ImportSource::Codex,
            ImportSource::Zcode,
        ]
        .into_iter()
        .filter_map(|source| {
            let external_ids: Vec<String> = s
                .candidates
                .iter()
                .filter(|c| c.source == source && s.selected.contains(&key_of(c)))
                .map(|c| c.external_id.clone())
                .collect();
            (!external_ids.is_empty()).then_some(ImportRequest {
                source,
                external_ids,
            })
        })
        .collect()
    });
    if requests.is_empty() {
        return;
    }
    set_status("", false);
    set_busy("Importing…");
    let _ = slint::spawn_local(async move {
        let mut total = ImportReport::default();
        let mut failure = None;
        for request in requests {
            let (reply, answer) = tokio::sync::oneshot::channel();
            crate::bridge::send(Command::Chat(ChatCall::Import(request, reply)));
            match answer.await.unwrap_or(Err(ApiError::ShutDown)) {
                Ok(report) => {
                    total.imported.extend(report.imported);
                    total.skipped.extend(report.skipped);
                }
                Err(error) => {
                    failure = Some(error.to_string());
                    break;
                }
            }
        }
        STATE.with(|s| s.borrow_mut().selected.clear());
        set_busy("");
        let imported = match total.imported.len() {
            1 => "Imported 1 session.".to_owned(),
            n => format!("Imported {n} sessions."),
        };
        let text = match total.skipped.first() {
            Some((_, reason)) => format!("{imported} {} skipped ({reason}).", total.skipped.len()),
            None => imported,
        };
        match failure {
            Some(error) => set_status(&format!("{text} Import stopped: {error}"), true),
            None => set_status(&text, false),
        }
        scan();
    });
}
