use std::collections::HashMap;

use harness_core_api::{self as api, EventKind, HubEvent};

#[derive(Clone, Debug, Default)]
pub struct FleetSession {
    pub id: String,
    pub title: String,
    pub prompt: String,
    pub agent: String,
    pub running: bool,
    pub open: bool,
    pub subagents: Vec<api::SubagentView>,
}

#[derive(Default)]
struct Entry {
    agent: String,
    running: bool,
    prompt: String,
    queued: HashMap<api::TurnId, String>,
    subagents: Vec<api::SubagentView>,
}

#[derive(Default)]
pub(super) struct Fleet {
    entries: HashMap<String, Entry>,
    titles: HashMap<String, String>,
    children: HashMap<String, String>,
    order: Vec<String>,
    open: Option<String>,
}

impl Fleet {
    pub(super) fn set_sessions(&mut self, list: &[api::SessionSummary]) {
        self.titles.clear();
        self.children.clear();
        self.order.clear();
        for s in list {
            self.titles.insert(s.id.0.clone(), s.title.clone());
            self.order.push(s.id.0.clone());
            if let Some(parent) = &s.parent_id {
                self.children.insert(s.id.0.clone(), parent.0.clone());
            }
            let entry = self.entries.entry(s.id.0.clone()).or_default();
            if entry.agent.is_empty() || !entry.running {
                entry.agent.clone_from(&s.active_agent.0);
            }
        }
        let titles = &self.titles;
        self.entries.retain(|id, _| titles.contains_key(id));
    }

    pub(super) fn set_open(&mut self, id: &str, subagents: &[api::SubagentView], prompt: &str) {
        self.open = Some(id.to_owned());
        let entry = self.entries.entry(id.to_owned()).or_default();
        entry.subagents = subagents.to_vec();
        if !prompt.is_empty() {
            prompt.clone_into(&mut entry.prompt);
        }
    }

    pub(super) fn set_running(&mut self, id: &str, running: bool) {
        self.entries.entry(id.to_owned()).or_default().running = running;
    }

    pub(super) fn on_event(&mut self, event: &HubEvent) -> bool {
        let Some(session) = event.session_id.as_ref().map(|s| s.0.clone()) else {
            return false;
        };
        match &event.kind {
            EventKind::TurnQueued {
                turn_id, user_text, ..
            } => {
                let entry = self.entries.entry(session).or_default();
                entry.queued.insert(turn_id.clone(), user_text.clone());
                if entry.prompt.is_empty() {
                    entry.prompt.clone_from(user_text);
                }
                true
            }
            EventKind::TurnStarted { turn_id, agent, .. } => {
                let entry = self.entries.entry(session).or_default();
                entry.running = true;
                entry.agent.clone_from(&agent.0);
                if let Some(text) = entry.queued.remove(turn_id) {
                    entry.prompt = text;
                }
                true
            }
            EventKind::TurnFinished { .. } | EventKind::TurnPaused { .. } => {
                self.entries.entry(session).or_default().running = false;
                true
            }
            EventKind::AgentSwitched { to, .. } => {
                self.entries
                    .entry(session)
                    .or_default()
                    .agent
                    .clone_from(&to.0);
                true
            }
            EventKind::SubagentUpdated { subagent } => {
                let parent = subagent.parent_id.0.clone();
                self.children
                    .insert(subagent.session_id.0.clone(), parent.clone());
                let list = &mut self.entries.entry(parent).or_default().subagents;
                match list
                    .iter_mut()
                    .find(|s| s.session_id == subagent.session_id)
                {
                    Some(slot) => *slot = subagent.clone(),
                    None => list.insert(0, subagent.clone()),
                }
                true
            }
            _ => false,
        }
    }

    pub(super) fn snapshot(&self) -> Vec<FleetSession> {
        let open = self.open.as_deref();
        self.order
            .iter()
            .filter_map(|id| {
                let entry = self.entries.get(id);
                let is_open = open == Some(id.as_str());
                let busy = entry
                    .is_some_and(|e| e.running || e.subagents.iter().any(|s| s.status.is_active()));
                let child = self.children.contains_key(id);
                if !(is_open || (busy && !child)) {
                    return None;
                }
                Some(FleetSession {
                    id: id.clone(),
                    title: self.titles.get(id).cloned().unwrap_or_default(),
                    prompt: entry.map(|e| e.prompt.clone()).unwrap_or_default(),
                    agent: entry.map(|e| e.agent.clone()).unwrap_or_default(),
                    running: entry.is_some_and(|e| e.running),
                    open: is_open,
                    subagents: entry.map(|e| e.subagents.clone()).unwrap_or_default(),
                })
            })
            .collect()
    }
}
