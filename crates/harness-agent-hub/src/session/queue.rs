//! Queue edit / reorder / run-now / steer (phase-9-parity.md §9.2.5 R12). The actor is the single owner of the
//! queue: every change runs inside its FIFO, so a turn that already started can no longer be changed and admission
//! stays serial.

use std::sync::Arc;

use harness_core_api::{
    ApiError, ApiResult, EventKind, QueuedTurn, SendMessage, SteerAccepted, SteerMode, TurnId,
    TurnSegment, TurnStatus, TurnView,
};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use super::{ActorMsg, SessionActor};
use crate::acp::{AcpConnection, RpcError};
use crate::store::ToolMark;
use crate::store_chat;
use crate::util::now_ms;

pub(crate) enum SteerReply {
    RunNow(oneshot::Sender<ApiResult<()>>),
    Steer(oneshot::Sender<ApiResult<SteerAccepted>>),
}

pub(crate) struct SteerFlight {
    turn_id: TurnId,
    host: TurnId,
    text: String,
    reply: SteerReply,
}

pub(super) fn user_with_steers(turn: &TurnView) -> String {
    turn.segments
        .iter()
        .fold(turn.user_text.clone(), |mut text, segment| {
            if let TurnSegment::Steer { text: steer } = segment {
                text.push_str("\n\n");
                text.push_str(steer);
            }
            text
        })
}

impl SessionActor {
    /// Emits the whole queue (replace semantics).
    pub(super) fn emit_queue(&self) {
        let queue = self
            .queue
            .iter()
            .map(|id| QueuedTurn {
                turn_id: id.clone(),
                user_text: self
                    .turn(id)
                    .map(|t| t.user_text.clone())
                    .unwrap_or_default(),
            })
            .collect();
        self.emit(EventKind::QueueChanged { queue });
    }

    fn not_queued(turn_id: &TurnId) -> ApiError {
        ApiError::Conflict(format!(
            "turn {turn_id} is not queued (it started, finished or does not exist)"
        ))
    }

    pub(super) async fn on_edit_queued(&mut self, turn_id: &TurnId, text: String) -> ApiResult<()> {
        if text.trim().is_empty() {
            return Err(ApiError::InvalidArgument("message text is empty".into()));
        }
        if !self.queue.contains(turn_id) {
            return Err(Self::not_queued(turn_id));
        }
        let (stored, id) = (text.clone(), turn_id.clone());
        self.ctx
            .store
            .run(move |c| store_chat::set_user_text(c, &id, &stored))
            .await?;
        if let Some(turn) = self.turn_mut(turn_id) {
            turn.user_text = text;
        }
        self.emit_queue();
        Ok(())
    }

    pub(super) fn on_reorder_queued(&mut self, turn_ids: Vec<TurnId>) -> ApiResult<()> {
        let mut wanted = turn_ids.clone();
        let mut current: Vec<TurnId> = self.queue.iter().cloned().collect();
        wanted.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        current.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        if wanted != current {
            return Err(ApiError::Conflict(
                "the order must list every queued turn exactly once (the queue changed)".into(),
            ));
        }
        self.queue = turn_ids.into_iter().collect();
        self.emit_queue();
        Ok(())
    }

    pub(super) fn on_run_queued_now(
        &mut self,
        turn_id: &TurnId,
        reply: oneshot::Sender<ApiResult<()>>,
    ) {
        let Some(index) = self.queue.iter().position(|t| t == turn_id) else {
            let _ = reply.send(Err(Self::not_queued(turn_id)));
            return;
        };
        if let Some(turn) = self.queue.remove(index) {
            self.queue.push_front(turn);
        }
        self.deliver(turn_id.clone(), SteerReply::RunNow(reply));
    }

    pub(super) async fn on_steer(
        &mut self,
        request: SendMessage,
        reply: oneshot::Sender<ApiResult<SteerAccepted>>,
    ) {
        let repeated = request
            .client_message_id
            .as_ref()
            .is_some_and(|id| self.client_ids.contains_key(id));
        let accepted = match self.on_send(request, None).await {
            Ok(accepted) => accepted,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        if repeated || self.queue.back() != Some(&accepted.turn_id) {
            let mode = if self
                .turn(&accepted.turn_id)
                .is_some_and(|t| t.stop_reason.as_deref() == Some(STEERED))
            {
                SteerMode::Injected
            } else {
                SteerMode::Queued
            };
            let _ = reply.send(Ok(SteerAccepted {
                turn_id: accepted.turn_id,
                seq: accepted.seq,
                mode,
            }));
            return;
        }
        if let Some(turn) = self.queue.pop_back() {
            self.queue.push_front(turn);
        }
        self.deliver(accepted.turn_id, SteerReply::Steer(reply));
    }

    fn steer_target(&self, turn_id: &TurnId) -> Option<(Arc<AcpConnection>, String, TurnId)> {
        if self.steering.is_some() {
            return None;
        }
        let running = self
            .running
            .as_ref()
            .filter(|r| r.prompt_sent && !r.cancelling)?;
        let connection = running
            .connection
            .as_ref()
            .filter(|c| c.supports_steering())?;
        let native = running.native.clone()?;
        let turn = self.turn(turn_id)?;
        if !turn.context.is_empty()
            || self
                .requested
                .get(turn_id)
                .is_some_and(|agent| agent != &running.agent)
        {
            return None;
        }
        Some((Arc::clone(connection), native, running.turn_id.clone()))
    }

    fn deliver(&mut self, turn_id: TurnId, reply: SteerReply) {
        let Some((connection, native, host)) = self.steer_target(&turn_id) else {
            self.emit_queue();
            self.answer(reply, &turn_id, SteerMode::Queued);
            return;
        };
        self.queue.retain(|t| t != &turn_id);
        let text = self
            .turn(&turn_id)
            .map(|t| t.user_text.clone())
            .unwrap_or_default();
        let receiver = connection.send_request(
            "_session/steering",
            json!({
                "sessionId": native,
                "prompt": [{"type": "text", "text": text}],
                "_meta": {"steering": {"idleBehavior": "promptRequired"}}
            }),
        );
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = receiver.await.unwrap_or_else(|_| {
                Err(RpcError {
                    code: RpcError::EXITED,
                    message: "agent process exited".into(),
                    data: None,
                })
            });
            let _ = tx.send(ActorMsg::SteerDone(result));
        });
        self.steering = Some(SteerFlight {
            turn_id,
            host,
            text,
            reply,
        });
        self.emit_queue();
    }

    fn answer(&self, reply: SteerReply, turn_id: &TurnId, mode: SteerMode) {
        match reply {
            SteerReply::RunNow(tx) => {
                let _ = tx.send(Ok(()));
            }
            SteerReply::Steer(tx) => {
                let seq = self.turn(turn_id).map_or(0, |t| t.seq);
                let _ = tx.send(Ok(SteerAccepted {
                    turn_id: turn_id.clone(),
                    seq,
                    mode,
                }));
            }
        }
    }

    pub(super) async fn on_steer_done(&mut self, result: Result<Value, RpcError>) {
        let Some(SteerFlight {
            turn_id,
            host,
            text,
            reply,
        }) = self.steering.take()
        else {
            return;
        };
        let outcome = match &result {
            Ok(value) => value
                .get("outcome")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            Err(error) => {
                tracing::warn!(code = error.code, message = %error.message, "steering request failed");
                String::new()
            }
        };
        if outcome == "injected" {
            self.record_steer(&turn_id, &host, text).await;
            self.answer(reply, &turn_id, SteerMode::Injected);
        } else {
            self.queue.push_front(turn_id.clone());
            self.emit_queue();
            self.answer(reply, &turn_id, SteerMode::Queued);
        }
        self.admit_next().await;
    }

    async fn record_steer(&mut self, turn_id: &TurnId, host: &TurnId, text: String) {
        let offset = self
            .running
            .as_ref()
            .filter(|r| &r.turn_id == host)
            .map(|r| r.text.len());
        let (agent, attempt) = self.turn(host).map_or((None, 1), |t| {
            (t.agent.clone(), t.attempts.last().map_or(1, |a| a.attempt))
        });
        if offset.is_some() {
            self.tool_marks
                .entry(host.clone())
                .or_default()
                .push(ToolMark {
                    tool_call_id: String::new(),
                    attempt: Some(attempt),
                    text_offset: offset,
                    steer: Some(text.clone()),
                });
        } else if let Some(turn) = self.turn_mut(host) {
            turn.segments
                .push(TurnSegment::Steer { text: text.clone() });
        }
        self.append_event(
            host,
            "steer",
            json!({"text": text, "attempt": attempt, "textOffset": offset}),
        )
        .await;
        if let Some(turn) = self.turn_mut(turn_id) {
            turn.status = TurnStatus::Completed;
            turn.stop_reason = Some(STEERED.to_owned());
            turn.agent = agent;
            turn.finished_at = Some(now_ms());
        }
        self.persist_turn(turn_id).await;
        self.emit(EventKind::SteerInjected {
            turn_id: host.clone(),
            steered_turn_id: turn_id.clone(),
            text,
        });
        self.touch().await;
    }

    pub(super) fn abandon_steer(&mut self) {
        if let Some(flight) = self.steering.take() {
            self.queue.push_front(flight.turn_id.clone());
            self.answer(flight.reply, &flight.turn_id, SteerMode::Queued);
        }
    }
}

pub(crate) const STEERED: &str = "steered";
