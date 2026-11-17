//! Session import, read-only on the source side (decision 14A, phase-9-parity.md §9.2.1). `harness-import` parses;
//! the hub writes each imported session in one store transaction. No agent runs.

use harness_agent_config::AgentKind;
use harness_core::WorkspaceKey;
use harness_core_api::{
    ApiError, ApiResult, EventKind, ImportCandidate, ImportReport, ImportRequest, ImportScan,
    ImportSource, SessionId, ToolCallStatus, ToolCallView, TurnId, TurnStatus, TurnView,
};
use harness_import::ImportedSession;

use crate::hub::Hub;
use crate::store::{self, HandleRow, SessionRow};
use crate::store_chat;
use crate::util::{new_id, now_ms};

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl Hub {
    pub(crate) async fn do_import_scan(
        &self,
        request: ImportScan,
    ) -> ApiResult<Vec<ImportCandidate>> {
        self.check_open()?;
        let sessions = harness_import::scan(&self.ctx.config.import_roots, &request.sources).await;
        let known = self.ctx.store.run(store_chat::imported_keys).await?;
        let native = self.ctx.store.run(store_chat::native_session_ids).await?;
        Ok(sessions
            .into_iter()
            .filter(|s| {
                s.source == ImportSource::Zcode
                    || !native.contains(&s.external_id)
                    || known.contains(&s.key())
            })
            .map(|s| ImportCandidate {
                imported: known.contains(&s.key()),
                source: s.source,
                external_id: s.external_id,
                title: s.title,
                workspace_path: s.workspace_path,
                turn_count: count(s.turns.len()),
                updated_at: s.updated_at,
            })
            .collect())
    }

    pub(crate) async fn do_import(&self, request: ImportRequest) -> ApiResult<ImportReport> {
        self.check_open()?;
        let order = self.ctx.fallback_order();
        let preferred = match request.source {
            ImportSource::ClaudeCode => Some(("claude-code", AgentKind::ClaudeCode)),
            ImportSource::Codex => Some(("codex", AgentKind::Codex)),
            ImportSource::Zcode => None,
        };
        let native_agent = preferred.and_then(|(id, kind)| {
            order
                .iter()
                .find(|a| a.as_str() == id)
                .or_else(|| order.iter().find(|a| self.agent_kind(a) == Some(kind)))
                .cloned()
        });
        let resumable = native_agent.is_some();
        let agent = native_agent
            .or_else(|| order.first().cloned())
            .ok_or_else(|| ApiError::AgentUnavailable("no agents configured".into()))?;
        let sessions = harness_import::scan(&self.ctx.config.import_roots, &[request.source]).await;
        let mut known = self.ctx.store.run(store_chat::imported_keys).await?;
        let mut report = ImportReport::default();
        for external_id in request.external_ids {
            let key = harness_import::key(request.source, &external_id);
            if known.contains(&key) {
                report
                    .skipped
                    .push((external_id, "already imported".into()));
                continue;
            }
            let Some(session) = sessions
                .iter()
                .find(|s| s.external_id == external_id)
                .cloned()
            else {
                report.skipped.push((external_id, "not found".into()));
                continue;
            };
            let Ok(workspace_key) = WorkspaceKey::resolve(None, &session.workspace_path) else {
                report
                    .skipped
                    .push((external_id, "the session has no workspace path".into()));
                continue;
            };
            let row = SessionRow {
                id: SessionId::new(new_id("ses")),
                title: session.title.clone(),
                workspace_path: session.workspace_path.clone(),
                workspace_key: workspace_key.as_str().to_owned(),
                active_agent: agent.clone(),
                created_at: if session.created_at > 0 {
                    session.created_at
                } else {
                    now_ms()
                },
                updated_at: if session.updated_at > 0 {
                    session.updated_at
                } else {
                    now_ms()
                },
            };
            let handle = resumable.then(|| HandleRow {
                agent: agent.clone(),
                native: session.external_id.clone(),
                last_seq: count(session.turns.len()),
            });
            let stored_key = key.clone();
            let summary = self
                .ctx
                .store
                .run(move |c| insert_imported(c, &row, &session, &stored_key, handle.as_ref()))
                .await?;
            known.push(key);
            report.imported.push(summary);
        }
        if !report.imported.is_empty() {
            self.ctx.events.emit(None, EventKind::SessionsChanged);
        }
        Ok(report)
    }
}

/// One transaction: the session, its finished turns and tool titles, `imported_from`, read state.
fn insert_imported(
    c: &rusqlite::Connection,
    row: &SessionRow,
    session: &ImportedSession,
    key: &str,
    handle: Option<&HandleRow>,
) -> Result<harness_core_api::SessionSummary, crate::HubError> {
    let tx = c.unchecked_transaction()?;
    store::insert_session(&tx, row)?;
    for (index, turn) in session.turns.iter().enumerate() {
        let at = if turn.created_at > 0 {
            turn.created_at
        } else {
            row.created_at
        };
        let view = TurnView {
            id: TurnId::new(new_id("turn")),
            seq: count(index + 1),
            user_text: turn.user_text.clone(),
            agent: None,
            assistant_text: turn.assistant_text.clone(),
            status: TurnStatus::Completed,
            stop_reason: Some("imported".into()),
            error: None,
            attempts: Vec::new(),
            tool_calls: Vec::new(),
            permissions: Vec::new(),
            created_at: at,
            finished_at: Some(at),
            segments: Vec::new(),
            context: Vec::new(),
            plan: Vec::new(),
            usage: None,
            context_usage: None,
            origin: None,
            thoughts: Vec::new(),
        };
        store::insert_turn(&tx, &row.id, &view, None)?;
        store::update_turn(&tx, &view)?;
        for (n, title) in turn.tools.iter().enumerate() {
            let tool = ToolCallView {
                tool_call_id: format!("imported-{n}"),
                title: title.clone(),
                kind: None,
                status: Some(ToolCallStatus::Completed),
                raw_input: None,
                content: Vec::new(),
                locations: Vec::new(),
                raw_output: None,
            };
            store::append_turn_event(&tx, &view.id, "tool_call", &serde_json::to_value(&tool)?)?;
        }
    }
    if let Some(handle) = handle {
        store::upsert_handle(&tx, &row.id, handle)?;
    }
    store_chat::set_imported_from(&tx, &row.id, key)?;
    store_chat::mark_read(&tx, &row.id, now_ms())?;
    let summary = store::summary(&tx, row)?;
    tx.commit()?;
    Ok(summary)
}
