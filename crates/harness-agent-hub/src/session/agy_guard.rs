//! Behaviour guard for every agy session (phase-9-parity.md §9.8.1): when agy calls its built-in
//! `invoke_subagent`, the actor stops the turn and queues one redirect turn at the front of the queue, worded for
//! the session's role. A redirect turn never triggers another redirect, so agy cannot loop.

use harness_core_api::{AgentId, SendMessage, ToolCallView, TurnId};

use super::SessionActor;
use crate::agy_delegate;

impl SessionActor {
    pub(super) async fn guard_agy_spawn(
        &mut self,
        turn_id: &TurnId,
        agent: &AgentId,
        tool: &ToolCallView,
    ) {
        let Some(task) = agy_delegate::builtin_spawn(&tool.title, tool.raw_input.as_deref()) else {
            return;
        };
        let Some(role) = agy_delegate::agy_role(&self.ctx, agent, &self.row.id) else {
            return;
        };
        if self.running.as_ref().is_none_or(|r| r.cancelling) {
            return;
        }
        let redirected = self
            .client_ids
            .iter()
            .any(|(id, turn)| turn == turn_id && id.starts_with(agy_delegate::REDIRECT_ID_PREFIX));
        if redirected {
            tracing::warn!(session = %self.row.id, %agent, "agy used invoke_subagent again after the redirect");
            return;
        }
        tracing::warn!(session = %self.row.id, %agent, "agy used invoke_subagent; redirecting to harness-delegate");
        self.on_stop().await;
        let request = SendMessage {
            session_id: self.row.id.clone(),
            text: agy_delegate::redirect_text(role, &task),
            agent: None,
            client_message_id: Some(format!("{}{turn_id}", agy_delegate::REDIRECT_ID_PREFIX)),
            context: Vec::new(),
        };
        match self.on_send(request, None).await {
            Ok(accepted) => {
                self.queue.retain(|t| t != &accepted.turn_id);
                self.queue.push_front(accepted.turn_id);
                self.emit_queue();
            }
            Err(error) => tracing::warn!(%error, "could not queue the agy redirect turn"),
        }
    }
}
