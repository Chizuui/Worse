//! Subagent registry and live status tracker (phase-9-parity.md §9.8).
//!
//! Single owner of the `subagents` table and the only emitter of `EventKind::SubagentUpdated`. Status is derived
//! from the child session's own hub events by [`track`].

use std::collections::HashMap;
use std::sync::{Mutex, Weak};

use harness_core_api::{
    EventKind, RecvError, ScopeView, SessionId, SubagentStatus, SubagentView, ToolCallView,
    TurnOutcome,
};
use rusqlite::{params, Connection};

use crate::error::HubError;
use crate::hub::HubCtx;
use crate::util::now_ms;

/// Characters of the answer kept as `result_preview`.
pub(crate) const PREVIEW_CHARS: usize = 300;

/// Loopback endpoint of the delegate server (address + per-run token).
#[derive(Clone)]
pub(crate) struct Endpoint {
    pub(crate) addr: String,
    pub(crate) token: String,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 令牌不得出现在日志里（§9.8 Transport）。
        f.debug_struct("Endpoint")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

/// In-memory copy of the `subagents` table (child id → view).
#[derive(Default)]
pub(crate) struct Registry {
    views: Mutex<HashMap<SessionId, SubagentView>>,
    pub(crate) endpoint: std::sync::OnceLock<Endpoint>,
    /// Notes, report marks and path claims (§9.8.1).
    pub(crate) coord: crate::delegate_coord::Coordination,
}

impl Registry {
    /// Loads every row; rows still active become `interrupted` (the hub that ran them is gone).
    pub(crate) fn load(c: &Connection) -> Result<Self, HubError> {
        let mut statement = c.prepare("SELECT json FROM subagents")?;
        let rows = statement
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut views = HashMap::new();
        for json in rows {
            let Ok(mut view) = serde_json::from_str::<SubagentView>(&json) else {
                continue;
            };
            if view.status.is_active() {
                view.status = SubagentStatus::Interrupted;
                view.finished_at.get_or_insert(view.started_at);
                save(c, &view)?;
            }
            views.insert(view.session_id.clone(), view);
        }
        Ok(Self {
            views: Mutex::new(views),
            endpoint: std::sync::OnceLock::new(),
            coord: crate::delegate_coord::Coordination::default(),
        })
    }

    pub(crate) fn get(&self, child: &SessionId) -> Option<SubagentView> {
        self.views.lock().ok().and_then(|v| v.get(child).cloned())
    }

    pub(crate) fn is_child(&self, session: &SessionId) -> bool {
        self.views.lock().is_ok_and(|v| v.contains_key(session))
    }

    /// Subagents of `parent`, newest first.
    pub(crate) fn of_parent(&self, parent: &SessionId) -> Vec<SubagentView> {
        let mut out: Vec<SubagentView> = self
            .views
            .lock()
            .map(|v| {
                v.values()
                    .filter(|s| &s.parent_id == parent)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        out
    }

    /// Active direct subagents of `parent` (their own subagents are not counted).
    pub(crate) fn active_children(&self, parent: &SessionId) -> usize {
        self.views.lock().map_or(0, |v| {
            v.values()
                .filter(|s| s.status.is_active() && &s.parent_id == parent)
                .count()
        })
    }

    fn insert(&self, view: SubagentView) {
        if let Ok(mut views) = self.views.lock() {
            views.insert(view.session_id.clone(), view);
        }
    }
}

pub(crate) const TABLE: &str = "CREATE TABLE IF NOT EXISTS subagents(child_id TEXT PRIMARY KEY, parent_id TEXT NOT NULL, json TEXT NOT NULL);";

pub(crate) fn save(c: &Connection, view: &SubagentView) -> Result<(), HubError> {
    c.execute(
        "INSERT INTO subagents(child_id,parent_id,json) VALUES(?1,?2,?3)
         ON CONFLICT(child_id) DO UPDATE SET json=?3",
        params![
            view.session_id.as_str(),
            view.parent_id.as_str(),
            serde_json::to_string(view)?
        ],
    )?;
    Ok(())
}

/// Parent of a subagent session (for `SessionSummary.parent_id`).
pub(crate) fn parent_of(c: &Connection, child: &SessionId) -> Result<Option<SessionId>, HubError> {
    use rusqlite::OptionalExtension;
    Ok(c.query_row(
        "SELECT parent_id FROM subagents WHERE child_id=?1",
        params![child.as_str()],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(SessionId::new))
}

impl HubCtx {
    /// Single write path of a subagent view: memory, store, `SubagentUpdated` on the parent.
    pub(crate) async fn put_subagent(&self, view: SubagentView) {
        if self.subagents.get(&view.session_id).as_ref() == Some(&view) {
            return;
        }
        self.subagents.insert(view.clone());
        let stored = view.clone();
        if let Err(error) = self.store.run(move |c| save(c, &stored)).await {
            tracing::warn!(%error, "saving subagent failed");
        }
        let parent = view.parent_id.clone();
        self.events
            .emit(Some(&parent), EventKind::SubagentUpdated { subagent: view });
    }
}

/// First `PREVIEW_CHARS` characters of an answer, whitespace collapsed.
pub(crate) fn preview(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(PREVIEW_CHARS).collect();
    if collapsed.chars().count() > PREVIEW_CHARS {
        out.push('…');
    }
    out
}

/// Follows hub events and keeps every subagent's status, agent and activity current. Holds only a weak
/// reference so the hub can drop; ends when the event bus closes.
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
        let Some(child) = event.session_id.as_ref() else {
            continue;
        };
        if let EventKind::TurnFinished { turn_id, .. } = &event.kind {
            // §9.8.1：会话一轮结束即释放它的路径占用；子会话的结束轮次登记为待汇报，供 subagent_wait 取走。
            ctx.subagents.coord.release(child, &[]);
            if let Some(view) = ctx.subagents.get(child) {
                ctx.subagents
                    .coord
                    .turn_finished(&view.parent_id, child, turn_id);
            }
        }
        let Some(current) = ctx.subagents.get(child) else {
            continue;
        };
        if let Some(next) = advance(current, &event.kind) {
            ctx.put_subagent(next).await;
        }
    }
}

/// Status change of one subagent for one of its session's events (`None` = no change).
fn advance(mut view: SubagentView, kind: &EventKind) -> Option<SubagentView> {
    match kind {
        EventKind::TurnStarted { agent, .. } => {
            if !view.status.is_active() {
                // 用户直接在子会话里继续对话：视为新一轮，重新计时。
                view.started_at = now_ms();
                view.finished_at = None;
                view.activity.clear();
            }
            view.status = SubagentStatus::Running;
            view.agent = agent.clone();
        }
        EventKind::ToolCall { tool, .. } | EventKind::ToolCallUpdated { tool, .. } => {
            let title = tool.title.trim();
            if title.is_empty() || title == view.activity {
                return None;
            }
            view.activity = title.chars().take(160).collect();
        }
        EventKind::PermissionRequested { .. } => view.status = SubagentStatus::NeedsApproval,
        EventKind::PermissionResolved { .. } if view.status == SubagentStatus::NeedsApproval => {
            view.status = SubagentStatus::Running;
        }
        EventKind::TurnFinished { outcome, .. } => {
            view.status = match outcome {
                TurnOutcome::Completed { .. } => SubagentStatus::Done,
                TurnOutcome::Cancelled => SubagentStatus::Cancelled,
                TurnOutcome::Failed { .. } => SubagentStatus::Failed,
            };
            view.finished_at = Some(now_ms());
        }
        _ => return None,
    }
    Some(view)
}

/// A subagent's permissions: workspace paths, tool title fragments, network and a deadline. Empty paths or
/// tools mean "no restriction", so a scope only ever narrows the parent's (§9.8.2).
pub(crate) fn narrow(
    parent: Option<&ScopeView>,
    request: &ScopeView,
    now: i64,
) -> Result<ScopeView, String> {
    let parent_paths = parent.map(|p| &p.paths).filter(|p| !p.is_empty());
    let paths = match parent_paths {
        None => request.paths.clone(),
        Some(allowed) => {
            let wider = |path: &str| !allowed.iter().any(|a| within(a, path));
            if request.paths.is_empty() {
                return Err(format!(
                    "paths are wider than the parent scope, which allows only: {}",
                    allowed.join(", ")
                ));
            }
            if let Some(path) = request.paths.iter().find(|p| wider(p.as_str())) {
                return Err(format!(
                    "{path} is wider than the parent scope, which allows only: {}",
                    allowed.join(", ")
                ));
            }
            request.paths.clone()
        }
    };
    let parent_tools = parent.map(|p| &p.tools).filter(|t| !t.is_empty());
    let tools = match parent_tools {
        None => request.tools.clone(),
        Some(allowed) => {
            if request.tools.is_empty() {
                return Err(format!(
                    "tools are wider than the parent scope, which allows only: {}",
                    allowed.join(", ")
                ));
            }
            let wider = request
                .tools
                .iter()
                .find(|t| !allowed.iter().any(|a| a.eq_ignore_ascii_case(t.as_str())));
            if let Some(tool) = wider {
                return Err(format!(
                    "{tool} is wider than the parent scope, which allows only: {}",
                    allowed.join(", ")
                ));
            }
            request.tools.clone()
        }
    };
    if parent.is_some_and(|p| !p.network) && request.network {
        return Err("network is off in the parent scope".to_owned());
    }
    let network = request.network;
    let expires_at = match (parent.and_then(|p| p.expires_at), request.expires_at) {
        (Some(limit), Some(want)) if want > limit => {
            let mins = (limit - now).max(0) / 60_000;
            return Err(format!(
                "the deadline is past the parent scope, which ends in {mins} min"
            ));
        }
        (Some(limit), None) => Some(limit),
        (_, want) => want,
    };
    Ok(ScopeView {
        paths,
        tools,
        network,
        expires_at,
    })
}

/// Whether `path` (already workspace-relative) lies inside `allowed`.
fn within(allowed: &str, path: &str) -> bool {
    let allowed = allowed.to_lowercase();
    let path = path.to_lowercase();
    path == allowed || path.starts_with(&format!("{allowed}/"))
}

/// Whether the scope lets the subagent touch `path`.
pub(crate) fn allows_path(scope: &ScopeView, path: &str) -> bool {
    scope.paths.is_empty() || scope.paths.iter().any(|allowed| within(allowed, path))
}

/// Why `scope` blocks the tool call, naming the rule that blocks it (`None` = allowed).
pub(crate) fn blocked(
    scope: &ScopeView,
    tool: &ToolCallView,
    paths: &[String],
    now: i64,
) -> Option<String> {
    if scope.expires_at.is_some_and(|deadline| now >= deadline) {
        return Some("its scope deadline has passed".to_owned());
    }
    if !scope.tools.is_empty() {
        let title = tool.title.to_lowercase();
        if !scope
            .tools
            .iter()
            .any(|t| title.contains(&t.to_lowercase()))
        {
            return Some(format!(
                "its scope allows only these tools: {}",
                scope.tools.join(", ")
            ));
        }
    }
    if !scope.network && uses_network(tool) {
        return Some("network access is off in its scope".to_owned());
    }
    if let Some(path) = paths.iter().find(|path| !allows_path(scope, path.as_str())) {
        return Some(format!(
            "{path} is outside its scope, which allows only: {}",
            scope.paths.join(", ")
        ));
    }
    None
}

/// Whether the tool call looks like it reaches the network.
fn uses_network(tool: &ToolCallView) -> bool {
    const WORDS: [&str; 8] = [
        "http", "web", "fetch", "curl", "wget", "download", "network", "browser",
    ];
    let haystack = format!(
        "{} {}",
        tool.title,
        tool.raw_input.as_deref().unwrap_or_default()
    )
    .to_lowercase();
    WORDS.iter().any(|word| haystack.contains(word))
}

/// One line describing the scope for the task preamble and the subagent card.
pub(crate) fn describe(scope: &ScopeView, now: i64) -> String {
    let mut parts = vec![if scope.paths.is_empty() {
        "whole workspace".to_owned()
    } else {
        scope.paths.join(", ")
    }];
    if !scope.tools.is_empty() {
        parts.push(scope.tools.join(", "));
    }
    if !scope.network {
        parts.push("no network".to_owned());
    }
    if let Some(deadline) = scope.expires_at {
        let mins = (deadline - now).max(0) / 60_000;
        parts.push(if mins >= 1 {
            format!("{mins} min left")
        } else {
            "ending now".to_owned()
        });
    }
    parts.join(" · ")
}

/// `path` relative to `workspace` (unchanged when it lies elsewhere).
pub(crate) fn relative_to(workspace: &str, path: &str) -> String {
    let path = path.replace('\\', "/");
    let root = workspace.replace('\\', "/");
    let root = root.trim_end_matches('/');
    if root.is_empty() {
        return path;
    }
    let lower_path = path.to_lowercase();
    let lower_root = root.to_lowercase();
    if lower_path == lower_root {
        return String::new();
    }
    match lower_path.strip_prefix(&format!("{lower_root}/")) {
        Some(rest) => path[path.len() - rest.len()..].to_owned(),
        None => path,
    }
}

impl HubCtx {
    /// Scope of a subagent session; `None` for a top-level session (unrestricted).
    pub(crate) fn scope_of(&self, session: &SessionId) -> Option<ScopeView> {
        self.subagents.get(session).and_then(|view| view.scope)
    }
}

#[cfg(test)]
mod tests {
    #![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

    use super::*;

    fn tool(title: &str) -> ToolCallView {
        ToolCallView {
            tool_call_id: String::new(),
            title: title.to_owned(),
            kind: None,
            status: Some(harness_core_api::ToolCallStatus::Pending),
            raw_input: None,
            content: Vec::new(),
            locations: Vec::new(),
            raw_output: None,
        }
    }

    fn scope(paths: &[&str], tools: &[&str], network: bool, minutes: i64, now: i64) -> ScopeView {
        ScopeView {
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            tools: tools.iter().map(|t| (*t).to_owned()).collect(),
            network,
            expires_at: (minutes > 0).then_some(now + minutes * 60_000),
        }
    }

    #[test]
    fn a_narrower_scope_is_accepted() {
        let parent = scope(&["crates"], &["read"], false, 30, 0);
        let child = scope(&["crates/hub"], &["read"], false, 5, 0);
        let narrowed = narrow(Some(&parent), &child, 0).unwrap();
        assert_eq!(narrowed.paths, vec!["crates/hub".to_owned()]);
        assert_eq!(narrowed.expires_at, Some(300_000));
    }

    #[test]
    fn an_empty_parent_scope_accepts_any_child_scope() {
        let child = scope(&["docs"], &["write"], true, 10, 0);
        let narrowed = narrow(None, &child, 0).unwrap();
        assert_eq!(narrowed, child);
    }

    #[test]
    fn a_child_without_paths_cannot_widen_a_restricted_parent() {
        let parent = scope(&["crates"], &[], true, 0, 0);
        let child = scope(&[], &[], true, 0, 0);
        let error = narrow(Some(&parent), &child, 0).unwrap_err();
        assert!(error.contains("crates"), "{error}");
    }

    #[test]
    fn a_sibling_path_is_wider_than_the_parent() {
        let parent = scope(&["crates/hub"], &[], true, 0, 0);
        let child = scope(&["docs"], &[], true, 0, 0);
        assert!(narrow(Some(&parent), &child, 0).is_err());
    }

    #[test]
    fn tools_and_network_can_only_narrow() {
        let parent = scope(&[], &["read", "search"], false, 0, 0);
        assert!(narrow(Some(&parent), &scope(&[], &["read"], false, 0, 0), 0).is_ok());
        assert!(narrow(Some(&parent), &scope(&[], &["write"], false, 0, 0), 0).is_err());
        assert!(narrow(Some(&parent), &scope(&[], &[], false, 0, 0), 0).is_err());
        assert!(narrow(Some(&parent), &scope(&[], &["read"], true, 0, 0), 0).is_err());
    }

    #[test]
    fn a_longer_deadline_is_rejected_and_a_missing_one_inherits() {
        let parent = scope(&[], &[], true, 10, 0);
        assert!(narrow(Some(&parent), &scope(&[], &[], true, 20, 0), 0).is_err());
        let inherited = narrow(Some(&parent), &scope(&[], &[], true, 0, 0), 0).unwrap();
        assert_eq!(inherited.expires_at, Some(600_000));
    }

    #[test]
    fn paths_outside_the_scope_are_blocked_with_a_reason() {
        let scope = scope(&["crates/hub"], &["read"], true, 0, 0);
        assert!(allows_path(&scope, "crates/hub/src/lib.rs"));
        assert!(!allows_path(&scope, "crates/other/src/lib.rs"));
        assert!(!allows_path(&scope, "crates/hubby"));
        let reason = blocked(
            &scope,
            &tool("read file"),
            &["docs/readme.md".to_owned()],
            0,
        )
        .unwrap();
        assert!(reason.contains("docs/readme.md"), "{reason}");
        assert!(blocked(
            &scope,
            &tool("read file"),
            &["crates/hub/src/lib.rs".to_owned()],
            0
        )
        .is_none());
    }

    #[test]
    fn tools_network_and_the_deadline_are_enforced() {
        let scope = scope(&[], &["read"], false, 5, 0);
        let reason = blocked(&scope, &tool("edit file"), &[], 0).unwrap();
        assert!(reason.contains("read"), "{reason}");
        let reason = blocked(&scope, &tool("read http://example.com"), &[], 0).unwrap();
        assert!(reason.contains("network"), "{reason}");
        let reason = blocked(&scope, &tool("read file"), &[], 600_000).unwrap();
        assert!(reason.contains("deadline"), "{reason}");
        assert!(blocked(&scope, &tool("read file"), &[], 60_000).is_none());
    }

    #[test]
    fn describe_lists_the_limits() {
        let text = describe(&scope(&["src"], &["read"], false, 10, 0), 0);
        assert!(text.contains("src") && text.contains("read") && text.contains("no network"));
        assert!(text.contains("10 min left"), "{text}");
    }

    #[test]
    fn absolute_paths_become_relative_to_the_workspace() {
        assert_eq!(
            relative_to("/work/project", "/work/project/src/main.rs"),
            "src/main.rs"
        );
        assert_eq!(relative_to("/work/project", "/etc/passwd"), "/etc/passwd");
        assert_eq!(
            relative_to("C:\\work", "C:\\work\\src\\main.rs"),
            "src/main.rs"
        );
    }
}
