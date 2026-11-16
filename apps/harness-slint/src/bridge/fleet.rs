use slint::{ComponentHandle, ModelRc, VecModel};

use harness_core_api::SubagentStatus;

use super::turns::{now_s, subagent_row};
use super::UiState;
use crate::core::FleetSession;
use crate::ui::{ChatUi, FleetGroup, SubagentRow};

const ORCH_H: i32 = 196;
const NODE_H: i32 = 116;
const NODE_GAP: i32 = 28;
const GROUP_GAP: i32 = 56;

fn int(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

impl UiState {
    pub(super) fn fleet(&self, list: &[FleetSession]) {
        let ui = self.window.global::<ChatUi>();
        ui.set_now_s(now_s());
        let mut y = 0;
        let mut running = 0;
        let mut waiting = 0;
        let mut sessions = 0;
        let mut groups = Vec::with_capacity(list.len());
        for s in list {
            let active: Vec<SubagentRow> = s
                .subagents
                .iter()
                .filter(|a| a.status.is_active())
                .map(|a| SubagentRow {
                    agent_name: self.agent_name(a.agent.as_str()).into(),
                    ..subagent_row(a)
                })
                .collect();
            running += active.len();
            waiting += s
                .subagents
                .iter()
                .filter(|a| matches!(a.status, SubagentStatus::NeedsApproval))
                .count();
            if s.running || !active.is_empty() {
                sessions += 1;
            }
            let n = int(active.len());
            let height = (n * NODE_H + (n - 1).max(0) * NODE_GAP).max(ORCH_H);
            groups.push(FleetGroup {
                id: s.id.as_str().into(),
                title: s.title.as_str().into(),
                prompt: s.prompt.trim().into(),
                agent: s.agent.as_str().into(),
                agent_name: self.agent_name(&s.agent).into(),
                running: s.running,
                open: s.open,
                y,
                height,
                total: int(s.subagents.len()),
                active: n,
                subagents: ModelRc::new(VecModel::from(active)),
            });
            y += height + GROUP_GAP;
        }
        ui.set_fleet_running(int(running));
        ui.set_fleet_waiting(int(waiting));
        ui.set_fleet_sessions(int(sessions));
        ui.set_fleet_height((y - GROUP_GAP).max(0));
        ui.set_fleet(ModelRc::new(VecModel::from(groups)));
    }
}
