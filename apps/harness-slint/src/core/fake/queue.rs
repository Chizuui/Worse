//! Fake-core queue edit / reorder / run-now / steer and session pickers (phase-9-parity.md §9.2.5 R12, R13), same
//! rules as the hub: only queued messages change, the running turn is never stopped.

use super::{emit, FakeCore};
use crate::core::{AgentId, Event, SessionId};

/// Session choices of the fake core (mode, model, effort).
#[derive(Default, Clone)]
pub(super) struct FakePrefs {
    pub mode: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub machine: bool,
}

/// A queue change.
pub(super) enum QueueOp {
    Edit(String, String),
    Reorder(Vec<String>),
    RunNow(String),
}

impl FakeCore {
    pub(super) fn queue_op(&mut self, session: SessionId, op: QueueOp) {
        let Some(s) = self.open.get_mut(&session) else {
            return emit(
                &self.ev,
                Event::Error(format!("session {session} not found")),
            );
        };
        let result = match op {
            QueueOp::Edit(id, text) => {
                if text.trim().is_empty() {
                    Err("message text is empty".to_owned())
                } else if let Some(q) = s.queue.iter_mut().find(|q| q.id == id) {
                    q.text = text;
                    Ok(())
                } else {
                    Err(format!("message {id} is not queued"))
                }
            }
            QueueOp::Reorder(ids) => {
                let mut wanted = ids.clone();
                let mut current: Vec<String> = s.queue.iter().map(|q| q.id.clone()).collect();
                wanted.sort();
                current.sort();
                if wanted == current {
                    let mut old: Vec<_> = s.queue.drain(..).collect();
                    for id in ids {
                        if let Some(index) = old.iter().position(|q| q.id == id) {
                            s.queue.push_back(old.remove(index));
                        }
                    }
                    Ok(())
                } else {
                    Err("the order must list every queued message once".to_owned())
                }
            }
            QueueOp::RunNow(id) => match s.queue.iter().position(|q| q.id == id) {
                Some(index) => {
                    if let Some(item) = s.queue.remove(index) {
                        s.queue.push_front(item);
                    }
                    Ok(())
                }
                None => Err(format!("message {id} is not queued")),
            },
        };
        let items = s.queue.iter().cloned().collect();
        match result {
            Ok(()) => emit(&self.ev, Event::Queue { session, items }),
            Err(error) => emit(&self.ev, Event::Error(error)),
        }
    }

    /// Steer: a normal send, then the new queued message (if it was queued) moves to the head.
    pub(super) async fn steer(&mut self, session: SessionId, agent: AgentId, text: String) {
        let before = self.open.get(&session).map_or(0, |s| s.queue.len());
        self.send(session.clone(), agent, text).await;
        let Some(s) = self.open.get_mut(&session) else {
            return;
        };
        if s.queue.len() > before && s.queue.len() > 1 {
            if let Some(item) = s.queue.pop_back() {
                s.queue.push_front(item);
            }
            let items = s.queue.iter().cloned().collect();
            emit(&self.ev, Event::Queue { session, items });
        }
    }

    /// Fixed lists of `harness-fake-agent` plus the session's choices.
    pub(super) fn emit_session_options(&self, session: &SessionId, pending: bool) {
        let Some(s) = self.open.get(session) else {
            return;
        };
        let prefs = self.prefs.get(session).cloned().unwrap_or_default();
        let pairs = |ids: &[&str]| {
            ids.iter()
                .map(|id| ((*id).to_owned(), (*id).to_owned()))
                .collect()
        };
        emit(
            &self.ev,
            Event::SessionOptions {
                session: session.clone(),
                agent: s.sticky.clone(),
                mode: Some(prefs.mode.unwrap_or_else(|| "default".into())),
                model: Some(prefs.model.unwrap_or_else(|| "fake-small".into())),
                effort: Some(prefs.effort.unwrap_or_else(|| "medium".into())),
                effort_id: Some("effort".into()),
                model_id: None,
                modes: pairs(&["default", "plan"]),
                models: pairs(&["fake-small", "fake-large"]),
                efforts: pairs(&["low", "medium", "high"]),
                known: true,
                pending,
                locked: s.run.is_some(),
                machine: prefs.machine,
            },
        );
    }

    pub(super) fn set_pref(&mut self, session: &SessionId, pick: impl FnOnce(&mut FakePrefs)) {
        if !self.open.contains_key(session) {
            return;
        }
        self.emit_session_options(session, true);
        pick(self.prefs.entry(session.clone()).or_default());
        self.emit_session_options(session, false);
    }
}
