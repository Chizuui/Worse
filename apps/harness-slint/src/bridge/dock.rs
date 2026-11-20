//! Composer dock of 9.2B round 3 (spec `docs/specs/harness/phase-9-parity.md` §7.4 "UI contract for 9.2B round 3"):
//! queue edit / reorder / steer (R12), mode / model / effort pickers and skill / subagent mentions (R13) and the typed
//! error banner (R20).
//!
//! The hub owns the queue, the per-session choices and the failure. This module only mirrors
//! `Event::Queue` / `Event::SessionOptions` / `Event::Failure` into `DockUi` and turns clicks into commands; the
//! UI-local state is the queued message being edited and the retry countdown.
//!
//! ```text
//! click ─▶ DockUi callback ─▶ dock.rs ─▶ Command::{EditQueued, ReorderQueued, RunQueuedNow, Steer, Set*, RetryInit}
//!                                              │ core thread (hub session actor = owner)
//! DockUi ◀── dock.rs ◀── Event::{Queue, SessionOptions, Failure} ◀──────────────┘ (replace semantics)
//! ```

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use harness_core_api::{
    ContextItem, FailureKind, MentionKind, MentionQuery, ResumableRun, RetryHint,
};

use super::{with_state, UiState};
use crate::core::{ChatCall, Command, QueuedMessage, SessionId};
use crate::ui::{ChatUi, DockUi, OptionRow};

/// UI-local dock state.
#[derive(Default)]
pub(super) struct DockState {
    /// Queued message whose text is in the composer; Send then sends `EditQueued`.
    editing: Option<String>,
    /// Text Copy puts on the clipboard (kind title is added by the banner reader).
    failure_copy: String,
    /// Session of the shown failure (`None` = the hub failed to start).
    failure_session: Option<SessionId>,
    /// Retry countdown of an `After` hint.
    countdown: Option<slint::Timer>,
    /// Config option id behind the effort picker (`SessionOptions.effort_id`).
    effort_id: Option<String>,
    /// Config option id behind the model picker (`SessionOptions.model_id`; `None` = `session/set_model`).
    model_id: Option<String>,
    /// Sessions in deep research mode: each composer message gets `ContextItem::Research`.
    research: std::collections::HashSet<SessionId>,
    /// Mode picker text without the research suffix.
    mode_label: String,
    /// Deep research chosen before the first message of a new chat.
    draft_research: bool,
}

/// Picker values of one `Event::SessionOptions`.
pub struct Options {
    pub mode: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub effort_id: Option<String>,
    pub model_id: Option<String>,
    pub modes: Vec<(String, String)>,
    pub models: Vec<(String, String)>,
    pub efforts: Vec<(String, String)>,
    pub pending: bool,
    pub locked: bool,
    pub machine: bool,
}

/// One `Event::Failure`.
pub struct FailureEvent {
    pub session: Option<SessionId>,
    pub kind: FailureKind,
    pub message: String,
    pub details: Option<String>,
    pub trace: Option<String>,
    pub retry: RetryHint,
}

fn sv(text: &str) -> SharedString {
    SharedString::from(text)
}

fn option_rows(list: &[(String, String)]) -> ModelRc<OptionRow> {
    ModelRc::new(VecModel::from(
        list.iter()
            .map(|(id, name)| OptionRow {
                id: sv(id),
                name: sv(if name.is_empty() { id } else { name }),
            })
            .collect::<Vec<_>>(),
    ))
}

/// The models whose name or id contains every word of `query` (case-insensitive); all of them for an empty
/// query.
fn matching_models(list: &[(String, String)], query: &str) -> Vec<(String, String)> {
    let query = query.to_lowercase();
    let words: Vec<&str> = query.split_whitespace().collect();
    list.iter()
        .filter(|(id, name)| {
            let text = format!("{name} {id}").to_lowercase();
            words.iter().all(|word| text.contains(word))
        })
        .cloned()
        .collect()
}

/// Display name of the chosen value; the id when the agent did not list it, "" when nothing is chosen.
fn chosen_name(list: &[(String, String)], value: Option<&str>) -> String {
    let Some(value) = value else {
        return list
            .first()
            .map(|(_, name)| name.clone())
            .unwrap_or_default();
    };
    list.iter()
        .find(|(id, _)| id == value)
        .map_or_else(|| value.to_string(), |(_, name)| name.clone())
}

fn kind_id(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Auth => "auth",
        FailureKind::ProcessExited => "processExited",
        FailureKind::InitFailed => "initFailed",
        FailureKind::Transient => "transient",
        FailureKind::Other => "other",
    }
}

impl UiState {
    fn dock(&self) -> DockUi<'_> {
        self.window.global::<DockUi>()
    }

    /// A session was opened or left: no edit in progress, no pickers until its options arrive, no failure.
    pub(super) fn reset_dock(&mut self) {
        self.end_edit();
        self.apply_options(None);
        self.dismiss_failure();
        self.load_skills();
        self.load_resumable();
    }

    // ---- queue (R12) ---------------------------------------------------------------------------------

    fn end_edit(&mut self) {
        self.dock_state.editing = None;
        self.dock().set_editing_id(SharedString::new());
    }

    /// `Event::Queue` of the open session arrived (after `set_queue`): an edited message that left the queue
    /// (it started meanwhile) ends the edit mode; the draft stays so nothing typed is lost.
    pub(super) fn queue_changed(&mut self, items: &[QueuedMessage]) {
        if let Some(id) = &self.dock_state.editing {
            if !items.iter().any(|q| &q.id == id) {
                self.end_edit();
            }
        }
    }

    fn queue_action(&mut self, action: &str, id: &str) {
        let Some(session) = self.active.clone() else {
            return;
        };
        match action {
            "steer" => self.send(Command::RunQueuedNow {
                session,
                id: id.to_string(),
            }),
            "edit" => {
                let draft = self.window.get_draft();
                let chips = self.window.global::<ChatUi>().get_chips().row_count();
                if !draft.trim().is_empty() || chips > 0 {
                    let text = self
                        .window
                        .global::<ChatUi>()
                        .invoke_action_label(sv("editDraftConflict"));
                    self.window.set_error_text(text);
                    return;
                }
                let Some(text) = (0..self.queue.row_count())
                    .filter_map(|i| self.queue.row_data(i))
                    .find(|q| q.id == id)
                    .map(|q| q.text.to_string())
                else {
                    return;
                };
                self.dock_state.editing = Some(id.to_string());
                self.dock().set_editing_id(sv(id));
                crate::chat::restore_composer(&text, Vec::new());
            }
            "cancel-edit" => {
                self.end_edit();
                crate::chat::restore_composer("", Vec::new());
            }
            "remove" => self.send(Command::CancelQueued {
                session,
                id: id.to_string(),
            }),
            _ => {}
        }
    }

    /// Grip drag / accessible increment: moves `id` by `delta` rows and sends the whole order once.
    fn queue_move(&self, id: &str, delta: i32) {
        let Some(session) = self.active.clone() else {
            return;
        };
        let mut ids: Vec<String> = (0..self.queue.row_count())
            .filter_map(|i| self.queue.row_data(i))
            .map(|q| q.id.to_string())
            .collect();
        let Some(from) = ids.iter().position(|q| q == id) else {
            return;
        };
        let last = i64::try_from(ids.len()).unwrap_or(i64::MAX) - 1;
        let to = (i64::try_from(from).unwrap_or_default() + i64::from(delta)).clamp(0, last.max(0));
        let to = usize::try_from(to).unwrap_or_default();
        if to == from {
            return;
        }
        let moved = ids.remove(from);
        ids.insert(to, moved);
        self.send(Command::ReorderQueued { session, ids });
    }

    /// Send while editing a queued message: replaces its text (`EditQueued`). Returns false when not editing.
    pub(super) fn send_edit(&mut self, text: String) -> bool {
        let Some(id) = self.dock_state.editing.clone() else {
            return false;
        };
        self.end_edit();
        if let Some(session) = self.active.clone() {
            self.send(Command::EditQueued { session, id, text });
        }
        true
    }

    fn steer(&mut self) {
        let Some(session) = self.active.clone() else {
            return;
        };
        let draft = self.window.get_draft().to_string();
        let (text, mut context) = crate::chat::prepare_send(&draft);
        if text.trim().is_empty() && context.is_empty() {
            return;
        }
        self.add_research(&session, &mut context);
        self.window.set_draft(SharedString::new());
        self.send(Command::Steer {
            session,
            agent: self.sticky.clone(),
            text,
            context,
        });
    }

    // ---- pickers (R13) -------------------------------------------------------------------------------

    /// `Event::SessionOptions` of the open session (`None` clears the pickers).
    pub(super) fn apply_options(&mut self, options: Option<Options>) {
        let dock = self.dock();
        let Some(o) = options else {
            for set in [
                DockUi::set_modes,
                DockUi::set_models,
                DockUi::set_models_shown,
                DockUi::set_efforts,
            ] {
                set(&dock, ModelRc::default());
            }
            dock.set_model_query(SharedString::new());
            dock.set_picker_open(SharedString::new());
            dock.set_machine(false);
            crate::machine_view::set_session(&self.window, None, false, "", "");
            dock.set_research(self.active.is_none() && self.dock_state.draft_research);
            self.dock_state.effort_id = None;
            self.dock_state.model_id = None;
            return;
        };
        dock.set_modes(option_rows(&o.modes));
        dock.set_models(option_rows(&o.models));
        // The menu's list is the full list narrowed by what is typed in its search box.
        let query = dock.get_model_query().to_string();
        dock.set_models_shown(option_rows(&matching_models(&o.models, &query)));
        dock.set_efforts(option_rows(&o.efforts));
        dock.set_mode(sv(o.mode.as_deref().unwrap_or_default()));
        dock.set_model(sv(o.model.as_deref().unwrap_or_default()));
        dock.set_effort(sv(o.effort.as_deref().unwrap_or_default()));
        dock.set_machine(o.machine);
        crate::machine_view::set_session(
            &self.window,
            self.active.as_deref(),
            o.machine,
            &self.sticky,
            &self.agent_name(&self.sticky),
        );
        let mode_label = if o.machine {
            "Machine".to_owned()
        } else {
            chosen_name(&o.modes, o.mode.as_deref())
        };
        dock.set_model_name(sv(&chosen_name(&o.models, o.model.as_deref())));
        dock.set_effort_name(sv(&chosen_name(&o.efforts, o.effort.as_deref())));
        dock.set_options_locked(o.locked);
        dock.set_options_pending(o.pending);
        if o.locked {
            dock.set_picker_open(SharedString::new());
        }
        self.dock_state.effort_id = o.effort_id;
        self.dock_state.model_id = o.model_id;
        self.dock_state.mode_label = mode_label;
        self.show_research();
    }

    fn show_research(&self) {
        let on = self
            .active
            .as_ref()
            .map_or(self.dock_state.draft_research, |session| {
                self.dock_state.research.contains(session)
            });
        let dock = self.dock();
        dock.set_research(on);
        let label = &self.dock_state.mode_label;
        dock.set_mode_name(sv(&match (on, label.is_empty()) {
            (false, _) => label.clone(),
            (true, true) => "Deep research".to_owned(),
            (true, false) => format!("{label} · Research"),
        }));
    }

    pub(super) fn add_research(&mut self, session: &SessionId, context: &mut Vec<ContextItem>) {
        if context.contains(&ContextItem::Research) {
            self.dock_state.research.insert(session.clone());
            self.show_research();
        } else if self.dock_state.research.contains(session) {
            context.push(ContextItem::Research);
        }
    }

    pub(super) fn take_draft_research(&mut self, context: &mut Vec<ContextItem>) {
        if std::mem::take(&mut self.dock_state.draft_research) {
            context.push(ContextItem::Research);
        }
    }

    fn pick_option(&mut self, which: &str, id: &str) {
        if which == "mode" && id == "harness:research" {
            match self.active.clone() {
                Some(session) => {
                    if !self.dock_state.research.remove(&session) {
                        self.dock_state.research.insert(session);
                    }
                }
                None => self.dock_state.draft_research = !self.dock_state.draft_research,
            }
            self.show_research();
            return;
        }
        let Some(session) = self.active.clone() else {
            return;
        };
        let value = id.to_string();
        match which {
            "mode" if value == "harness:machine" => {
                self.send(Command::SetMachine { session, on: true });
            }
            "mode" => {
                if self.dock().get_machine() {
                    self.send(Command::SetMachine {
                        session: session.clone(),
                        on: false,
                    });
                }
                self.send(Command::SetMode {
                    session,
                    mode: value,
                });
            }
            "model" => match self.dock_state.model_id.clone() {
                Some(option) => self.send(Command::SetConfigOption {
                    session,
                    id: option,
                    value,
                }),
                None => self.send(Command::SetModel {
                    session,
                    model: value,
                }),
            },
            "effort" => {
                if let Some(option) = self.dock_state.effort_id.clone() {
                    self.send(Command::SetConfigOption {
                        session,
                        id: option,
                        value,
                    });
                }
            }
            _ => {}
        }
    }

    /// Skills and subagents of the open session's agent for the `@` popup (asked on open and on an agent change).
    pub(super) fn load_skills(&self) {
        let Some(session) = self.active.clone() else {
            crate::chat::agent_mentions(Vec::new());
            return;
        };
        let query = MentionQuery {
            session_id: harness_core_api::SessionId::new(session.as_str()),
            query: String::new(),
            kinds: vec![MentionKind::Skill, MentionKind::Subagent],
            limit: 50,
        };
        let asked = session;
        self.chat_call(
            |reply| ChatCall::Mentions(query, reply),
            move |s, list| {
                if s.is_active(&asked) {
                    crate::chat::agent_mentions(list);
                }
            },
        );
    }

    // ---- typed failure (R20) -------------------------------------------------------------------------

    pub(super) fn failure(&mut self, event: FailureEvent) {
        if let Some(session) = &event.session {
            if !self.is_active(session) {
                return;
            }
        }
        // 启动失败时 port 还会再发一条同文的 Event::Error；类型化横幅已展示它，普通横幅不再重复。
        if self.window.get_error_text().as_str() == event.message {
            self.window.set_error_text(SharedString::new());
        }
        let details = [event.details.as_deref(), event.trace.as_deref()]
            .into_iter()
            .flatten()
            .filter(|d| !d.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        let (retry, wait) = match event.retry {
            RetryHint::Retry => ("retry", 0),
            RetryHint::After { secs } => ("after", secs.unwrap_or_default()),
            RetryHint::Login => ("login", 0),
            RetryHint::No => ("no", 0),
        };
        let dock = self.dock();
        dock.set_failure_kind(sv(kind_id(event.kind)));
        dock.set_failure_message(sv(&event.message));
        dock.set_failure_details(sv(&details));
        dock.set_failure_retry(sv(retry));
        dock.set_failure_wait(i32::try_from(wait).unwrap_or(i32::MAX));
        dock.set_failure_expanded(false);
        dock.set_failure_shown(true);
        self.dock_state.failure_copy = if details.is_empty() {
            event.message
        } else {
            format!("{}\n\n{details}", event.message)
        };
        self.dock_state.failure_session = event.session;
        self.dock_state.countdown = (wait > 0).then(|| {
            let timer = slint::Timer::default();
            let weak = self.window.as_weak();
            timer.start(
                slint::TimerMode::Repeated,
                std::time::Duration::from_secs(1),
                move || {
                    if let Some(window) = weak.upgrade() {
                        let dock = window.global::<DockUi>();
                        dock.set_failure_wait((dock.get_failure_wait() - 1).max(0));
                    }
                },
            );
            timer
        });
    }

    /// True when `message` is the text of the shown typed failure (the port's duplicate `Event::Error`).
    pub(super) fn failure_repeats(&self, message: &str) -> bool {
        self.dock().get_failure_shown() && self.dock().get_failure_message().as_str() == message
    }

    pub(super) fn dismiss_failure(&mut self) {
        self.dock_state.countdown = None;
        self.dock_state.failure_session = None;
        self.dock_state.failure_copy.clear();
        let dock = self.dock();
        dock.set_failure_shown(false);
        dock.set_failure_expanded(false);
    }

    fn failure_action(&mut self, action: &str) {
        match action {
            "retry" => {
                let session = self.dock_state.failure_session.clone();
                self.dismiss_failure();
                self.send(Command::RetryInit { session });
            }
            "copy" => {
                let kind = self.dock().get_failure_kind();
                let title = self.dock().invoke_failure_title(kind);
                let text = format!("{title}\n{}", self.dock_state.failure_copy);
                self.window.global::<ChatUi>().invoke_copy(sv(&text));
            }
            "dismiss" => self.dismiss_failure(),
            _ => {}
        }
    }

    // ---- resumable run (docs/specs/harness/resumable-runs.md §6) -------------------------------------

    /// Asks the hub for the open session's interrupted run; the banner appears when there is one.
    fn load_resumable(&mut self) {
        let Some(session) = self.active.clone() else {
            self.show_resumable(None);
            return;
        };
        self.chat_call(
            move |reply| ChatCall::ResumableRun { session, reply },
            super::UiState::show_resumable,
        );
    }

    fn show_resumable(&mut self, run: Option<ResumableRun>) {
        let dock = self.dock();
        if let Some(run) = run.filter(|run| !run.resumed) {
            let plural = if run.finished_steps == 1 { "" } else { "s" };
            dock.set_resume_title(sv(&format!(
                "{} finished step{plural} before the interruption, for: {}",
                run.finished_steps,
                run.user_text.lines().next().unwrap_or_default()
            )));
            dock.set_resume_shown(true);
        } else {
            dock.set_resume_title(SharedString::new());
            dock.set_resume_shown(false);
        }
    }

    fn resume_action(&mut self, action: &str) {
        let Some(session) = self.active.clone() else {
            return;
        };
        self.dock().set_resume_shown(false);
        match action {
            "resume" => self.chat_call(
                move |reply| ChatCall::ResumeRun { session, reply },
                |_, ()| {},
            ),
            "discard" => self.chat_call(
                move |reply| ChatCall::DiscardRun { session, reply },
                |_, ()| {},
            ),
            _ => {}
        }
    }
}

pub(super) fn wire(window: &crate::ui::AppWindow) {
    let dock = window.global::<DockUi>();
    dock.on_queue_action(|action, id| with_state(|s| s.queue_action(&action, &id)));
    dock.on_queue_move(|id, delta| with_state(|s| s.queue_move(&id, delta)));
    dock.on_steer(|| with_state(UiState::steer));
    dock.on_pick_option(|which, id| with_state(|s| s.pick_option(&which, &id)));
    let weak = window.as_weak();
    dock.on_filter_models(move |query| {
        use slint::Model as _;
        let Some(window) = weak.upgrade() else {
            return;
        };
        let dock = window.global::<DockUi>();
        let all: Vec<(String, String)> = dock
            .get_models()
            .iter()
            .map(|row| (row.id.to_string(), row.name.to_string()))
            .collect();
        dock.set_models_shown(option_rows(&matching_models(&all, &query)));
    });
    dock.on_failure_action(|action| with_state(|s| s.failure_action(&action)));
    dock.on_resume_action(|action| with_state(|s| s.resume_action(&action)));
}
