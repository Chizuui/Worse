//! Fake answers for the chat and workspace calls (hub API 0.5.0, phase-9-parity.md §9.2). Session flags (title, pin,
//! unread) and find work on the fake sessions; groups, schedules and imports answer empty lists; calls that need real
//! turns (fork, rewind, retry, file restore, slash commands, command center) answer "unsupported".

use harness_core_api as api;

use super::{emit, FakeCore};
use crate::core::{ChatCall, Event, FindMatch, Item, SessionId, SessionSummary};

fn unsupported<T>(reply: crate::core::Reply<T>, what: &str) {
    let _ = reply.send(Err(api::ApiError::Internal(format!(
        "{what} is not supported by the fake core"
    ))));
}

/// Char offsets of every match of `query` in `text`.
fn matches(text: &str, query: &str, case_sensitive: bool) -> Vec<(u32, u32)> {
    let fold = |s: &str| {
        if case_sensitive {
            s.to_owned()
        } else {
            s.to_lowercase()
        }
    };
    let (hay, needle) = (fold(text), fold(query));
    if needle.is_empty() {
        return Vec::new();
    }
    let len = u32::try_from(needle.chars().count()).unwrap_or(u32::MAX);
    hay.match_indices(&needle)
        .map(|(byte, _)| {
            let start = hay[..byte].chars().count();
            (u32::try_from(start).unwrap_or(u32::MAX), len)
        })
        .collect()
}

impl FakeCore {
    /// Applies `change` to a session summary (listed and opened copies) and re-emits the list.
    fn update_summary(
        &mut self,
        session: &SessionId,
        change: impl Fn(&mut SessionSummary),
    ) -> bool {
        let mut found = false;
        for s in self.summaries.iter_mut().filter(|s| &s.id == session) {
            change(s);
            found = true;
        }
        if let Some(open) = self.open.get_mut(session) {
            change(&mut open.summary);
            found = true;
        }
        if found {
            emit(&self.ev, Event::Sessions(self.summaries.clone()));
        }
        found
    }

    fn find(&self, session: &SessionId, query: &str, case_sensitive: bool) -> Vec<FindMatch> {
        let Some(open) = self.open.get(session) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (id, item) in &open.items {
            let (field, text) = match item {
                Item::User { text, .. } => (api::FindField::User, text),
                Item::Assistant { text, .. } => (api::FindField::Assistant, text),
                Item::Tool { title, .. } => (api::FindField::Tool, title),
                Item::Notice { .. } => continue,
            };
            for (start, len) in matches(text, query, case_sensitive) {
                out.push(FindMatch {
                    item: Some(*id),
                    field,
                    start,
                    len,
                    snippet: text.chars().take(80).collect(),
                });
            }
        }
        out
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn chat_call(&mut self, call: ChatCall) {
        match call {
            ChatCall::Rename {
                session,
                title,
                reply,
            } => {
                let title = title.trim().to_owned();
                let result = if title.is_empty() {
                    Err(api::ApiError::InvalidArgument("empty title".into()))
                } else if self.update_summary(&session, |s| s.title.clone_from(&title)) {
                    self.persist_session(&session).await;
                    Ok(())
                } else {
                    Err(api::ApiError::NotFound(session))
                };
                let _ = reply.send(result);
            }
            ChatCall::SetPinned {
                sessions,
                pinned,
                reply,
            } => {
                for session in &sessions {
                    self.update_summary(session, |s| s.pinned = pinned);
                    self.persist_session(session).await;
                }
                let _ = reply.send(Ok(()));
            }
            ChatCall::SetUnread {
                session,
                unread,
                reply,
            } => {
                self.update_summary(&session, |s| s.unread = unread);
                self.persist_session(&session).await;
                let _ = reply.send(Ok(()));
            }
            ChatCall::Find {
                session,
                query,
                case_sensitive,
                reply,
            } => {
                let _ = reply.send(Ok(self.find(&session, &query, case_sensitive)));
            }
            ChatCall::ListGroups(reply) => {
                let _ = reply.send(Ok(Vec::new()));
            }
            ChatCall::Mentions(_, reply) => {
                let _ = reply.send(Ok(Vec::new()));
            }
            ChatCall::ImportScan(_, reply) => {
                let _ = reply.send(Ok(Vec::new()));
            }
            ChatCall::SchedulesEnabled(reply) => {
                let _ = reply.send(Ok(false));
            }
            ChatCall::ListSchedules(reply) => {
                let _ = reply.send(Ok(Vec::new()));
            }
            ChatCall::CreateGroup { reply, .. } | ChatCall::UpdateGroup(_, reply) => {
                unsupported(reply, "groups");
            }
            ChatCall::DeleteGroup(_, reply)
            | ChatCall::ReorderGroups(_, reply)
            | ChatCall::MoveSessions { reply, .. }
            | ChatCall::EditResend { reply, .. }
            | ChatCall::Retry { reply, .. }
            | ChatCall::SetSchedulesEnabled(_, reply)
            | ChatCall::DeleteSchedule(_, reply) => unsupported(reply, "this call"),
            ChatCall::Fork { reply, .. } => unsupported(reply, "fork"),
            ChatCall::Export { reply, .. } => unsupported(reply, "export"),
            ChatCall::Rewind { reply, .. } => unsupported(reply, "rewind"),
            ChatCall::PlanFiles { reply, .. } => unsupported(reply, "file changes"),
            ChatCall::ApplyFiles { reply, .. } => unsupported(reply, "file changes"),
            ChatCall::ResolveContext { reply, .. } => unsupported(reply, "attachments"),
            ChatCall::Commands { reply, .. } => unsupported(reply, "slash commands"),
            ChatCall::Search(_, reply) => unsupported(reply, "command search"),
            ChatCall::Import(_, reply) => unsupported(reply, "import"),
            ChatCall::CreateSchedule(_, reply) | ChatCall::UpdateSchedule(_, reply) => {
                unsupported(reply, "schedules");
            }
            ChatCall::RunScheduleNow(_, reply) => unsupported(reply, "schedules"),
            ChatCall::GetOrchestra(reply) => {
                let _ = reply.send(Ok(api::Orchestra::default()));
            }
            ChatCall::SetOrchestra(_, reply) => unsupported(reply, "orchestra"),
            ChatCall::Board(key, reply) => {
                let _ = reply.send(Ok(api::Board {
                    workspace_key: key,
                    mode: "local".into(),
                    cards: Vec::new(),
                    ..api::Board::default()
                }));
            }
            ChatCall::CreateCard(_, reply)
            | ChatCall::UpdateCard(_, reply)
            | ChatCall::StartCard(_, reply)
            | ChatCall::MoveCard { reply, .. } => unsupported(reply, "the board"),
            ChatCall::DeleteCard(_, reply) | ChatCall::UnlinkBoard(_, reply) => {
                unsupported(reply, "the board");
            }
            ChatCall::LinkBoard(_, reply) | ChatCall::SyncBoard(_, reply) => {
                unsupported(reply, "the board");
            }
            ChatCall::InboxStatus(reply) => {
                let _ = reply.send(Ok(api::InboxStatus::default()));
            }
            ChatCall::Tickets(reply) => {
                let _ = reply.send(Ok(Vec::new()));
            }
            ChatCall::Triggers(_, reply) => {
                let _ = reply.send(Ok(Vec::new()));
            }
            ChatCall::SaveInboxSource(_, reply) => unsupported(reply, "the inbox"),
            ChatCall::SetInboxEnabled(_, reply)
            | ChatCall::DeleteInboxSource(_, reply)
            | ChatCall::PollInboxNow(_, reply)
            | ChatCall::PostTicket(_, reply)
            | ChatCall::StopTicket(_, reply)
            | ChatCall::DiscardTicket(_, reply) => unsupported(reply, "the inbox"),
            ChatCall::ResumableRun { reply, .. } => {
                let _ = reply.send(Ok(None));
            }
            ChatCall::ResumeRun { reply, .. } | ChatCall::DiscardRun { reply, .. } => {
                unsupported(reply, "resumable runs");
            }
        }
    }
}
