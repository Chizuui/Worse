use serde::{Deserialize, Serialize};

use crate::chat::Cost;
use crate::types::{AgentId, SessionId};

pub const MAX_BOARD_CARDS: usize = 500;
pub const MAX_CARD_TITLE: usize = 200;
pub const MAX_CARD_BODY: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum BoardLane {
    #[default]
    Backlog,
    Todo,
    InProgress,
    Review,
    Done,
}

impl BoardLane {
    pub const ALL: [Self; 5] = [
        Self::Backlog,
        Self::Todo,
        Self::InProgress,
        Self::Review,
        Self::Done,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Backlog => "backlog",
            Self::Todo => "todo",
            Self::InProgress => "inProgress",
            Self::Review => "review",
            Self::Done => "done",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|lane| lane.as_str() == text)
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Backlog => "Backlog",
            Self::Todo => "Todo",
            Self::InProgress => "In progress",
            Self::Review => "Review",
            Self::Done => "Done",
        }
    }

    #[must_use]
    pub fn index(self) -> usize {
        match self {
            Self::Backlog => 0,
            Self::Todo => 1,
            Self::InProgress => 2,
            Self::Review => 3,
            Self::Done => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum CardState {
    #[default]
    None,
    Running,
    NeedsApproval,
    Idle,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CardView {
    pub id: String,
    pub lane: BoardLane,
    pub position: u32,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub session_id: Option<SessionId>,
    #[serde(default)]
    pub state: CardState,
    #[serde(default)]
    pub agent: Option<AgentId>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub cost: Option<Cost>,
    #[serde(default)]
    pub issue_number: Option<u64>,
    #[serde(default)]
    pub url: Option<String>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BoardSync {
    pub repo: String,
    pub project_title: String,
    pub project_url: String,
    #[serde(default)]
    pub last_sync: Option<i64>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Board {
    pub workspace_key: String,
    pub mode: String,
    pub cards: Vec<CardView>,
    #[serde(default)]
    pub sync: Option<BoardSync>,
    #[serde(default)]
    pub link_repo: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewCard {
    pub workspace_key: String,
    pub workspace_path: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub lane: BoardLane,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCard {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
}
