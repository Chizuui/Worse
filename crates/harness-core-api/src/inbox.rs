use serde::{Deserialize, Serialize};

use crate::types::{AgentId, SessionId};

pub const MAX_INBOX_SOURCES: usize = 20;
pub const DEFAULT_INBOX_HANDLE: &str = "@harness";
pub const DEFAULT_INBOX_INTERVAL: u32 = 120;
pub const MIN_INBOX_INTERVAL: u32 = 60;
pub const MAX_INBOX_INTERVAL: u32 = 3600;
pub const DEFAULT_INBOX_MAX_TURNS: u32 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum GhStatus {
    #[default]
    Missing,
    SignedOut,
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxSource {
    pub id: String,
    pub repo: String,
    pub workspace_path: String,
    pub workspace_key: String,
    pub enabled: bool,
    pub handle: String,
    #[serde(default)]
    pub allowlist: Vec<String>,
    pub interval_secs: u32,
    #[serde(default)]
    pub agent: Option<AgentId>,
    #[serde(default)]
    pub draft_first: bool,
    pub max_turns: u32,
    pub armed_at: i64,
    #[serde(default)]
    pub last_poll: Option<i64>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub route: bool,
    #[serde(default)]
    pub shadow_labels: Vec<String>,
    #[serde(default)]
    pub router_session_id: Option<SessionId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InboxSourceInput {
    #[serde(default)]
    pub id: Option<String>,
    pub repo: String,
    pub workspace_path: String,
    #[serde(default)]
    pub workspace_key: String,
    pub enabled: bool,
    #[serde(default)]
    pub handle: String,
    #[serde(default)]
    pub allowlist: Vec<String>,
    #[serde(default)]
    pub interval_secs: u32,
    #[serde(default)]
    pub agent: Option<AgentId>,
    #[serde(default)]
    pub draft_first: bool,
    #[serde(default)]
    pub max_turns: u32,
    #[serde(default)]
    pub route: bool,
    #[serde(default)]
    pub shadow_labels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InboxStatus {
    pub enabled: bool,
    pub gh: GhStatus,
    #[serde(default)]
    pub gh_detail: String,
    #[serde(default)]
    pub gh_path: String,
    pub sources: Vec<InboxSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum TicketStatus {
    #[default]
    Working,
    NeedsApproval,
    Review,
    Done,
    Failed,
    Discarded,
}

impl TicketStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::NeedsApproval => "needsApproval",
            Self::Review => "review",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Discarded => "discarded",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        [
            Self::Working,
            Self::NeedsApproval,
            Self::Review,
            Self::Done,
            Self::Failed,
            Self::Discarded,
        ]
        .into_iter()
        .find(|s| s.as_str() == text)
    }

    #[must_use]
    pub fn is_active(self) -> bool {
        matches!(self, Self::Working | Self::NeedsApproval | Self::Review)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ticket {
    pub id: String,
    pub source_id: String,
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub requester: String,
    pub status: TicketStatus,
    #[serde(default)]
    pub session_id: Option<SessionId>,
    #[serde(default)]
    pub card_id: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub pr_url: Option<String>,
    #[serde(default)]
    pub pending_post: bool,
    pub turns: u32,
    #[serde(default)]
    pub shadow: bool,
    #[serde(default)]
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TriggerRecord {
    pub id: String,
    pub source_id: String,
    pub repo: String,
    pub event_id: String,
    pub number: u64,
    pub author: String,
    pub kind: String,
    pub url: String,
    pub accepted: bool,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub ticket_id: Option<String>,
    pub received_at: i64,
}
