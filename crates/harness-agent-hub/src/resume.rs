//! Journal tracker and resume entry points (spec `docs/specs/harness/resumable-runs.md` §4–§6).
//!
//! The tracker subscribes to the hub event stream (like `delegate::track`) and mirrors turn progress into
//! [`crate::journal`]. Resume sends a normal turn through the session actor, so the actor stays the only
//! writer of the transcript.

use std::sync::{Arc, Weak};

use harness_core_api::{
    ApiResult, EventKind, RecvError, ResumableRun, SendAccepted, SendMessage, SessionId, TurnId,
};

use crate::hub::{Hub, HubCtx};
use crate::journal::{self, RunOffer, StepRow};
use crate::util::{new_id, now_ms};

const RESUME_STEP_LINES: usize = 40;

pub(crate) async fn track(ctx: Weak<HubCtx>) {
    let Some(mut events) = ctx.upgrade().map(|c| c.events.subscribe()) else {
        return;
    };
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return,
        };
        let Some(ctx) = ctx.upgrade() else {
            return;
        };
        let Some(session) = event.session_id.clone() else {
            continue;
        };
        let at = now_ms();
        let result = ctx
            .store
            .run(move |c| mirror(c, &session, &event.kind, at))
            .await;
        if let Err(error) = result {
            tracing::warn!(%error, "run journal write failed");
        }
    }
}

fn mirror(
    c: &rusqlite::Connection,
    session: &SessionId,
    kind: &EventKind,
    at: i64,
) -> Result<(), crate::error::HubError> {
    match kind {
        EventKind::TurnStarted { turn_id, agent, .. } => {
            // 说明：TurnStarted 事件本身不带用户文本，而 journal 需要它来重建 resume 提示；
            // 此时 actor 已持久化该轮（persist_turn 先于 emit），因此从 turns 表读取是安全的。
            let text = journal::turn_user_text(c, turn_id)?.unwrap_or_default();
            journal::begin(c, session, turn_id, agent, &text, at)
        }
        EventKind::ToolCall { turn_id, tool, .. }
        | EventKind::ToolCallUpdated { turn_id, tool, .. } => {
            let (input, output) = journal::tool_texts(tool);
            journal::step(
                c,
                turn_id,
                journal::step_state(tool.status),
                Some(&tool.tool_call_id),
                Some(&tool.title),
                input,
                output,
                at,
            )
        }
        EventKind::TurnFinished { turn_id, .. } => journal::finish(c, turn_id, at),
        _ => Ok(()),
    }
}

fn prompt(offer: &RunOffer, steps: &[StepRow]) -> String {
    let done: Vec<&StepRow> = steps.iter().filter(|s| s.state == "tool_result").collect();
    let mut lines: Vec<String> = done.iter().map(|s| s.summary()).collect();
    if lines.len() > RESUME_STEP_LINES {
        lines.drain(..lines.len() - RESUME_STEP_LINES);
    }
    let body = if lines.is_empty() {
        "No tool call finished before the interruption.".to_owned()
    } else {
        format!(
            "Steps already executed before the interruption (do not repeat them, continue after step {}):\n{}",
            done.last().map_or(0, |s| s.idx),
            lines.join("\n")
        )
    };
    format!(
        "{text}\n\n<harness-resume run=\"{turn}\" finished-steps=\"{count}\">\n{body}\n</harness-resume>",
        text = offer.user_text,
        turn = offer.turn_id,
        count = done.len(),
    )
}

impl RunOffer {
    fn into_api(self, session: &SessionId) -> ResumableRun {
        ResumableRun {
            session_id: session.clone(),
            turn_id: self.turn_id,
            agent: self.agent,
            user_text: self.user_text,
            finished_steps: self.steps,
            last_step: self.last_step,
            started_at: self.started_at,
            finished_at: self.finished_at,
            resumed: self.resumed,
        }
    }
}

impl Hub {
    pub(crate) async fn do_resumable_run(
        &self,
        session: SessionId,
    ) -> ApiResult<Option<ResumableRun>> {
        self.check_open()?;
        let lookup = session.clone();
        let offer = self
            .ctx
            .store
            .run(move |c| journal::offer(c, &lookup))
            .await
            .map_err(harness_core_api::ApiError::from)?;
        Ok(offer.map(|offer| offer.into_api(&session)))
    }

    pub(crate) async fn do_discard_run(&self, session: SessionId) -> ApiResult<()> {
        self.check_open()?;
        let lookup = session.clone();
        self.ctx
            .store
            .run(move |c| journal::discard(c, &lookup))
            .await
            .map_err(harness_core_api::ApiError::from)?;
        self.ctx
            .events
            .emit(Some(&session), EventKind::SessionsChanged);
        Ok(())
    }

    pub(crate) async fn do_resume_run(&self, session: SessionId) -> ApiResult<SendAccepted> {
        self.check_open()?;
        self.unarchive_for_send(&session).await?;
        let lookup = session.clone();
        let found = self
            .ctx
            .store
            .run(move |c| {
                let offer = journal::offer(c, &lookup)?;
                let steps = match &offer {
                    Some(offer) => journal::steps(c, &offer.turn_id)?,
                    None => Vec::new(),
                };
                Ok(offer.map(|offer| (offer, steps)))
            })
            .await
            .map_err(harness_core_api::ApiError::from)?;
        let Some((offer, steps)) = found else {
            return Err(harness_core_api::ApiError::NotFound(format!(
                "resumable run in session {session}"
            )));
        };
        let text = prompt(&offer, &steps);
        let turn = offer.turn_id.clone();
        let at = now_ms();
        self.ctx
            .store
            .run(move |c| journal::mark_resumed(c, &turn, at))
            .await
            .map_err(harness_core_api::ApiError::from)?;
        let request = SendMessage {
            session_id: session.clone(),
            text,
            agent: Some(offer.agent.clone()),
            client_message_id: Some(new_id("resume")),
            context: Vec::new(),
        };
        let accepted = self
            .ask(&session, |reply| {
                crate::session::SessionCmd::Send(request, reply)
            })
            .await?;
        self.ctx
            .events
            .emit(Some(&session), EventKind::SessionsChanged);
        Ok(accepted)
    }
}

/// Keeps the unused-import checker honest when the journal is compiled for tests only.
#[allow(dead_code)]
fn _turn(_: &TurnId, _: Arc<HubCtx>) {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use harness_core_api::AgentId;

    fn step(idx: u32, state: &str, title: &str) -> StepRow {
        StepRow {
            idx,
            state: state.into(),
            tool_id: Some(format!("t{idx}")),
            tool_title: Some(title.into()),
            tool_input: None,
            tool_output: Some("done".into()),
        }
    }

    fn offer() -> RunOffer {
        RunOffer {
            turn_id: TurnId::new("turn-1"),
            agent: AgentId::new("claude-code"),
            user_text: "fix the build".into(),
            started_at: 1,
            finished_at: Some(2),
            steps: 2,
            last_step: Some("shell".into()),
            resumed: false,
        }
    }

    #[test]
    fn prompt_keeps_the_user_text_and_lists_finished_steps() {
        let text = prompt(
            &offer(),
            &[
                step(1, "tool_call", "read"),
                step(2, "tool_result", "shell"),
            ],
        );
        assert!(text.starts_with("fix the build\n\n<harness-resume"));
        assert!(text.contains("finished-steps=\"1\""));
        assert!(text.contains("2. shell -> done"));
        assert!(!text.contains("1. read -> done"));
    }

    #[test]
    fn prompt_without_results_says_so() {
        let text = prompt(&offer(), &[step(1, "tool_call", "read")]);
        assert!(text.contains("No tool call finished"));
    }

    #[test]
    fn prompt_keeps_the_tail_of_a_long_run() {
        let steps: Vec<StepRow> = (1..=60).map(|i| step(i, "tool_result", "shell")).collect();
        let text = prompt(&offer(), &steps);
        assert!(!text.contains("20. shell"));
        assert!(text.contains("21. shell -> done"));
        assert!(text.contains("60. shell -> done"));
    }
}
