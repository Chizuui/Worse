//! Agent process pool (spec §10): one process per agent id, shared by all sessions; spawned on demand,
//! stopped after the idle timeout. Single owner of agent process lifecycle.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use harness_core_api::{AgentId, AgentRuntimeState, EventKind};
use tokio::sync::Mutex;

use crate::acp::AcpConnection;
use crate::agents::{resolve_program, AgentSpec};
use crate::bus::EventBus;

struct Slot {
    connection: Arc<AcpConnection>,
    busy: u32,
    idle_epoch: u64,
}

struct Shared {
    specs: HashMap<AgentId, AgentSpec>,
    idle: Duration,
    events: EventBus,
    slots: Mutex<HashMap<AgentId, Slot>>,
    next_generation: AtomicU64,
    shutting_down: AtomicBool,
    /// Agents whose idle-stopped process is still shutting down (slot already removed).
    stopping: std::sync::Mutex<HashMap<AgentId, u32>>,
}

/// Cloneable handle to the pool.
#[derive(Clone)]
pub struct AgentPool {
    shared: Arc<Shared>,
}

impl AgentPool {
    /// Pool over the configured agents.
    #[must_use]
    pub fn new(specs: &[AgentSpec], idle: Duration, events: EventBus) -> Self {
        Self {
            shared: Arc::new(Shared {
                specs: specs.iter().map(|s| (s.id.clone(), s.clone())).collect(),
                idle,
                events,
                slots: Mutex::new(HashMap::new()),
                next_generation: AtomicU64::new(1),
                shutting_down: AtomicBool::new(false),
                stopping: std::sync::Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Returns a live, initialized connection and marks it busy. Pair with [`AgentPool::release`].
    ///
    /// # Errors
    ///
    /// Unknown/not-installed agent, spawn or `initialize` failure (message for the transcript).
    pub async fn acquire(&self, agent: &AgentId) -> Result<Arc<AcpConnection>, String> {
        if self.shared.shutting_down.load(Ordering::SeqCst) {
            return Err("hub is shutting down".into());
        }
        let spec = self
            .shared
            .specs
            .get(agent)
            .ok_or_else(|| format!("unknown agent {agent}"))?;
        let mut slots = self.shared.slots.lock().await;
        if let Some(slot) = slots.get_mut(agent) {
            if !slot.connection.is_closed() {
                slot.busy += 1;
                slot.idle_epoch += 1;
                let pid = slot.connection.pid();
                let connection = Arc::clone(&slot.connection);
                drop(slots);
                self.emit_runtime(agent, AgentRuntimeState::Busy, pid);
                return Ok(connection);
            }
            slots.remove(agent);
        }
        let program = resolve_program(&spec.program).ok_or_else(|| {
            format!(
                "{} is not installed: {}",
                spec.display_name, spec.install_hint
            )
        })?;
        let generation = self.shared.next_generation.fetch_add(1, Ordering::SeqCst);
        self.emit_runtime(agent, AgentRuntimeState::Starting, None);
        let on_exit = {
            let pool = self.clone();
            let agent = agent.clone();
            Box::new(move || {
                tokio::spawn(async move { pool.on_process_exit(&agent, generation).await });
            })
        };
        let connection = AcpConnection::spawn(&program, &spec.args, &spec.env, generation, on_exit)
            .map_err(|e| format!("failed to start {}: {e}", spec.display_name))?;
        if let Err(error) = connection.initialize().await {
            connection.shutdown().await;
            self.emit_runtime(agent, AgentRuntimeState::Crashed, None);
            return Err(format!("{} initialize failed: {error}", spec.display_name));
        }
        slots.insert(
            agent.clone(),
            Slot {
                connection: Arc::clone(&connection),
                busy: 1,
                idle_epoch: 0,
            },
        );
        drop(slots);
        self.emit_runtime(agent, AgentRuntimeState::Busy, connection.pid());
        Ok(connection)
    }

    pub async fn acquire_live(
        &self,
        agent: &AgentId,
        generation: u64,
    ) -> Option<Arc<AcpConnection>> {
        let mut slots = self.shared.slots.lock().await;
        let slot = slots.get_mut(agent)?;
        if slot.connection.generation() != generation || slot.connection.is_closed() {
            return None;
        }
        slot.busy += 1;
        slot.idle_epoch += 1;
        Some(Arc::clone(&slot.connection))
    }

    /// Ends one use of a connection; at zero users arms the idle stop.
    pub async fn release(&self, agent: &AgentId, generation: u64) {
        let mut slots = self.shared.slots.lock().await;
        let Some(slot) = slots.get_mut(agent) else {
            return;
        };
        if slot.connection.generation() != generation {
            return;
        }
        slot.busy = slot.busy.saturating_sub(1);
        if slot.busy > 0 {
            return;
        }
        slot.idle_epoch += 1;
        let epoch = slot.idle_epoch;
        let pid = slot.connection.pid();
        drop(slots);
        self.emit_runtime(agent, AgentRuntimeState::Idle, pid);
        let pool = self.clone();
        let agent = agent.clone();
        let idle = self.shared.idle;
        tokio::spawn(async move {
            // Idle stop is a product rule (spec §10), not a sync workaround: stop only if the epoch is unchanged.
            tokio::time::sleep(idle).await;
            pool.stop_if_idle(&agent, generation, epoch).await;
        });
    }

    async fn stop_if_idle(&self, agent: &AgentId, generation: u64, epoch: u64) {
        let mut slots = self.shared.slots.lock().await;
        let still_idle = slots.get(agent).is_some_and(|slot| {
            slot.connection.generation() == generation && slot.busy == 0 && slot.idle_epoch == epoch
        });
        if !still_idle {
            return;
        }
        let Some(slot) = slots.remove(agent) else {
            return;
        };
        // 修复说明：原先先移除 slot 再 await shutdown（最长 SHUTDOWN_GRACE），这段时间里 runtime_state 已报告
        // Stopped 而进程仍存活（Agents 概览会显示错误状态）。现在 shutdown 完成前记为 stopping，runtime_state 仍报告
        // Idle。（phase8-core 门禁的偶发残留报告另有原因，见 harness-core-gate idle 检查的说明。）
        self.mark_stopping(agent, true);
        drop(slots);
        let pid = slot.connection.pid();
        slot.connection.shutdown().await;
        self.mark_stopping(agent, false);
        self.emit_runtime(agent, AgentRuntimeState::Stopped, pid);
    }

    fn mark_stopping(&self, agent: &AgentId, on: bool) {
        if let Ok(mut stopping) = self.shared.stopping.lock() {
            if on {
                *stopping.entry(agent.clone()).or_default() += 1;
            } else if let Some(n) = stopping.get_mut(agent) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    stopping.remove(agent);
                }
            }
        }
    }

    fn is_stopping(&self, agent: &AgentId) -> bool {
        self.shared
            .stopping
            .lock()
            .is_ok_and(|stopping| stopping.contains_key(agent))
    }

    async fn on_process_exit(&self, agent: &AgentId, generation: u64) {
        let mut slots = self.shared.slots.lock().await;
        let ours = slots
            .get(agent)
            .is_some_and(|slot| slot.connection.generation() == generation);
        if !ours {
            return; // intentional stop already removed the slot
        }
        let slot = slots.remove(agent);
        drop(slots);
        let pid = slot.as_ref().and_then(|s| s.connection.pid());
        if let Some(slot) = slot {
            slot.connection.shutdown().await;
        }
        self.emit_runtime(agent, AgentRuntimeState::Crashed, pid);
    }

    /// Current process state of an agent.
    pub async fn runtime_state(&self, agent: &AgentId) -> AgentRuntimeState {
        let slots = self.shared.slots.lock().await;
        match slots.get(agent) {
            Some(slot) if slot.connection.is_closed() => AgentRuntimeState::Crashed,
            Some(slot) if slot.busy > 0 => AgentRuntimeState::Busy,
            Some(_) => AgentRuntimeState::Idle,
            None if self.is_stopping(agent) => AgentRuntimeState::Idle,
            None => AgentRuntimeState::Stopped,
        }
    }

    /// `agentInfo` (name, version) of the live process of an agent, if any.
    pub async fn agent_info(&self, agent: &AgentId) -> Option<(Option<String>, Option<String>)> {
        let slots = self.shared.slots.lock().await;
        slots
            .get(agent)
            .filter(|slot| !slot.connection.is_closed())
            .map(|slot| slot.connection.agent_info())
    }

    /// Stops the process of one agent now, busy or not (its in-flight calls fail).
    pub async fn stop_agent(&self, agent: &AgentId) {
        let Some(slot) = self.shared.slots.lock().await.remove(agent) else {
            return;
        };
        let pid = slot.connection.pid();
        slot.connection.shutdown().await;
        self.emit_runtime(agent, AgentRuntimeState::Stopped, pid);
    }

    /// Stops every agent process (hub shutdown).
    pub async fn shutdown_all(&self) {
        self.shared.shutting_down.store(true, Ordering::SeqCst);
        let slots: Vec<(AgentId, Slot)> = self.shared.slots.lock().await.drain().collect();
        for (agent, slot) in slots {
            let pid = slot.connection.pid();
            slot.connection.shutdown().await;
            self.emit_runtime(&agent, AgentRuntimeState::Stopped, pid);
        }
    }

    fn emit_runtime(&self, agent: &AgentId, state: AgentRuntimeState, pid: Option<u32>) {
        self.shared.events.emit(
            None,
            EventKind::AgentRuntime {
                agent: agent.clone(),
                state,
                pid,
            },
        );
    }
}
