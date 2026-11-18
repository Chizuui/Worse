//! Settings › Memory › Recall: the local memory shared by every agent and session
//! (`harness_agent_hub::recall`). The store is a small file; it is read and written on a worker thread and the
//! rows are set back on the UI thread.

use std::path::{Path, PathBuf};

use harness_agent_hub::recall::{self, Kind, Memory, NewMemory, Query, Store};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::ui::{AppWindow, RecallRow, RecallUi};

/// Rows shown at most (the search narrows a larger store).
const SHOWN_MAX: usize = 300;

#[derive(Clone)]
struct Ctx {
    home: PathBuf,
    /// [`recall::project_key`] of the workspace the app was opened in.
    project: String,
    window: slint::Weak<AppWindow>,
}

pub fn install(window: &AppWindow, home: &Path, workspace: &Path) {
    let ctx = Ctx {
        home: home.to_path_buf(),
        project: recall::project_key(&workspace.to_string_lossy()),
        window: window.as_weak(),
    };
    let ui = window.global::<RecallUi>();
    let config = Store::new(home).config();
    ui.set_auto_recall(config.recall);
    ui.set_auto_capture(config.capture);

    let c = ctx.clone();
    ui.on_refresh(move || reload(&c, None));

    let c = ctx.clone();
    ui.on_add(move || {
        let Some(window) = c.window.upgrade() else {
            return;
        };
        let ui = window.global::<RecallUi>();
        let new = NewMemory {
            kind: Kind::parse(&ui.get_draft_kind()),
            text: ui.get_draft().to_string(),
            project: (!ui.get_draft_global()).then(|| c.project.clone()),
            ..NewMemory::default()
        };
        let c = c.clone();
        run(move || {
            let note = match Store::new(&c.home).add(new) {
                Ok(recall::Added::New(_)) => (true, "Saved.".to_owned()),
                Ok(recall::Added::Duplicate(_)) => (true, "Already remembered.".to_owned()),
                Err(message) => (false, capitalized(&message)),
            };
            reload(&c, Some(note));
        });
    });

    let c = ctx.clone();
    ui.on_forget(move |id| {
        let c = c.clone();
        run(move || {
            let note = match Store::new(&c.home).forget(&id) {
                Ok(_) => "Forgotten.".to_owned(),
                Err(error) => format!("Could not write the memory file: {error}"),
            };
            reload(&c, Some((false, note)));
        });
    });

    let c = ctx.clone();
    ui.on_pin(move |id, pinned| {
        let c = c.clone();
        run(move || {
            let note = Store::new(&c.home)
                .set_pinned(&id, pinned)
                .err()
                .map(|error| (false, format!("Could not write the memory file: {error}")));
            reload(&c, note);
        });
    });

    let c = ctx.clone();
    ui.on_set_auto_recall(move |on| set_config(&c, move |config| config.recall = on));
    let c = ctx;
    ui.on_set_auto_capture(move |on| set_config(&c, move |config| config.capture = on));
}

fn run(work: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new()
        .name("harness-recall".into())
        .spawn(work);
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

fn set_config(ctx: &Ctx, change: impl FnOnce(&mut recall::Config) + Send + 'static) {
    let ctx = ctx.clone();
    run(move || {
        let store = Store::new(&ctx.home);
        let mut config = store.config();
        change(&mut config);
        let failed = store.set_config(config).err();
        let config = store.config();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(window) = ctx.window.upgrade() else {
                return;
            };
            let ui = window.global::<RecallUi>();
            ui.set_auto_recall(config.recall);
            ui.set_auto_capture(config.capture);
            if let Some(error) = failed {
                ui.set_note(format!("Could not save the setting: {error}").into());
            }
        });
    });
}

/// Loads the store and shows it, searched and filtered as the page asks. `note`: a status line, and whether
/// the draft was used up.
fn reload(ctx: &Ctx, note: Option<(bool, String)>) {
    let Some(window) = ctx.window.upgrade() else {
        // Called from a worker thread: go through the event loop to read the query.
        return reload_later(ctx.clone(), note);
    };
    let ui = window.global::<RecallUi>();
    let query = ui.get_query().to_string();
    let filter = ui.get_filter().to_string();
    let ctx = ctx.clone();
    run(move || show(&ctx, &query, &filter, note));
}

/// `slint::Weak::upgrade` only works on the UI thread; a worker asks the UI thread to start the reload.
fn reload_later(ctx: Ctx, note: Option<(bool, String)>) {
    let _ = slint::invoke_from_event_loop(move || {
        if ctx.window.upgrade().is_some() {
            reload(&ctx, note);
        }
    });
}

fn show(ctx: &Ctx, query: &str, filter: &str, note: Option<(bool, String)>) {
    let all = Store::new(&ctx.home).load();
    let total = all.len();
    let wanted = |m: &&Memory| match filter {
        "memories" => m.kind != Kind::Episode,
        "sessions" => m.kind == Kind::Episode,
        _ => true,
    };
    let refs: Vec<&Memory> = all.iter().filter(wanted).collect();
    let now = recall::now_ms();
    let shown: Vec<&Memory> = if query.trim().is_empty() {
        let mut newest = refs;
        newest.sort_by_key(|m| (std::cmp::Reverse(m.pinned), std::cmp::Reverse(m.at)));
        newest.into_iter().take(SHOWN_MAX).collect()
    } else {
        recall::search(
            &refs,
            &Query {
                text: query,
                limit: SHOWN_MAX,
                need: 1,
                kind: None,
            },
        )
        .into_iter()
        .map(|hit| hit.memory)
        .collect()
    };
    let rows: Vec<(String, String, String, String, bool)> = shown
        .into_iter()
        .map(|m| {
            (
                m.id.clone(),
                m.kind.as_str().to_owned(),
                row_text(m),
                meta(m, now, &ctx.project),
                m.pinned,
            )
        })
        .collect();
    let weak = ctx.window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let ui = window.global::<RecallUi>();
        let rows: Vec<RecallRow> = rows
            .into_iter()
            .map(|(id, kind, text, meta, pinned)| RecallRow {
                id: id.into(),
                kind: kind.into(),
                text: text.into(),
                meta: meta.into(),
                pinned,
            })
            .collect();
        ui.set_rows(ModelRc::new(VecModel::from(rows)));
        ui.set_total(i32::try_from(total).unwrap_or(i32::MAX));
        if let Some((used, text)) = note {
            if used {
                ui.set_draft(SharedString::new());
            }
            ui.set_note(text.into());
        }
    });
}

/// A session digest leads with its title, unless the digest already starts with the same words (an untitled
/// session is named after its first request).
fn row_text(memory: &Memory) -> String {
    match (&memory.title, memory.kind) {
        (Some(title), Kind::Episode) if !memory.text.contains(title.trim_end_matches('…')) => {
            format!("{title}\n{}", memory.text)
        }
        _ => memory.text.clone(),
    }
}

/// "3d ago · Claude Code · worse": when, who, which project.
fn meta(memory: &Memory, now: i64, here: &str) -> String {
    let who = memory.agent.as_deref().unwrap_or("you");
    let place = match memory.project.as_deref() {
        None => "everywhere".to_owned(),
        Some(project) if project == here => "this project".to_owned(),
        Some(project) => project.rsplit('/').next().unwrap_or(project).to_owned(),
    };
    format!("{} · {who} · {place}", recall::age(now, memory.at))
}

#[cfg(test)]
mod tests {
    use super::{capitalized, meta};
    use harness_agent_hub::recall::{Kind, Memory};

    #[test]
    fn meta_names_time_author_and_place() {
        let mut memory = Memory {
            id: "m1".into(),
            at: 0,
            kind: Kind::Fact,
            text: "x".into(),
            project: Some("/work/worse".into()),
            pinned: false,
            session: None,
            agent: Some("Codex".into()),
            title: None,
            key: None,
            replaces: None,
            expires: None,
        };
        let hour = 60 * 60 * 1000;
        assert_eq!(
            meta(&memory, 3 * hour, "/work/worse"),
            "3h ago · Codex · this project"
        );
        assert_eq!(
            meta(&memory, 3 * hour, "/work/other"),
            "3h ago · Codex · worse"
        );
        memory.project = None;
        memory.agent = None;
        assert_eq!(
            meta(&memory, 0, "/work/worse"),
            "just now · you · everywhere"
        );
        assert_eq!(capitalized("text is empty"), "Text is empty");
    }
}
