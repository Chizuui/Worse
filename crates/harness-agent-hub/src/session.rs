//! Session actor (spec §6.4, §7, §9): single owner of one session's transcript, queue, sticky agent,
//! native handles and pending permissions. Every input (UI commands, agent traffic, driver results)
//! arrives through one unbounded FIFO inbox, which gives the ordering guarantee of spec §9.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use harness_core_api::{
    AgentId, ApiError, ApiResult, ErrorClass, EventKind, PauseReason, PermissionDecision,
    PermissionOption, PermissionOptionKind, PermissionRequestId, PermissionRequestView, QuotaState,
    SendAccepted, SendMessage, SessionDetail, SwitchReason, ToolCallStatus, ToolCallView,
    ToolContent, TurnAttempt, TurnId, TurnOutcome, TurnStatus, TurnView,
};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::acp::{AcpConnection, Inbound, RouteSink, RpcError};
use crate::catchup::{
    catchup_block, clip_head_tail, partial_block, seed_block, ContextTurn, ToolDigest,
    TOOL_OUTPUT_CHARS,
};
use crate::hub::HubCtx;
use crate::memory::{injection_block, MemoryTarget};
use crate::options::{PrefChange, Prefs};
use crate::quota::{format_utc, AgentErrorInfo};
use crate::store::{self, HandleRow, SessionRow, ToolMark};
use crate::util::{new_id, now_ms, now_secs};

mod background;
mod chat;
mod queue;
pub(crate) use chat::{Resend, RewindCmd, RewindOutcome};

/// UI-originated commands.
pub(crate) enum SessionCmd {
    Send(SendMessage, oneshot::Sender<ApiResult<SendAccepted>>),
    SetAgent(AgentId, oneshot::Sender<ApiResult<()>>),
    Stop(oneshot::Sender<ApiResult<()>>),
    CancelQueued(TurnId, oneshot::Sender<ApiResult<()>>),
    RespondPermission(PermissionRequestId, String, oneshot::Sender<ApiResult<()>>),
    Snapshot(oneshot::Sender<ApiResult<SessionDetail>>),
    Shutdown(oneshot::Sender<()>),
    /// Active agent and the session's own choices for it (phase-9-parity.md §9.1.5).
    Prefs(oneshot::Sender<ApiResult<(AgentId, Prefs)>>),
    SetPref(PrefChange, oneshot::Sender<ApiResult<()>>),
    /// Archive/delete (phase-9-parity.md §9.1.5a): refuses with `Busy` while a turn is running, queued or paused
    /// or a permission is pending; otherwise answers `Ok` and the actor exits.
    Retire(oneshot::Sender<ApiResult<()>>),
    /// Send of a scheduled prompt: the turn carries its origin (phase-9-parity.md §9.2.1, TL-014).
    SendScheduled(
        SendMessage,
        harness_core_api::TurnOrigin,
        oneshot::Sender<ApiResult<SendAccepted>>,
    ),
    /// Title (the actor owns `row.title`).
    Rename(String, oneshot::Sender<ApiResult<()>>),
    /// Rewind / edit-and-resend / retry (truncate at a turn, optionally restore files, optionally resend).
    Rewind(RewindCmd, oneshot::Sender<ApiResult<RewindOutcome>>),
    /// Undo / reapply a turn's file changes (refused while busy).
    ApplyFiles(
        harness_core_api::ApplyFileChanges,
        oneshot::Sender<ApiResult<harness_core_api::FileRestoreReport>>,
    ),
    /// Queue edit / reorder / run-now / steer (phase-9-parity.md §9.2.5 R12).
    EditQueued(TurnId, String, oneshot::Sender<ApiResult<()>>),
    ReorderQueued(Vec<TurnId>, oneshot::Sender<ApiResult<()>>),
    RunQueuedNow(TurnId, oneshot::Sender<ApiResult<()>>),
    Steer(
        SendMessage,
        oneshot::Sender<ApiResult<harness_core_api::SteerAccepted>>,
    ),
    StopBackgroundTask(String, oneshot::Sender<ApiResult<()>>),
}

pub(crate) struct Ready {
    agent: AgentId,
    connection: Arc<AcpConnection>,
    native: String,
    fresh: bool,
    /// `session/new` / `session/load` result (option advertisement), `None` when reused.
    advertised: Option<Value>,
    /// Choices now in effect in the native session.
    applied: Prefs,
}

/// What the driver needs to bring a native session to the session's choices.
struct PrefPlan {
    own: Prefs,
    defaults: Option<harness_core_api::AgentDefaults>,
    applied: Option<Prefs>,
}

/// Everything the actor consumes, in one FIFO.
pub(crate) enum ActorMsg {
    Cmd(SessionCmd),
    Inbound(Inbound),
    Ready(u64, Result<Ready, AgentErrorInfo>),
    PromptDone(u64, Result<Value, RpcError>),
    SteerDone(Result<Value, RpcError>),
    Wake,
    AgentExited(AgentId, u64),
    IdleEnd(u64, u64),
}

pub(crate) type ActorTx = mpsc::UnboundedSender<ActorMsg>;

struct Handle {
    native: String,
    last_seq: u32,
    /// Pool generation of the process that has this native session loaded.
    generation: Option<u64>,
    /// Mode/model/config choices applied to the native session in that generation.
    applied: Option<Prefs>,
}

struct Running {
    turn_id: TurnId,
    agent: AgentId,
    key: u64,
    connection: Option<Arc<AcpConnection>>,
    native: Option<String>,
    text: String,
    thought: String,
    cancelling: bool,
    prompt_sent: bool,
    autonomous: Option<background::Autonomous>,
}

struct PendingPermission {
    view: PermissionRequestView,
    rpc_id: Value,
    connection: Arc<AcpConnection>,
}

pub(crate) struct SessionActor {
    ctx: Arc<HubCtx>,
    row: SessionRow,
    turns: Vec<TurnView>,
    client_ids: HashMap<String, TurnId>,
    requested: HashMap<TurnId, AgentId>,
    /// Index of the first attempt of the current failover round per turn (a pause ends a round).
    round_base: HashMap<TurnId, usize>,
    queue: VecDeque<TurnId>,
    running: Option<Running>,
    paused: Option<TurnId>,
    handles: HashMap<AgentId, Handle>,
    pending: Vec<PendingPermission>,
    tx: ActorTx,
    next_key: u64,
    /// Where tool calls appeared in the live stream, per turn (segments, spec §15.1 request 4).
    tool_marks: HashMap<TurnId, Vec<ToolMark>>,
    /// Turns whose `before` snapshot was requested (spec §15.2); they get an `after` snapshot at the end.
    snapshotted: HashSet<TurnId>,
    /// Mode/model/config choices per agent (owner of `session_prefs` rows of this session).
    prefs: HashMap<AgentId, Prefs>,
    steering: Option<queue::SteerFlight>,
    tasks: Vec<background::Tracked>,
}

impl SessionActor {
    /// Loads a session from the store and starts its actor task.
    pub(crate) async fn spawn(ctx: Arc<HubCtx>, row: SessionRow) -> ApiResult<ActorTx> {
        let id = row.id.clone();
        let (turns, handles, prefs) = ctx
            .store
            .run(move |c| {
                Ok((
                    store::load_turns(c, &id)?,
                    store::load_handles(c, &id)?,
                    store::load_prefs(c, &id)?,
                ))
            })
            .await?;
        let (tx, rx) = mpsc::unbounded_channel();
        let mut client_ids = HashMap::new();
        let turns = turns
            .into_iter()
            .map(|(turn, client_id)| {
                if let Some(client_id) = client_id {
                    client_ids.insert(client_id, turn.id.clone());
                }
                turn
            })
            .collect();
        let actor = Self {
            ctx,
            row,
            turns,
            client_ids,
            requested: HashMap::new(),
            round_base: HashMap::new(),
            queue: VecDeque::new(),
            running: None,
            paused: None,
            handles: handles
                .into_iter()
                .map(|h| {
                    (
                        h.agent,
                        Handle {
                            native: h.native,
                            last_seq: h.last_seq,
                            generation: None,
                            applied: None,
                        },
                    )
                })
                .collect(),
            pending: Vec::new(),
            tx: tx.clone(),
            next_key: 0,
            tool_marks: HashMap::new(),
            snapshotted: HashSet::new(),
            prefs,
            steering: None,
            tasks: Vec::new(),
        };
        tokio::spawn(actor.run(rx));
        Ok(tx)
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<ActorMsg>) {
        while let Some(message) = rx.recv().await {
            let shutdown = match message {
                ActorMsg::Cmd(SessionCmd::Shutdown(reply)) => {
                    self.on_shutdown().await;
                    let _ = reply.send(());
                    true
                }
                ActorMsg::Cmd(SessionCmd::Retire(reply)) => {
                    if self.busy() {
                        let _ = reply.send(Err(ApiError::Busy(format!(
                            "session {} has a running, queued or paused turn",
                            self.row.id
                        ))));
                        false
                    } else {
                        let _ = reply.send(Ok(()));
                        true
                    }
                }
                ActorMsg::Cmd(command) => {
                    self.on_command(command).await;
                    false
                }
                ActorMsg::Inbound(inbound) => {
                    self.on_inbound(inbound).await;
                    false
                }
                ActorMsg::Ready(key, result) => {
                    self.on_ready(key, result).await;
                    false
                }
                ActorMsg::PromptDone(key, result) => {
                    self.on_prompt_done(key, result).await;
                    false
                }
                ActorMsg::SteerDone(result) => {
                    self.on_steer_done(result).await;
                    false
                }
                ActorMsg::Wake => {
                    self.on_wake().await;
                    false
                }
                ActorMsg::AgentExited(agent, generation) => {
                    self.on_agent_exited(&agent, generation).await;
                    false
                }
                ActorMsg::IdleEnd(key, stamp) => {
                    self.on_idle_end(key, stamp).await;
                    false
                }
            };
            if shutdown {
                break;
            }
        }
    }

    // ---- commands ---------------------------------------------------------------------------------

    async fn on_command(&mut self, command: SessionCmd) {
        match command {
            SessionCmd::Send(request, reply) => {
                let result = self.on_send(request, None).await;
                let _ = reply.send(result);
                self.admit_next().await;
            }
            SessionCmd::SetAgent(agent, reply) => {
                let result = self.on_set_agent(agent).await;
                let _ = reply.send(result);
            }
            SessionCmd::Stop(reply) => {
                self.on_stop().await;
                let _ = reply.send(Ok(()));
            }
            SessionCmd::CancelQueued(turn_id, reply) => {
                let result = self.on_cancel_queued(&turn_id).await;
                let _ = reply.send(result);
            }
            SessionCmd::RespondPermission(request_id, option_id, reply) => {
                let result = self.on_respond_permission(&request_id, &option_id).await;
                let _ = reply.send(result);
            }
            SessionCmd::Snapshot(reply) => {
                let _ = reply.send(Ok(self.full_snapshot().await));
            }
            SessionCmd::SendScheduled(request, origin, reply) => {
                let result = self.on_send(request, Some(origin)).await;
                let _ = reply.send(result);
                self.admit_next().await;
            }
            SessionCmd::Rename(title, reply) => {
                let result = self.on_rename(&title).await;
                let _ = reply.send(result);
            }
            SessionCmd::Rewind(command, reply) => {
                let result = self.on_rewind(command).await;
                let _ = reply.send(result);
                self.admit_next().await;
            }
            SessionCmd::ApplyFiles(request, reply) => {
                let result = self.on_apply_files(request).await;
                let _ = reply.send(result);
            }
            SessionCmd::EditQueued(turn_id, text, reply) => {
                let result = self.on_edit_queued(&turn_id, text).await;
                let _ = reply.send(result);
            }
            SessionCmd::ReorderQueued(turn_ids, reply) => {
                let _ = reply.send(self.on_reorder_queued(turn_ids));
            }
            SessionCmd::RunQueuedNow(turn_id, reply) => {
                self.on_run_queued_now(&turn_id, reply);
                self.admit_next().await;
            }
            SessionCmd::Steer(request, reply) => {
                self.on_steer(request, reply).await;
                self.admit_next().await;
            }
            SessionCmd::StopBackgroundTask(task_id, reply) => {
                let _ = reply.send(self.on_stop_background_task(&task_id).await);
            }
            SessionCmd::Shutdown(reply) => {
                let _ = reply.send(());
            }
            // Handled in `run` (it ends the loop); kept for exhaustiveness.
            SessionCmd::Retire(reply) => {
                let _ = reply.send(Ok(()));
            }
            SessionCmd::Prefs(reply) => {
                let agent = self.row.active_agent.clone();
                let prefs = self.prefs.get(&agent).cloned().unwrap_or_default();
                let _ = reply.send(Ok((agent, prefs)));
            }
            SessionCmd::SetPref(change, reply) => {
                let result = self.on_set_pref(change).await;
                let _ = reply.send(result);
            }
        }
    }

    /// Stores a mode/model/config choice for the active agent. Never touches a running turn: the driver
    /// applies it before the next prompt (phase-9-parity.md §9.1.5).
    async fn on_set_pref(&mut self, change: PrefChange) -> ApiResult<()> {
        let agent = self.row.active_agent.clone();
        change.validate(&self.ctx.advertised(&agent).await)?;
        let mut prefs = self.prefs.get(&agent).cloned().unwrap_or_default();
        change.apply(&mut prefs);
        if self.prefs.get(&agent) == Some(&prefs) {
            return Ok(());
        }
        let (session, stored_agent) = (self.row.id.clone(), agent.clone());
        let (kind, key, value) = change.row();
        let (kind, key, value) = (kind, key.to_owned(), value.to_owned());
        self.ctx
            .store
            .run(move |c| store::save_pref(c, &session, &stored_agent, (kind, &key, &value)))
            .await?;
        self.prefs.insert(agent.clone(), prefs);
        self.emit(EventKind::SessionOptionsChanged { agent });
        Ok(())
    }

    async fn on_send(
        &mut self,
        request: SendMessage,
        origin: Option<harness_core_api::TurnOrigin>,
    ) -> ApiResult<SendAccepted> {
        if request.text.trim().is_empty() {
            return Err(ApiError::InvalidArgument("message text is empty".into()));
        }
        if let Some(client_id) = &request.client_message_id {
            if let Some(turn_id) = self.client_ids.get(client_id) {
                let turn = self
                    .turn(turn_id)
                    .ok_or_else(|| ApiError::NotFound(turn_id.to_string()))?;
                return Ok(SendAccepted {
                    turn_id: turn.id.clone(),
                    seq: turn.seq,
                    position: self.position_of(&turn.id),
                });
            }
        }
        if let Some(agent) = &request.agent {
            self.ctx.require_available(agent)?;
        }
        crate::context::validate(
            &self.ctx,
            &self.row.workspace_path,
            &self.row.id,
            &request.context,
        )
        .await?;
        let turn_id = TurnId::new(new_id("turn"));
        let context =
            crate::context::store_images(&self.ctx.config.home, &turn_id, request.context.clone())
                .await?;
        let seq = self.turns.last().map_or(1, |t| t.seq + 1);
        let turn = TurnView {
            id: turn_id,
            seq,
            user_text: request.text.clone(),
            agent: None,
            assistant_text: String::new(),
            status: TurnStatus::Queued,
            stop_reason: None,
            error: None,
            attempts: Vec::new(),
            tool_calls: Vec::new(),
            permissions: Vec::new(),
            created_at: now_ms(),
            finished_at: None,
            segments: Vec::new(),
            context,
            plan: Vec::new(),
            usage: None,
            context_usage: None,
            origin,
            thoughts: Vec::new(),
        };
        let session_id = self.row.id.clone();
        let stored = turn.clone();
        let client_id = request.client_message_id.clone();
        self.ctx
            .store
            .run(move |c| store::insert_turn(c, &session_id, &stored, client_id.as_deref()))
            .await?;
        if let Some(client_id) = request.client_message_id {
            self.client_ids.insert(client_id, turn.id.clone());
        }
        if let Some(agent) = request.agent {
            self.requested.insert(turn.id.clone(), agent);
        }
        let ahead = u32::from(self.running.is_some() || self.paused.is_some())
            + u32::try_from(self.queue.len()).unwrap_or(u32::MAX);
        self.queue.push_back(turn.id.clone());
        let accepted = SendAccepted {
            turn_id: turn.id.clone(),
            seq,
            position: ahead,
        };
        self.emit(EventKind::TurnQueued {
            turn_id: turn.id.clone(),
            seq,
            user_text: turn.user_text.clone(),
            position: ahead,
            background: false,
            images: harness_core_api::ContextItem::image_paths(&turn.context),
        });
        self.turns.push(turn);
        self.touch().await;
        Ok(accepted)
    }

    async fn on_set_agent(&mut self, agent: AgentId) -> ApiResult<()> {
        self.ctx.require_available(&agent)?;
        // A manual swap is a deliberate retry: clear the exhaustion mark.
        if !self.ctx.quota.is_usable(&agent) {
            self.ctx.set_quota(&agent, QuotaState::Ok).await;
        }
        self.set_sticky(&agent, SwitchReason::Manual).await;
        if let Some(paused) = self.paused.take() {
            self.requested.insert(paused.clone(), agent);
            self.start_turn(paused).await;
        }
        Ok(())
    }

    async fn on_stop(&mut self) {
        if let Some(running) = self.running.as_mut() {
            running.cancelling = true;
            if running.prompt_sent {
                if let (Some(connection), Some(native)) = (&running.connection, &running.native) {
                    connection.notify("session/cancel", json!({"sessionId": native}));
                }
            }
            self.cancel_pending_permissions();
            return;
        }
        if let Some(paused) = self.paused.take() {
            self.finish(
                &paused,
                TurnStatus::Cancelled,
                None,
                None,
                TurnOutcome::Cancelled,
            )
            .await;
            self.admit_next().await;
        }
    }

    async fn on_cancel_queued(&mut self, turn_id: &TurnId) -> ApiResult<()> {
        let Some(index) = self.queue.iter().position(|t| t == turn_id) else {
            return Err(ApiError::NotFound(format!("queued turn {turn_id}")));
        };
        self.queue.remove(index);
        self.finish(
            turn_id,
            TurnStatus::Cancelled,
            None,
            None,
            TurnOutcome::Cancelled,
        )
        .await;
        Ok(())
    }

    async fn on_respond_permission(
        &mut self,
        request_id: &PermissionRequestId,
        option_id: &str,
    ) -> ApiResult<()> {
        let Some(index) = self
            .pending
            .iter()
            .position(|p| &p.view.request_id == request_id)
        else {
            return Err(ApiError::AlreadyResolved(request_id.to_string()));
        };
        let Some(option) = self.pending[index]
            .view
            .options
            .iter()
            .find(|o| o.option_id == option_id)
            .cloned()
        else {
            return Err(ApiError::InvalidArgument(format!(
                "unknown option {option_id}"
            )));
        };
        let pending = self.pending.remove(index);
        pending.connection.respond(
            &pending.rpc_id,
            json!({"outcome": {"outcome": "selected", "optionId": option.option_id}}),
        );
        let decision = PermissionDecision::Selected {
            option_id: option.option_id,
            kind: option.kind,
        };
        self.record_permission(&pending.view, decision).await;
        Ok(())
    }

    async fn on_shutdown(&mut self) {
        self.release_task_leases().await;
        self.abandon_steer();
        self.on_stop().await;
        if let Some(running) = self.running.take() {
            let turn_id = running.turn_id.clone();
            if let Some(connection) = &running.connection {
                self.ctx
                    .pool
                    .release(&running.agent, connection.generation())
                    .await;
            }
            self.finish(
                &turn_id,
                TurnStatus::Cancelled,
                None,
                Some("hub shut down".into()),
                TurnOutcome::Cancelled,
            )
            .await;
        }
        let queued: Vec<TurnId> = self.queue.drain(..).chain(self.paused.take()).collect();
        for turn_id in queued {
            self.finish(
                &turn_id,
                TurnStatus::Interrupted,
                None,
                None,
                TurnOutcome::Cancelled,
            )
            .await;
        }
    }

    // ---- admission & agent selection --------------------------------------------------------------

    async fn admit_next(&mut self) {
        if self.running.is_some() || self.paused.is_some() || self.steering.is_some() {
            return;
        }
        if let Some(turn_id) = self.queue.pop_front() {
            self.start_turn(turn_id).await;
        }
    }

    async fn on_wake(&mut self) {
        if self.running.is_none() {
            if let Some(paused) = self.paused.take() {
                self.start_turn(paused).await;
            }
        }
    }

    /// Picks the agent for a turn: explicit request, else sticky, else fallback order (spec §6.4).
    fn select_agent(&self, turn_id: &TurnId) -> Option<AgentId> {
        // Idempotency (spec §6.4): within one failover round an agent runs the turn at most once.
        // A pause ends the round, so a manual swap or a reset may retry an agent that already tried.
        let base = self.round_base.get(turn_id).copied().unwrap_or(0);
        let tried: Vec<AgentId> = self
            .turn(turn_id)
            .map(|t| {
                t.attempts
                    .iter()
                    .skip(base)
                    .map(|a| a.agent.clone())
                    .collect()
            })
            .unwrap_or_default();
        let usable = |agent: &AgentId| {
            !tried.contains(agent)
                && self.ctx.is_installed(agent)
                && self.ctx.quota.is_usable(agent)
        };
        if let Some(agent) = self.requested.get(turn_id) {
            return (!tried.contains(agent)).then(|| agent.clone());
        }
        if usable(&self.row.active_agent) {
            return Some(self.row.active_agent.clone());
        }
        let order = self.ctx.fallback_order();
        let start = order
            .iter()
            .position(|a| a == &self.row.active_agent)
            .map_or(0, |i| i + 1);
        (0..order.len())
            .map(|offset| &order[(start + offset) % order.len()])
            .find(|agent| usable(agent))
            .cloned()
    }

    async fn set_sticky(&mut self, agent: &AgentId, reason: SwitchReason) {
        if &self.row.active_agent == agent {
            return;
        }
        let from = self.row.active_agent.clone();
        let message = match &reason {
            SwitchReason::Manual => format!(
                "switched from {} to {}",
                self.ctx.display_name(&from),
                self.ctx.display_name(agent)
            ),
            SwitchReason::QuotaExhausted { resets_at } => format!(
                "switched from {} to {}: {} limit reached ({})",
                self.ctx.display_name(&from),
                self.ctx.display_name(agent),
                self.ctx.display_name(&from),
                resets_at.map_or_else(
                    || "reset time unknown".to_owned(),
                    |t| format!("resets at {}", format_utc(t))
                )
            ),
            SwitchReason::Unavailable => format!(
                "switched from {} to {}: {} is not available",
                self.ctx.display_name(&from),
                self.ctx.display_name(agent),
                self.ctx.display_name(&from)
            ),
        };
        self.row.active_agent = agent.clone();
        self.touch().await;
        self.emit(EventKind::AgentSwitched {
            from: Some(from),
            to: agent.clone(),
            reason,
            message,
        });
    }

    async fn start_turn(&mut self, turn_id: TurnId) {
        let Some(agent) = self.select_agent(&turn_id) else {
            self.pause(turn_id).await;
            return;
        };
        if self.requested.remove(&turn_id).is_some() {
            self.set_sticky(&agent, SwitchReason::Manual).await;
        } else if agent != self.row.active_agent {
            let previous = self.row.active_agent.clone();
            let reason = match self.ctx.quota.get(&previous) {
                QuotaState::Exhausted { resets_at, .. } => {
                    SwitchReason::QuotaExhausted { resets_at }
                }
                QuotaState::Ok => SwitchReason::Unavailable,
            };
            self.note_failover(&turn_id, &previous, &agent).await;
            self.set_sticky(&agent, reason).await;
        }
        let attempt_no = self.turn(&turn_id).map_or(1, |t| {
            u32::try_from(t.attempts.len()).unwrap_or(u32::MAX) + 1
        });
        let attempt = TurnAttempt {
            attempt: attempt_no,
            agent: agent.clone(),
            status: "running".into(),
            partial_text: String::new(),
            error: None,
            error_class: None,
        };
        if let Some(turn) = self.turn_mut(&turn_id) {
            turn.status = TurnStatus::Running;
            turn.agent = Some(agent.clone());
            turn.attempts.push(attempt.clone());
        }
        self.persist_turn(&turn_id).await;
        self.persist_attempt(&turn_id, attempt).await;
        self.emit(EventKind::TurnStarted {
            turn_id: turn_id.clone(),
            agent: agent.clone(),
            attempt: attempt_no,
        });
        self.next_key += 1;
        let key = self.next_key;
        let turn_id_for_snapshot = turn_id.clone();
        self.running = Some(Running {
            turn_id,
            agent: agent.clone(),
            key,
            connection: None,
            native: None,
            text: String::new(),
            thought: String::new(),
            cancelling: false,
            prompt_sent: false,
            autonomous: None,
        });
        // 快照只在第一次尝试前拍（failover 重跑保留同一 before，spec §15.2）。
        let snapshot =
            self.ctx.config.turn_snapshots && self.snapshotted.insert(turn_id_for_snapshot.clone());
        self.spawn_driver(key, agent, snapshot.then_some(turn_id_for_snapshot));
    }

    async fn pause(&mut self, turn_id: TurnId) {
        let agents = self
            .ctx
            .fallback_order()
            .into_iter()
            .map(|agent| {
                let resets_at = match self.ctx.quota.get(&agent) {
                    QuotaState::Exhausted { resets_at, .. } => resets_at,
                    QuotaState::Ok => None,
                };
                (agent, resets_at)
            })
            .collect();
        let attempts = self.turn(&turn_id).map_or(0, |t| t.attempts.len());
        self.round_base.insert(turn_id.clone(), attempts);
        if let Some(turn) = self.turn_mut(&turn_id) {
            turn.status = TurnStatus::Paused;
        }
        self.persist_turn(&turn_id).await;
        self.emit(EventKind::TurnPaused {
            turn_id: turn_id.clone(),
            reason: PauseReason::AllAgentsExhausted { agents },
        });
        self.paused = Some(turn_id);
        if let Some(reset) = self.ctx.quota.earliest_reset() {
            let tx = self.tx.clone();
            let wait = u64::try_from(reset - now_secs()).unwrap_or(0) + 1;
            tokio::spawn(async move {
                // Product rule (spec §6.4): resume a paused turn when the earliest known reset passes.
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                let _ = tx.send(ActorMsg::Wake);
            });
        }
    }

    // ---- driver: process + native session setup ---------------------------------------------------

    fn spawn_driver(&self, key: u64, agent: AgentId, snapshot: Option<TurnId>) {
        let ctx = Arc::clone(&self.ctx);
        let tx = self.tx.clone();
        let handle = self
            .handles
            .get(&agent)
            .map(|h| (h.native.clone(), h.generation));
        let plan = PrefPlan {
            own: self.prefs.get(&agent).cloned().unwrap_or_default(),
            defaults: self.ctx.config.agent_defaults.get(&agent).cloned(),
            applied: self.handles.get(&agent).and_then(|h| h.applied.clone()),
        };
        let cwd = self.row.workspace_path.clone();
        let session = self.row.id.clone();
        tokio::spawn(async move {
            if let Some(turn_id) = snapshot {
                // Before the prompt is sent (it is sent only after Ready), so agent edits cannot precede it.
                take_before_snapshot(&ctx, &turn_id, &cwd).await;
            }
            let result = drive_setup(&ctx, &tx, &agent, handle, &cwd, plan, &session).await;
            let _ = tx.send(ActorMsg::Ready(key, result));
        });
    }

    async fn on_ready(&mut self, key: u64, result: Result<Ready, AgentErrorInfo>) {
        let Some(running) = self.running.as_ref().filter(|r| r.key == key) else {
            if let Ok(ready) = result {
                // Stale setup (turn already ended): give the process back.
                self.ctx
                    .pool
                    .release(&ready.agent, ready.connection.generation())
                    .await;
            }
            return;
        };
        let turn_id = running.turn_id.clone();
        let agent = running.agent.clone();
        let ready = match result {
            Ok(ready) => ready,
            Err(error) => {
                self.on_attempt_failed(&turn_id, &agent, error, crate::failure::Phase::Setup)
                    .await;
                return;
            }
        };
        let generation = ready.connection.generation();
        let previous_seq = if ready.fresh {
            0
        } else {
            self.handles.get(&agent).map_or(0, |h| h.last_seq)
        };
        self.handles.insert(
            agent.clone(),
            Handle {
                native: ready.native.clone(),
                last_seq: previous_seq,
                generation: Some(generation),
                applied: Some(ready.applied.clone()),
            },
        );
        self.persist_handle(&agent).await;
        if let Some(result) = &ready.advertised {
            if self
                .ctx
                .record_advertised(&agent, |a| a.merge_response(result))
                .await
            {
                self.emit(EventKind::SessionOptionsChanged {
                    agent: agent.clone(),
                });
            }
        }
        let cancelling = self.running.as_ref().is_some_and(|r| r.cancelling);
        if let Some(running) = self.running.as_mut() {
            running.connection = Some(Arc::clone(&ready.connection));
            running.native = Some(ready.native.clone());
        }
        if cancelling {
            self.end_running(TurnStatus::Cancelled, None, None, TurnOutcome::Cancelled)
                .await;
            return;
        }
        let (image, embedded) = ready.connection.prompt_capabilities();
        let caps = crate::context::Caps { image, embedded };
        self.ctx.chat.set_caps(&agent, caps);
        let blocks = self
            .prompt_blocks(&turn_id, &agent, ready.fresh, previous_seq, caps)
            .await;
        let receiver = ready.connection.send_request(
            "session/prompt",
            json!({"sessionId": ready.native, "prompt": blocks}),
        );
        if let Some(running) = self.running.as_mut() {
            running.prompt_sent = true;
        }
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = receiver.await.unwrap_or_else(|_| {
                Err(RpcError {
                    code: RpcError::EXITED,
                    message: "agent process exited".into(),
                    data: None,
                })
            });
            // Posted after the reader delivered all updates of this prompt (spec §9).
            let _ = tx.send(ActorMsg::PromptDone(key, result));
        });
    }

    async fn prompt_blocks(
        &self,
        turn_id: &TurnId,
        agent: &AgentId,
        fresh: bool,
        previous_seq: u32,
        caps: crate::context::Caps,
    ) -> Vec<Value> {
        let Some(turn) = self.turn(turn_id) else {
            return Vec::new();
        };
        let seq = turn.seq;
        let tools = tool_digests(&turn.tool_calls);
        let mut blocks = Vec::new();
        let earlier = |include: &dyn Fn(&TurnView) -> bool| -> Vec<ContextTurn> {
            self.turns
                .iter()
                .filter(|t| t.seq < seq && !t.assistant_text.is_empty() && include(t))
                .map(|t| ContextTurn {
                    seq: t.seq,
                    agent: t
                        .agent
                        .as_ref()
                        .map_or_else(String::new, |a| self.ctx.display_name(a)),
                    user: queue::user_with_steers(t),
                    assistant: t.assistant_text.clone(),
                    tools: tool_digests(&t.tool_calls),
                })
                .collect()
        };
        let mut notes_pushed = false;
        if fresh {
            blocks.push(text_block(&self.memory_block().await));
            if let Some(notes) = self.notes_block().await {
                blocks.push(text_block(&notes));
                notes_pushed = true;
            }
            if let Some(seed) = seed_block(&earlier(&|_| true)) {
                blocks.push(text_block(&seed));
            }
        } else if let Some(catchup) = catchup_block(&earlier(&|t| {
            t.seq > previous_seq && t.agent.as_ref() != Some(agent)
        })) {
            blocks.push(text_block(&catchup));
            if let Some(notes) = self.notes_block().await {
                blocks.push(text_block(&notes));
                notes_pushed = true;
            }
        }
        let mut failed_over = false;
        for attempt in turn
            .attempts
            .iter()
            .filter(|a| a.status == "quotaExhausted")
        {
            failed_over = true;
            blocks.push(text_block(&partial_block(
                &self.ctx.display_name(&attempt.agent),
                &attempt.partial_text,
                &tools,
            )));
        }
        if failed_over && !notes_pushed {
            if let Some(notes) = self.notes_block().await {
                blocks.push(text_block(&notes));
            }
        }
        blocks.extend(
            crate::context::blocks(
                &self.ctx,
                &self.row.workspace_path,
                &turn.context,
                caps,
                &self.ctx.display_name(agent),
            )
            .await,
        );
        let mut user = None;
        if let Some((name, input)) = crate::hub_chat::skill_call(&turn.user_text) {
            if let Some(skill) = crate::hub_chat::harness_skills(&self.ctx)
                .await
                .into_iter()
                .find(|s| s.name == name)
            {
                user = Some(crate::hub_chat::skill_prompt(&skill, input));
            }
        }
        blocks.push(text_block(user.as_deref().unwrap_or(&turn.user_text)));
        blocks
    }

    async fn memory_block(&self) -> String {
        let memory = self.ctx.memory.clone();
        let project = std::path::PathBuf::from(&self.row.workspace_path);
        let root = self.row.workspace_path.clone();
        tokio::task::spawn_blocking(move || {
            let global = memory.read(&MemoryTarget::Global).unwrap_or_default();
            let project = memory
                .read(&MemoryTarget::Project(project))
                .unwrap_or_default();
            injection_block(&global, &project, &root)
        })
        .await
        .unwrap_or_default()
    }

    async fn notes_block(&self) -> Option<String> {
        let home = self.ctx.config.home.clone();
        let session = self.row.id.as_str().to_owned();
        tokio::task::spawn_blocking(move || crate::session_notes::block(&home, &session))
            .await
            .ok()
            .filter(|block| !block.is_empty())
    }

    // ---- agent traffic ----------------------------------------------------------------------------

    async fn on_inbound(&mut self, inbound: Inbound) {
        match inbound {
            Inbound::Update { native, update } => {
                // 命令列表可能在 session/new 之后、Ready 之前到达，此时 running.native 尚未设置；按 native 归属接收。
                if update.get("sessionUpdate").and_then(Value::as_str)
                    == Some("available_commands_update")
                {
                    self.on_commands_update(&native, &update).await;
                } else if background::is_task_update(&update) {
                    self.on_task_update(&native, &update).await;
                } else if self.running.as_ref().and_then(|r| r.native.as_ref()) == Some(&native) {
                    self.on_update(&update).await;
                    self.arm_idle_end();
                } else if self.running.is_none() {
                    self.on_unprompted_update(&native, &update).await;
                }
            }
            Inbound::Request {
                native,
                id,
                method,
                params,
            } => {
                let matches = self
                    .running
                    .as_ref()
                    .is_some_and(|r| r.native.as_ref() == Some(&native) && !r.cancelling);
                let connection = self.running.as_ref().and_then(|r| r.connection.clone());
                match (method.as_str(), matches, connection) {
                    ("session/request_permission", true, Some(connection)) => {
                        self.on_permission_request(connection, id, &params).await;
                    }
                    ("session/request_permission", _, Some(connection)) => {
                        connection.respond(&id, json!({"outcome": {"outcome": "cancelled"}}));
                    }
                    (_, _, Some(connection)) => {
                        connection.respond_error(&id, -32601, "method not supported by Harness");
                    }
                    _ => {}
                }
            }
        }
    }

    async fn flush_thought(&mut self) {
        let Some(running) = self.running.as_mut() else {
            return;
        };
        if running.thought.trim().is_empty() {
            running.thought.clear();
            return;
        }
        let text = std::mem::take(&mut running.thought);
        let turn_id = running.turn_id.clone();
        let text_offset = running.text.len();
        let Some(turn) = self.turn_mut(&turn_id) else {
            return;
        };
        let thought = harness_core_api::ThoughtView {
            attempt: turn.attempts.last().map_or(1, |a| a.attempt),
            text_offset,
            text,
        };
        turn.thoughts.push(thought.clone());
        let payload = serde_json::to_value(&thought).unwrap_or(Value::Null);
        self.append_event(&turn_id, "thought", payload).await;
    }

    async fn on_update(&mut self, update: &Value) {
        let Some(running) = self.running.as_mut() else {
            return;
        };
        let turn_id = running.turn_id.clone();
        let agent = running.agent.clone();
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or("");
        match kind {
            "agent_message_chunk" | "agent_thought_chunk" => {
                let Some(text) = update
                    .get("content")
                    .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
                    .and_then(|c| c.get("text"))
                    .and_then(Value::as_str)
                else {
                    return;
                };
                let text = text.to_owned();
                if kind == "agent_message_chunk" {
                    self.flush_thought().await;
                    let Some(running) = self.running.as_mut() else {
                        return;
                    };
                    running.text.push_str(&text);
                    self.emit(EventKind::MessageDelta {
                        turn_id,
                        agent,
                        text,
                    });
                } else {
                    running.thought.push_str(&text);
                    self.emit(EventKind::ThoughtDelta {
                        turn_id,
                        agent,
                        text,
                    });
                }
            }
            "tool_call" | "tool_call_update" => {
                self.flush_thought().await;
                let tool = tool_view(update);
                let offset = self.running.as_ref().map_or(0, |r| r.text.len());
                let mut mark = None;
                if let Some(turn) = self.turn_mut(&turn_id) {
                    let attempt = turn.attempts.last().map_or(1, |a| a.attempt);
                    if !turn
                        .tool_calls
                        .iter()
                        .any(|t| t.tool_call_id == tool.tool_call_id)
                    {
                        mark = Some(ToolMark {
                            tool_call_id: tool.tool_call_id.clone(),
                            attempt: Some(attempt),
                            text_offset: Some(offset),
                            steer: None,
                        });
                    }
                    store::merge_tool(&mut turn.tool_calls, tool.clone());
                }
                let mut payload = serde_json::to_value(&tool).unwrap_or(Value::Null);
                if let (Some(mark), Some(object)) = (&mark, payload.as_object_mut()) {
                    object.insert("attempt".into(), json!(mark.attempt));
                    object.insert("textOffset".into(), json!(mark.text_offset));
                }
                if let Some(mark) = mark {
                    self.tool_marks
                        .entry(turn_id.clone())
                        .or_default()
                        .push(mark);
                }
                self.append_event(&turn_id, "tool_call", payload).await;
                let event = if kind == "tool_call" {
                    EventKind::ToolCall {
                        turn_id,
                        agent,
                        tool,
                    }
                } else {
                    EventKind::ToolCallUpdated {
                        turn_id,
                        agent,
                        tool,
                    }
                };
                self.emit(event);
            }
            "current_mode_update" | "config_option_update" => {
                self.on_agent_option_update(&agent, update).await;
            }
            "plan" | "usage_update" => self.on_progress_update(&turn_id, &agent, update).await,
            _ => {}
        }
    }

    /// The agent changed its own mode/options during a turn (e.g. leaving plan mode). That becomes the
    /// session's choice too, otherwise the next turn would switch the agent back.
    async fn on_agent_option_update(&mut self, agent: &AgentId, update: &Value) {
        let changed = self
            .ctx
            .record_advertised(agent, |a| a.merge_update(update))
            .await;
        let mut followed = Vec::new();
        if let Some(mode) = update
            .get("currentModeId")
            .and_then(Value::as_str)
            .filter(|_| {
                update.get("sessionUpdate").and_then(Value::as_str) == Some("current_mode_update")
            })
        {
            if let Some(applied) = self.handles.get_mut(agent).and_then(|h| h.applied.as_mut()) {
                applied.mode = Some(mode.to_owned());
            }
            if self
                .prefs
                .get(agent)
                .is_some_and(|p| p.mode.as_ref().is_some_and(|m| m != mode))
            {
                followed.push(PrefChange::Mode(mode.to_owned()));
            }
        }
        for change in followed {
            if let Some(prefs) = self.prefs.get_mut(agent) {
                change.apply(prefs);
            }
            let (session, stored_agent) = (self.row.id.clone(), agent.clone());
            let (kind, key, value) = change.row();
            let (key, value) = (key.to_owned(), value.to_owned());
            let result = self
                .ctx
                .store
                .run(move |c| store::save_pref(c, &session, &stored_agent, (kind, &key, &value)))
                .await;
            self.log_store(result);
        }
        if changed {
            self.emit(EventKind::SessionOptionsChanged {
                agent: agent.clone(),
            });
        }
    }

    async fn on_permission_request(
        &mut self,
        connection: Arc<AcpConnection>,
        rpc_id: Value,
        params: &Value,
    ) {
        let Some(running) = self.running.as_ref() else {
            return;
        };
        let options = params
            .get("options")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(permission_option)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let view = PermissionRequestView {
            request_id: PermissionRequestId::new(new_id("perm")),
            turn_id: running.turn_id.clone(),
            agent: running.agent.clone(),
            tool: tool_view(params.get("toolCall").unwrap_or(&Value::Null)),
            options,
        };
        if self.ctx.routers.is_router(&self.row.id) {
            let title = view.tool.title.to_lowercase();
            let allowed = [
                "harness-delegate",
                "harness_delegate",
                "list_agents",
                "route_trigger",
                "dismiss_trigger",
            ]
            .iter()
            .any(|name| title.contains(name));
            let wanted = if allowed {
                [
                    PermissionOptionKind::AllowAlways,
                    PermissionOptionKind::AllowOnce,
                ]
            } else {
                [
                    PermissionOptionKind::RejectOnce,
                    PermissionOptionKind::RejectAlways,
                ]
            };
            let option = wanted
                .iter()
                .find_map(|kind| view.options.iter().find(|o| o.kind == *kind))
                .cloned();
            let decision = match option {
                Some(option) => {
                    connection.respond(
                        &rpc_id,
                        json!({"outcome": {"outcome": "selected", "optionId": option.option_id}}),
                    );
                    PermissionDecision::Selected {
                        option_id: option.option_id,
                        kind: option.kind,
                    }
                }
                None => {
                    connection.respond(&rpc_id, json!({"outcome": {"outcome": "cancelled"}}));
                    PermissionDecision::Cancelled
                }
            };
            self.record_permission(&view, decision).await;
            return;
        }
        if self.ctx.machine.is_on(self.row.id.as_str()) {
            let allow = view
                .options
                .iter()
                .find(|o| matches!(o.kind, PermissionOptionKind::AllowAlways))
                .or_else(|| {
                    view.options
                        .iter()
                        .find(|o| matches!(o.kind, PermissionOptionKind::AllowOnce))
                })
                .cloned();
            if let Some(option) = allow {
                connection.respond(
                    &rpc_id,
                    json!({"outcome": {"outcome": "selected", "optionId": option.option_id}}),
                );
                let decision = PermissionDecision::Selected {
                    option_id: option.option_id,
                    kind: option.kind,
                };
                self.record_permission(&view, decision).await;
                return;
            }
        }
        self.pending.push(PendingPermission {
            view: view.clone(),
            rpc_id,
            connection,
        });
        self.emit(EventKind::PermissionRequested { request: view });
    }

    fn cancel_pending_permissions(&mut self) {
        for pending in std::mem::take(&mut self.pending) {
            pending.connection.respond(
                &pending.rpc_id,
                json!({"outcome": {"outcome": "cancelled"}}),
            );
            if let Some(turn) = self.turn_mut(&pending.view.turn_id) {
                turn.permissions.push((
                    pending.view.tool.title.clone(),
                    PermissionDecision::Cancelled,
                ));
            }
            let payload = json!({"title": pending.view.tool.title, "decision": PermissionDecision::Cancelled});
            let store = self.ctx.store.clone();
            let turn_id = pending.view.turn_id.clone();
            tokio::spawn(async move {
                let _ = store
                    .run(move |c| store::append_turn_event(c, &turn_id, "permission", &payload))
                    .await;
            });
            self.emit(EventKind::PermissionResolved {
                turn_id: pending.view.turn_id,
                request_id: pending.view.request_id,
                decision: PermissionDecision::Cancelled,
            });
        }
    }

    async fn record_permission(
        &mut self,
        view: &PermissionRequestView,
        decision: PermissionDecision,
    ) {
        if let Some(turn) = self.turn_mut(&view.turn_id) {
            turn.permissions
                .push((view.tool.title.clone(), decision.clone()));
        }
        self.append_event(
            &view.turn_id,
            "permission",
            json!({"title": view.tool.title, "agent": view.agent, "decision": decision}),
        )
        .await;
        self.emit(EventKind::PermissionResolved {
            turn_id: view.turn_id.clone(),
            request_id: view.request_id.clone(),
            decision,
        });
    }

    // ---- turn end ---------------------------------------------------------------------------------

    async fn on_prompt_done(&mut self, key: u64, result: Result<Value, RpcError>) {
        let Some(running) = self.running.as_ref().filter(|r| r.key == key) else {
            return;
        };
        let cancelling = running.cancelling;
        let turn_id = running.turn_id.clone();
        let agent = running.agent.clone();
        if let Ok(value) = &result {
            chat::record_prompt_usage(self, &turn_id, value);
        }
        match result {
            Ok(value) => {
                let stop_reason = value
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .unwrap_or("end_turn")
                    .to_owned();
                if stop_reason == "cancelled" || cancelling {
                    self.end_running(
                        TurnStatus::Cancelled,
                        Some(stop_reason),
                        None,
                        TurnOutcome::Cancelled,
                    )
                    .await;
                } else {
                    let outcome = TurnOutcome::Completed {
                        stop_reason: stop_reason.clone(),
                    };
                    self.end_running(TurnStatus::Completed, Some(stop_reason), None, outcome)
                        .await;
                }
            }
            Err(_) if cancelling => {
                self.end_running(TurnStatus::Cancelled, None, None, TurnOutcome::Cancelled)
                    .await;
            }
            Err(error) => {
                self.on_attempt_failed(
                    &turn_id,
                    &agent,
                    error.info(),
                    crate::failure::Phase::Prompt,
                )
                .await;
            }
        }
    }

    async fn on_attempt_failed(
        &mut self,
        turn_id: &TurnId,
        agent: &AgentId,
        error: AgentErrorInfo,
        phase: crate::failure::Phase,
    ) {
        let class = self.ctx.classify(agent, &error);
        if let ErrorClass::QuotaExhausted { resets_at } = class {
            // Spec §6.4 failover: record the partial attempt, mark the agent exhausted, re-run the same turn.
            let partial = self
                .running
                .as_ref()
                .map(|r| r.text.clone())
                .unwrap_or_default();
            self.cancel_pending_permissions();
            self.flush_thought().await;
            if let Some(running) = self.running.take() {
                if let Some(connection) = &running.connection {
                    self.ctx
                        .pool
                        .release(&running.agent, connection.generation())
                        .await;
                }
            }
            self.update_last_attempt(
                turn_id,
                "quotaExhausted",
                partial,
                Some(error.message.clone()),
                Some(class.clone()),
            )
            .await;
            if crate::agy_ops::is_agy(&self.ctx, agent)
                && crate::agy_agents::account_of(agent).is_none()
                && crate::agy_ops::rotate_on_quota(&self.ctx).await
            {
                let attempts = self.turn(turn_id).map_or(0, |t| t.attempts.len());
                self.round_base.insert(turn_id.clone(), attempts);
                self.start_turn(turn_id.clone()).await;
                return;
            }
            self.ctx
                .set_quota(
                    agent,
                    QuotaState::Exhausted {
                        resets_at,
                        reason: error.message.clone(),
                    },
                )
                .await;
            self.start_turn(turn_id.clone()).await;
            return;
        }
        // 类型化失败（§9.2.5 R20）：在 TurnFinished 之前发出，UI 据此显示错误横幅与重试提示。
        let trace = self
            .running
            .as_ref()
            .and_then(|r| r.connection.as_ref())
            .and_then(|c| c.stderr_tail());
        let failure = crate::failure::classify(&error, &class, phase, trace);
        self.emit(EventKind::TurnFailure {
            turn_id: turn_id.clone(),
            agent: agent.clone(),
            failure,
        });
        let message = error.message.clone();
        self.end_running(
            TurnStatus::Failed,
            None,
            Some(message.clone()),
            TurnOutcome::Failed {
                error: message,
                class,
            },
        )
        .await;
    }

    async fn end_running(
        &mut self,
        status: TurnStatus,
        stop_reason: Option<String>,
        error: Option<String>,
        outcome: TurnOutcome,
    ) {
        self.cancel_pending_permissions();
        self.flush_thought().await;
        let Some(running) = self.running.take() else {
            return;
        };
        if let Some(connection) = &running.connection {
            self.ctx
                .pool
                .release(&running.agent, connection.generation())
                .await;
        }
        let attempt_status = match status {
            TurnStatus::Completed => "completed",
            TurnStatus::Cancelled => "cancelled",
            _ => "failed",
        };
        let class = match &outcome {
            TurnOutcome::Failed { class, .. } => Some(class.clone()),
            _ => None,
        };
        self.update_last_attempt(
            &running.turn_id,
            attempt_status,
            running.text.clone(),
            error.clone(),
            class,
        )
        .await;
        let marks = self.tool_marks.remove(&running.turn_id).unwrap_or_default();
        if let Some(turn) = self.turn_mut(&running.turn_id) {
            let last_attempt = turn.attempts.last().map_or(1, |a| a.attempt);
            turn.segments = store::build_segments(&running.text, last_attempt, &marks);
            turn.assistant_text = running.text;
        }
        if status == TurnStatus::Completed || status == TurnStatus::Cancelled {
            let seq = self.turn(&running.turn_id).map_or(0, |t| t.seq);
            if let Some(handle) = self.handles.get_mut(&running.agent) {
                handle.last_seq = handle.last_seq.max(seq);
            }
            self.persist_handle(&running.agent).await;
        }
        self.finish(&running.turn_id, status, stop_reason, error, outcome)
            .await;
        self.admit_next().await;
    }

    async fn finish(
        &mut self,
        turn_id: &TurnId,
        status: TurnStatus,
        stop_reason: Option<String>,
        error: Option<String>,
        outcome: TurnOutcome,
    ) {
        let agent = self.turn(turn_id).and_then(|t| t.agent.clone());
        if self.snapshotted.remove(turn_id) {
            // After-snapshot before TurnFinished: a UI that sees TurnFinished can ask turn_diff at once (§15.2).
            take_after_snapshot(&self.ctx, turn_id, &self.row.workspace_path).await;
        }
        self.tool_marks.remove(turn_id);
        if let Some(turn) = self.turn_mut(turn_id) {
            turn.status = status;
            turn.stop_reason = stop_reason;
            turn.error = error;
            turn.finished_at = Some(now_ms());
        }
        self.persist_turn(turn_id).await;
        self.touch().await;
        self.emit(EventKind::TurnFinished {
            turn_id: turn_id.clone(),
            agent,
            outcome,
        });
    }

    // ---- persistence helpers ----------------------------------------------------------------------

    async fn note_failover(&mut self, turn_id: &TurnId, from: &AgentId, to: &AgentId) {
        let resets_at = match self.ctx.quota.get(from) {
            QuotaState::Exhausted { resets_at, .. } => resets_at,
            QuotaState::Ok => None,
        };
        self.append_event(
            turn_id,
            "failover",
            json!({"from": from, "to": to, "resetsAt": resets_at}),
        )
        .await;
    }

    async fn update_last_attempt(
        &mut self,
        turn_id: &TurnId,
        status: &str,
        partial: String,
        error: Option<String>,
        class: Option<ErrorClass>,
    ) {
        let Some(attempt) = self.turn_mut(turn_id).and_then(|t| t.attempts.last_mut()) else {
            return;
        };
        status.clone_into(&mut attempt.status);
        attempt.partial_text = partial;
        attempt.error = error;
        attempt.error_class = class;
        let attempt = attempt.clone();
        self.persist_attempt(turn_id, attempt).await;
    }

    async fn persist_attempt(&self, turn_id: &TurnId, attempt: TurnAttempt) {
        let turn_id = turn_id.clone();
        self.log_store(
            self.ctx
                .store
                .run(move |c| store::upsert_attempt(c, &turn_id, &attempt))
                .await,
        );
    }

    async fn persist_turn(&self, turn_id: &TurnId) {
        let Some(turn) = self.turn(turn_id).cloned() else {
            return;
        };
        self.log_store(
            self.ctx
                .store
                .run(move |c| store::update_turn(c, &turn))
                .await,
        );
    }

    async fn persist_handle(&self, agent: &AgentId) {
        let Some(handle) = self.handles.get(agent) else {
            return;
        };
        let row = HandleRow {
            agent: agent.clone(),
            native: handle.native.clone(),
            last_seq: handle.last_seq,
        };
        let session_id = self.row.id.clone();
        self.log_store(
            self.ctx
                .store
                .run(move |c| store::upsert_handle(c, &session_id, &row))
                .await,
        );
    }

    async fn append_event(&self, turn_id: &TurnId, kind: &'static str, payload: Value) {
        let turn_id = turn_id.clone();
        self.log_store(
            self.ctx
                .store
                .run(move |c| store::append_turn_event(c, &turn_id, kind, &payload))
                .await,
        );
    }

    async fn touch(&mut self) {
        self.row.updated_at = now_ms();
        let row = self.row.clone();
        let result = self
            .ctx
            .store
            .run(move |c| {
                store::update_session(c, &row)?;
                store::summary(c, &row)
            })
            .await;
        match result {
            Ok(summary) => self.emit(EventKind::SessionUpdated { summary }),
            Err(error) => tracing::warn!(%error, "session update failed"),
        }
    }

    fn log_store(&self, result: Result<(), crate::HubError>) {
        if let Err(error) = result {
            tracing::warn!(session = %self.row.id, %error, "store write failed");
        }
    }

    // ---- misc -------------------------------------------------------------------------------------

    fn snapshot(&self) -> SessionDetail {
        let mut turns = self.turns.clone();
        if let Some(running) = &self.running {
            // API request 3 (spec §15.1): the running turn carries the text streamed so far. The actor owns both
            // the text and the event order, so no MessageDelta is lost or counted twice by a UI.
            if let Some(turn) = turns.iter_mut().find(|t| t.id == running.turn_id) {
                turn.assistant_text.clone_from(&running.text);
                if let Some(attempt) = turn.attempts.last_mut() {
                    attempt.partial_text.clone_from(&running.text);
                }
                let last_attempt = turn.attempts.last().map_or(1, |a| a.attempt);
                let marks = self
                    .tool_marks
                    .get(&running.turn_id)
                    .map_or(&[][..], Vec::as_slice);
                turn.segments = store::build_segments(&running.text, last_attempt, marks);
            }
        }
        let mut agents_used: Vec<AgentId> = Vec::new();
        for agent in turns
            .iter()
            .flat_map(|t| t.attempts.iter().map(|a| &a.agent).chain(t.agent.iter()))
        {
            if !agents_used.contains(agent) {
                agents_used.push(agent.clone());
            }
        }
        SessionDetail {
            summary: harness_core_api::SessionSummary {
                id: self.row.id.clone(),
                title: self.row.title.clone(),
                workspace_path: self.row.workspace_path.clone(),
                workspace_key: self.row.workspace_key.clone(),
                active_agent: self.row.active_agent.clone(),
                created_at: self.row.created_at,
                updated_at: self.row.updated_at,
                turn_count: u32::try_from(self.turns.len()).unwrap_or(u32::MAX),
                agents_used,
                pinned: false,
                group_id: None,
                order: 0,
                unread: false,
                forked_from: None,
                imported_from: None,
                parent_id: None,
                worktree: None,
            },
            turns,
            queue: self.queue.iter().cloned().collect(),
            pending_permissions: self.pending.iter().map(|p| p.view.clone()).collect(),
            plan: Vec::new(),
            context_usage: None,
            // 修复说明：快照在 actor 内同步构建，此前本会话的事件都已发布；客户端据此丢弃快照已包含的旧事件
            // （否则重试/回退后重开会话时，排在快照之前的 TurnQueued 会覆盖快照的条目映射，留下一直“Streaming”的行）。
            event_seq: self.ctx.events.last_seq(),
            subagents: Vec::new(),
            cost: None,
            background_tasks: self.background_views(),
        }
    }

    fn position_of(&self, turn_id: &TurnId) -> u32 {
        self.queue.iter().position(|t| t == turn_id).map_or(0, |i| {
            u32::try_from(i).unwrap_or(u32::MAX) + u32::from(self.running.is_some())
        })
    }

    fn turn(&self, turn_id: &TurnId) -> Option<&TurnView> {
        self.turns.iter().find(|t| &t.id == turn_id)
    }

    fn turn_mut(&mut self, turn_id: &TurnId) -> Option<&mut TurnView> {
        self.turns.iter_mut().find(|t| &t.id == turn_id)
    }

    fn emit(&self, kind: EventKind) {
        self.ctx.events.emit(Some(&self.row.id), kind);
    }
}

/// Takes the turn's efore snapshot and stores it (never fails the turn: errors are recorded).
async fn take_before_snapshot(ctx: &Arc<HubCtx>, turn_id: &TurnId, workspace: &str) {
    let tmp = ctx.config.home.join("tmp");
    let result = crate::snapshot::take(&tmp, std::path::Path::new(workspace)).await;
    let (tree, error) = match result {
        Ok(Some(tree)) => (Some(tree), None),
        Ok(None) => (None, Some("workspace is not a git repository".to_owned())),
        Err(error) => {
            tracing::warn!(turn = %turn_id, %error, "turn snapshot (before) failed");
            (None, Some(error))
        }
    };
    let turn_id = turn_id.clone();
    if let Err(error) = ctx
        .store
        .run(move |c| store::set_snapshot_before(c, &turn_id, tree.as_deref(), error.as_deref()))
        .await
    {
        tracing::warn!(%error, "storing turn snapshot failed");
    }
}

/// Takes the turn's fter snapshot when a efore exists.
async fn take_after_snapshot(ctx: &Arc<HubCtx>, turn_id: &TurnId, workspace: &str) {
    let lookup = turn_id.clone();
    let before = ctx
        .store
        .run(move |c| store::load_snapshot(c, &lookup))
        .await
        .ok()
        .flatten()
        .and_then(|row| row.before_tree);
    if before.is_none() {
        return;
    }
    let tmp = ctx.config.home.join("tmp");
    let (tree, error) = match crate::snapshot::take(&tmp, std::path::Path::new(workspace)).await {
        Ok(tree) => (tree, None),
        Err(error) => {
            tracing::warn!(turn = %turn_id, %error, "turn snapshot (after) failed");
            (None, Some(error))
        }
    };
    let turn_id = turn_id.clone();
    if let Err(error) = ctx
        .store
        .run(move |c| store::set_snapshot_after(c, &turn_id, tree.as_deref(), error.as_deref()))
        .await
    {
        tracing::warn!(%error, "storing turn snapshot failed");
    }
}

/// Acquires the agent process and establishes the native session (new, reused or loaded).
async fn drive_setup(
    ctx: &Arc<HubCtx>,
    tx: &ActorTx,
    agent: &AgentId,
    handle: Option<(String, Option<u64>)>,
    cwd: &str,
    plan: PrefPlan,
    session: &harness_core_api::SessionId,
) -> Result<Ready, AgentErrorInfo> {
    crate::agy_agents::ensure_account(ctx, agent)
        .await
        .map_err(|message| AgentErrorInfo {
            code: None,
            message,
            data: None,
        })?;
    let connection = ctx
        .pool
        .acquire(agent)
        .await
        .map_err(|message| AgentErrorInfo {
            code: None,
            message,
            data: None,
        })?;
    let sink: RouteSink = {
        let tx = tx.clone();
        Arc::new(move |inbound| {
            let _ = tx.send(ActorMsg::Inbound(inbound));
        })
    };
    let mcp_servers = ctx
        .mcp_servers(agent, cwd, connection.mcp_capabilities(), session)
        .await;
    // (native, fresh, advertisement, choices already in effect)
    let mut established: Option<(String, bool, Option<Value>, Prefs)> = None;
    if let Some((native, generation)) = handle {
        if generation == Some(connection.generation()) {
            connection.register_route(&native, sink.clone());
            let applied = plan.applied.clone().unwrap_or_default();
            established = Some((native, false, None, applied));
        } else if connection.supports_load() {
            let loaded = connection
                .call(
                    "session/load",
                    json!({"sessionId": native, "cwd": cwd, "mcpServers": mcp_servers}),
                )
                .await;
            if let Ok(result) = loaded {
                // Route only after the load response: replayed history is dropped (spec §7).
                connection.register_route(&native, sink.clone());
                established = Some((native, false, Some(result), Prefs::default()));
            }
        }
    }
    let (native, fresh, advertised, applied) = match established {
        Some(found) => found,
        None => match connection
            .call(
                "session/new",
                json!({"cwd": cwd, "mcpServers": mcp_servers}),
            )
            .await
        {
            Ok(result) => {
                let Some(native) = result.get("sessionId").and_then(Value::as_str) else {
                    ctx.pool.release(agent, connection.generation()).await;
                    return Err(AgentErrorInfo {
                        code: None,
                        message: "session/new returned no sessionId".into(),
                        data: None,
                    });
                };
                let native = native.to_owned();
                connection.register_route(&native, sink);
                (native, true, Some(result), Prefs::default())
            }
            Err(error) => {
                ctx.pool.release(agent, connection.generation()).await;
                return Err(error.info());
            }
        },
    };
    // Defaults only for a fresh native session; a loaded one keeps its state except the session's own choices.
    let target = if fresh {
        plan.own.over_defaults(plan.defaults.as_ref())
    } else {
        plan.own
    };
    let applied = apply_prefs(&connection, &native, &target, applied).await;
    Ok(Ready {
        agent: agent.clone(),
        connection,
        native,
        fresh,
        advertised,
        applied,
    })
}

/// Sends the set calls that bring the native session from `applied` to `target`, awaiting each before the
/// prompt is sent. A refused call is logged and stays pending (retried before the next turn).
async fn apply_prefs(
    connection: &Arc<AcpConnection>,
    native: &str,
    target: &Prefs,
    mut applied: Prefs,
) -> Prefs {
    for (method, params) in target.calls_from(&applied, native) {
        match connection.call(method, params.clone()).await {
            Ok(_) => {
                if let Some(mode) = params.get("modeId").and_then(Value::as_str) {
                    applied.mode = Some(mode.to_owned());
                } else if let Some(model) = params.get("modelId").and_then(Value::as_str) {
                    applied.model = Some(model.to_owned());
                } else if let (Some(id), Some(value)) = (
                    params.get("configId").and_then(Value::as_str),
                    params.get("value").and_then(Value::as_str),
                ) {
                    applied.config.insert(id.to_owned(), value.to_owned());
                }
            }
            Err(error) => {
                // Newer agents (Claude Code ACP) list their models only as the `model` config option.
                if let Some(model) = params.get("modelId").and_then(Value::as_str) {
                    let retry = json!({"sessionId": native, "configId": "model", "value": model});
                    if connection
                        .call("session/set_config_option", retry)
                        .await
                        .is_ok()
                    {
                        applied.model = Some(model.to_owned());
                        continue;
                    }
                }
                tracing::warn!(method, code = error.code, message = %error.message, "agent refused option");
            }
        }
    }
    applied
}

fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

/// What the agent already saw of a tool call: its text content, or the raw output as fallback.
///
/// The output is clipped here, before it is kept in a `ContextTurn`: a tool result can be well over a
/// megabyte and `prompt_blocks` builds one digest per call of every turn, so keeping the full text would
/// copy it repeatedly only for [`render_tools`](crate::catchup::render_tools) to cut it again.
fn tool_digests(calls: &[ToolCallView]) -> Vec<ToolDigest> {
    calls
        .iter()
        .map(|call| ToolDigest {
            title: call.title.clone(),
            input: call.raw_input.clone().unwrap_or_default(),
            output: clip_head_tail(&tool_output(call), TOOL_OUTPUT_CHARS),
        })
        .collect()
}

fn tool_output(call: &ToolCallView) -> String {
    let mut parts: Vec<String> = Vec::new();
    for item in &call.content {
        match item {
            ToolContent::Text { text } => parts.push(text.clone()),
            ToolContent::Diff { path, new_text, .. } => {
                parts.push(format!("(diff {path}, {} lines)", new_text.lines().count()));
            }
            ToolContent::Terminal { terminal_id } => {
                parts.push(format!("(terminal {terminal_id})"));
            }
        }
    }
    if parts.is_empty() {
        return call.raw_output.clone().unwrap_or_default();
    }
    parts.join("\n")
}

fn tool_view(value: &Value) -> ToolCallView {
    ToolCallView {
        tool_call_id: value
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        title: value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        kind: value.get("kind").and_then(Value::as_str).map(str::to_owned),
        status: value
            .get("status")
            .and_then(Value::as_str)
            .and_then(|s| match s {
                "pending" => Some(ToolCallStatus::Pending),
                "in_progress" => Some(ToolCallStatus::InProgress),
                "completed" => Some(ToolCallStatus::Completed),
                "failed" => Some(ToolCallStatus::Failed),
                _ => None,
            }),
        raw_input: value.get("rawInput").map(Value::to_string),
        content: chat::tool_content(value),
        locations: chat::tool_locations(value),
        raw_output: value.get("rawOutput").map(|v| {
            chat::capped(match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
        }),
    }
}

fn permission_option(value: &Value) -> Option<PermissionOption> {
    let kind = match value.get("kind").and_then(Value::as_str)? {
        "allow_once" => PermissionOptionKind::AllowOnce,
        "allow_always" => PermissionOptionKind::AllowAlways,
        "reject_once" => PermissionOptionKind::RejectOnce,
        "reject_always" => PermissionOptionKind::RejectAlways,
        _ => return None,
    };
    Some(PermissionOption {
        option_id: value.get("optionId").and_then(Value::as_str)?.to_owned(),
        name: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        kind,
    })
}
