use std::path::Path;
use std::sync::Weak;

use harness_core_api::{
    AgentId, ApiError, ApiResult, Board, BoardLane, CardState, CardView, CreateSession, EventKind,
    HarnessCore, NewCard, RecvError, SendMessage, SessionId, TurnOutcome, UpdateCard,
    WorktreeRequest, MAX_BOARD_CARDS, MAX_CARD_BODY, MAX_CARD_TITLE,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::HubError;
use crate::hub::{Hub, HubCtx};
use crate::store_chat::{parse_json, session_cost};
use crate::util::{new_id, now_ms};

pub(crate) const TABLE: &str = "
CREATE TABLE IF NOT EXISTS kanban_cards(
  id TEXT PRIMARY KEY, workspace_key TEXT NOT NULL, json TEXT NOT NULL, lane TEXT NOT NULL,
  position INTEGER NOT NULL, session_id TEXT, updated_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS kanban_cards_ws ON kanban_cards(workspace_key, lane, position);
CREATE INDEX IF NOT EXISTS kanban_cards_session ON kanban_cards(session_id);
";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CardData {
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) body: String,
    #[serde(default)]
    pub(crate) workspace_path: String,
    #[serde(default)]
    pub(crate) state: CardState,
    #[serde(default)]
    pub(crate) agent: Option<AgentId>,
    #[serde(default)]
    pub(crate) issue_number: Option<u64>,
    #[serde(default)]
    pub(crate) url: Option<String>,
    #[serde(default)]
    pub(crate) item_id: Option<String>,
    #[serde(default)]
    pub(crate) synced_lane: Option<BoardLane>,
    #[serde(default)]
    pub(crate) local_only: bool,
}

pub(crate) struct Row {
    pub(crate) id: String,
    pub(crate) workspace_key: String,
    pub(crate) data: CardData,
    pub(crate) lane: BoardLane,
    pub(crate) position: u32,
    pub(crate) session_id: Option<SessionId>,
    pub(crate) updated_at: i64,
}

const COLUMNS: &str = "id, workspace_key, json, lane, position, session_id, updated_at";

pub(crate) fn rows_of(c: &Connection, key: &str) -> Result<Vec<Row>, HubError> {
    let mut statement = c.prepare(&format!(
        "SELECT {COLUMNS} FROM kanban_cards WHERE workspace_key=?1"
    ))?;
    let rows = statement
        .query_map(params![key], read_row)?
        .filter_map(Result::ok)
        .collect();
    Ok(rows)
}

fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    let json: String = r.get(2)?;
    let lane: String = r.get(3)?;
    let position: i64 = r.get(4)?;
    Ok(Row {
        id: r.get(0)?,
        workspace_key: r.get(1)?,
        data: serde_json::from_str(&json).unwrap_or_default(),
        lane: BoardLane::parse(&lane).unwrap_or_default(),
        position: u32::try_from(position.max(0)).unwrap_or(u32::MAX),
        session_id: r.get::<_, Option<String>>(5)?.map(SessionId::new),
        updated_at: r.get(6)?,
    })
}

pub(crate) fn load(c: &Connection, id: &str) -> Result<Option<Row>, HubError> {
    Ok(c.query_row(
        &format!("SELECT {COLUMNS} FROM kanban_cards WHERE id=?1"),
        params![id],
        read_row,
    )
    .optional()?)
}

fn by_session(c: &Connection, session: &SessionId) -> Result<Vec<Row>, HubError> {
    let mut statement = c.prepare(&format!(
        "SELECT {COLUMNS} FROM kanban_cards WHERE session_id=?1"
    ))?;
    let rows = statement
        .query_map(params![session.as_str()], read_row)?
        .filter_map(Result::ok)
        .collect();
    Ok(rows)
}

pub(crate) fn lane_ids(
    c: &Connection,
    key: &str,
    lane: BoardLane,
) -> Result<Vec<String>, HubError> {
    let mut statement = c.prepare(
        "SELECT id FROM kanban_cards WHERE workspace_key=?1 AND lane=?2 ORDER BY position, updated_at",
    )?;
    let ids = statement
        .query_map(params![key, lane.as_str()], |r| r.get::<_, String>(0))?
        .filter_map(Result::ok)
        .collect();
    Ok(ids)
}

fn renumber(c: &Connection, ids: &[String]) -> Result<(), HubError> {
    for (index, id) in ids.iter().enumerate() {
        c.execute(
            "UPDATE kanban_cards SET position=?2 WHERE id=?1",
            params![id, i64::try_from(index).unwrap_or(i64::MAX)],
        )?;
    }
    Ok(())
}

pub(crate) fn save(c: &Connection, row: &Row) -> Result<(), HubError> {
    c.execute(
        "INSERT INTO kanban_cards(id, workspace_key, json, lane, position, session_id, updated_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(id) DO UPDATE SET json=?3, lane=?4, position=?5, session_id=?6, updated_at=?7",
        params![
            row.id,
            row.workspace_key,
            serde_json::to_string(&row.data)?,
            row.lane.as_str(),
            i64::from(row.position),
            row.session_id.as_ref().map(SessionId::as_str),
            row.updated_at,
        ],
    )?;
    Ok(())
}

pub(crate) fn place(
    c: &Connection,
    row: &mut Row,
    lane: BoardLane,
    index: Option<usize>,
) -> Result<(), HubError> {
    let old = lane_ids(c, &row.workspace_key, row.lane)?;
    let rest: Vec<String> = old.into_iter().filter(|id| *id != row.id).collect();
    if row.lane != lane {
        renumber(c, &rest)?;
    }
    let mut target = if row.lane == lane {
        rest
    } else {
        lane_ids(c, &row.workspace_key, lane)?
            .into_iter()
            .filter(|id| *id != row.id)
            .collect()
    };
    let at = index.unwrap_or(target.len()).min(target.len());
    target.insert(at, row.id.clone());
    row.lane = lane;
    row.position = u32::try_from(at).unwrap_or(u32::MAX);
    row.updated_at = now_ms();
    save(c, row)?;
    renumber(c, &target)
}

fn view(c: &Connection, row: Row) -> Result<CardView, HubError> {
    let (branch, cost) = match &row.session_id {
        Some(session) => {
            let worktree: Option<String> = c
                .query_row(
                    "SELECT worktree_json FROM sessions WHERE id=?1",
                    params![session.as_str()],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            let branch = parse_json::<harness_core_api::WorktreeInfo>(worktree).map(|w| w.branch);
            (branch, session_cost(c, session)?.map(|cost| cost.total))
        }
        None => (None, None),
    };
    Ok(CardView {
        id: row.id,
        lane: row.lane,
        position: row.position,
        title: row.data.title,
        body: row.data.body,
        session_id: row.session_id,
        state: row.data.state,
        agent: row.data.agent,
        branch,
        cost,
        issue_number: row.data.issue_number,
        url: row.data.url,
        updated_at: row.updated_at,
    })
}

pub(crate) fn link_card(
    c: &Connection,
    workspace_key: &str,
    workspace_path: &str,
    title: &str,
    body: &str,
    session: &SessionId,
    agent: Option<AgentId>,
    issue_number: Option<u64>,
) -> Result<String, HubError> {
    let lane = BoardLane::InProgress;
    let existing = issue_number
        .map(|_| rows_of(c, workspace_key))
        .transpose()?
        .and_then(|rows| {
            rows.into_iter()
                .find(|r| r.data.issue_number == issue_number && r.session_id.is_none())
        });
    if let Some(mut row) = existing {
        row.session_id = Some(session.clone());
        row.data.state = CardState::Idle;
        row.data.agent = agent;
        place(c, &mut row, lane, None)?;
        return Ok(row.id);
    }
    let row = Row {
        id: new_id("card"),
        workspace_key: workspace_key.to_owned(),
        data: CardData {
            title: title.chars().take(MAX_CARD_TITLE).collect(),
            body: body.chars().take(MAX_CARD_BODY).collect(),
            workspace_path: workspace_path.to_owned(),
            state: CardState::Idle,
            agent,
            issue_number,
            url: body.starts_with("https://").then(|| body.to_owned()),
            ..CardData::default()
        },
        lane,
        position: u32::try_from(lane_ids(c, workspace_key, lane)?.len()).unwrap_or(u32::MAX),
        session_id: Some(session.clone()),
        updated_at: now_ms(),
    };
    save(c, &row)?;
    Ok(row.id)
}

pub(crate) fn move_session_cards(
    c: &Connection,
    session: &SessionId,
    lane: BoardLane,
) -> Result<Vec<String>, HubError> {
    let mut keys = Vec::new();
    for mut row in by_session(c, session)? {
        if row.lane == lane {
            continue;
        }
        place(c, &mut row, lane, None)?;
        if !keys.contains(&row.workspace_key) {
            keys.push(row.workspace_key);
        }
    }
    Ok(keys)
}

fn clean_title(title: &str) -> ApiResult<String> {
    let title = title.trim();
    if title.is_empty() {
        return Err(ApiError::InvalidArgument("card title is empty".into()));
    }
    if title.chars().count() > MAX_CARD_TITLE {
        return Err(ApiError::InvalidArgument(format!(
            "card title is longer than {MAX_CARD_TITLE} characters"
        )));
    }
    Ok(title.to_owned())
}

fn clean_body(body: &str) -> ApiResult<String> {
    if body.chars().count() > MAX_CARD_BODY {
        return Err(ApiError::InvalidArgument(format!(
            "card body is longer than {MAX_CARD_BODY} characters"
        )));
    }
    Ok(body.trim_end().to_owned())
}

fn branch_for(title: &str, id: &str) -> String {
    let mut slug = String::new();
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 32 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "task" } else { slug };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or_default();
    let mut hash: u32 = 0x811c_9dc5;
    for byte in id.bytes().chain(nanos.to_le_bytes()) {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("harness/card-{slug}-{:04x}", hash & 0xffff)
}

fn not_found(id: &str) -> ApiError {
    ApiError::NotFound(format!("card {id}"))
}

impl Hub {
    pub(crate) fn board_changed(&self, key: &str) {
        self.ctx.events.emit(
            None,
            EventKind::BoardChanged {
                workspace_key: key.to_owned(),
            },
        );
    }

    async fn card_view(&self, id: String) -> ApiResult<CardView> {
        let card = self
            .ctx
            .store
            .run(move |c| match load(c, &id)? {
                Some(row) => view(c, row).map(Some),
                None => Ok(None),
            })
            .await?;
        card.ok_or_else(|| ApiError::NotFound("card".into()))
    }

    pub(crate) async fn do_board(&self, workspace_key: String) -> ApiResult<Board> {
        self.check_open()?;
        let key = workspace_key.clone();
        let (cards, sync, link_repo) = self
            .ctx
            .store
            .run(move |c| {
                let mut rows = rows_of(c, &key)?;
                rows.sort_by_key(|r| (r.lane.index(), r.position, r.updated_at));
                let cards = rows
                    .into_iter()
                    .map(|row| view(c, row))
                    .collect::<Result<Vec<_>, _>>()?;
                let (sync, link_repo) = crate::board_sync::meta(c, &key)?;
                Ok((cards, sync, link_repo))
            })
            .await?;
        Ok(Board {
            workspace_key,
            mode: if sync.is_some() { "github" } else { "local" }.into(),
            cards,
            sync,
            link_repo,
        })
    }

    pub(crate) async fn do_create_card(&self, request: NewCard) -> ApiResult<CardView> {
        self.check_open()?;
        let key = request.workspace_key.trim().to_owned();
        let key = if key.is_empty() {
            request.workspace_path.trim().to_owned()
        } else {
            key
        };
        if key.is_empty() {
            return Err(ApiError::InvalidArgument("card has no project".into()));
        }
        let title = clean_title(&request.title)?;
        let body = clean_body(&request.body)?;
        let id = new_id("card");
        let mut row = Row {
            id: id.clone(),
            workspace_key: key.clone(),
            data: CardData {
                title,
                body,
                workspace_path: request.workspace_path,
                ..CardData::default()
            },
            lane: request.lane,
            position: 0,
            session_id: None,
            updated_at: now_ms(),
        };
        let lane = request.lane;
        let count_key = key.clone();
        self.ctx
            .store
            .run(move |c| {
                let count: i64 = c.query_row(
                    "SELECT COUNT(*) FROM kanban_cards WHERE workspace_key=?1",
                    params![count_key],
                    |r| r.get(0),
                )?;
                if usize::try_from(count).unwrap_or(usize::MAX) >= MAX_BOARD_CARDS {
                    return Ok(false);
                }
                row.position =
                    u32::try_from(lane_ids(c, &row.workspace_key, lane)?.len()).unwrap_or(u32::MAX);
                save(c, &row)?;
                Ok(true)
            })
            .await?
            .then_some(())
            .ok_or_else(|| {
                ApiError::InvalidArgument(format!("a board holds at most {MAX_BOARD_CARDS} cards"))
            })?;
        self.board_changed(&key);
        if lane == BoardLane::Todo {
            return self.do_start_card(id).await;
        }
        self.card_view(id).await
    }

    pub(crate) async fn do_start_card(&self, card_id: String) -> ApiResult<CardView> {
        self.check_open()?;
        let id = card_id.clone();
        let (row, linked) = self
            .ctx
            .store
            .run(move |c| {
                let Some(row) = load(c, &id)? else {
                    return Ok(None);
                };
                let linked = crate::board_sync::is_linked(c, &row.workspace_key)?;
                Ok(Some((row, linked)))
            })
            .await?
            .ok_or_else(|| not_found(&card_id))?;
        if row.session_id.is_some() || !linked {
            return self.start_local_card(card_id).await;
        }
        let number = match (row.data.issue_number, row.data.item_id.as_deref()) {
            (None, Some(item)) => {
                match self.convert_draft(&row.workspace_key, &card_id, item).await {
                    Ok(number) => Some(number),
                    Err(error) => {
                        self.fail_card(&card_id, &row.workspace_key).await?;
                        return Err(ApiError::InvalidArgument(error));
                    }
                }
            }
            (number, _) => number,
        };
        match number {
            Some(number) => {
                if let Err(error) = self.start_issue_card(&row.workspace_key, number).await {
                    self.fail_card(&card_id, &row.workspace_key).await?;
                    return Err(ApiError::InvalidArgument(error));
                }
            }
            None => self.board_sync.wake(),
        }
        self.card_view(card_id).await
    }

    pub(crate) async fn fail_card(&self, card_id: &str, key: &str) -> ApiResult<()> {
        let id = card_id.to_owned();
        self.ctx
            .store
            .run(move |c| {
                if let Some(mut row) = load(c, &id)? {
                    row.data.state = CardState::Failed;
                    row.updated_at = now_ms();
                    save(c, &row)?;
                }
                Ok(())
            })
            .await?;
        self.board_changed(key);
        Ok(())
    }

    pub(crate) async fn do_update_card(&self, request: UpdateCard) -> ApiResult<CardView> {
        self.check_open()?;
        let title = request.title.as_deref().map(clean_title).transpose()?;
        let body = request.body.as_deref().map(clean_body).transpose()?;
        let id = request.id.clone();
        let key = self
            .ctx
            .store
            .run(move |c| {
                let Some(mut row) = load(c, &id)? else {
                    return Ok(None);
                };
                if let Some(title) = title {
                    row.data.title = title;
                }
                if let Some(body) = body {
                    row.data.body = body;
                }
                row.updated_at = now_ms();
                save(c, &row)?;
                Ok(Some(row.workspace_key))
            })
            .await?
            .ok_or_else(|| not_found(&request.id))?;
        self.board_changed(&key);
        self.card_view(request.id).await
    }

    pub(crate) async fn do_move_card(
        &self,
        card_id: String,
        lane: BoardLane,
        index: u32,
    ) -> ApiResult<CardView> {
        self.check_open()?;
        let id = card_id.clone();
        let moved = self
            .ctx
            .store
            .run(move |c| {
                let Some(mut row) = load(c, &id)? else {
                    return Ok(None);
                };
                place(c, &mut row, lane, usize::try_from(index).ok())?;
                Ok(Some((row.workspace_key, row.session_id.is_none())))
            })
            .await?;
        let (key, fresh) = moved.ok_or_else(|| not_found(&card_id))?;
        self.board_changed(&key);
        if lane == BoardLane::Todo && fresh {
            return self.do_start_card(card_id).await;
        }
        self.card_view(card_id).await
    }

    pub(crate) async fn do_delete_card(&self, card_id: String) -> ApiResult<()> {
        self.check_open()?;
        let id = card_id.clone();
        let item = self
            .ctx
            .store
            .run(move |c| {
                Ok(load(c, &id)?
                    .and_then(|row| row.data.item_id.map(|item| (row.workspace_key, item))))
            })
            .await?;
        if let Some((key, item)) = item {
            self.remove_board_item(&key, &item)
                .await
                .map_err(ApiError::InvalidArgument)?;
        }
        let id = card_id.clone();
        let key = self
            .ctx
            .store
            .run(move |c| {
                let Some(row) = load(c, &id)? else {
                    return Ok(None);
                };
                c.execute("DELETE FROM kanban_cards WHERE id=?1", params![id])?;
                renumber(c, &lane_ids(c, &row.workspace_key, row.lane)?)?;
                Ok(Some(row.workspace_key))
            })
            .await?
            .ok_or_else(|| not_found(&card_id))?;
        self.board_changed(&key);
        Ok(())
    }

    async fn start_local_card(&self, card_id: String) -> ApiResult<CardView> {
        let id = card_id.clone();
        let row = self
            .ctx
            .store
            .run(move |c| load(c, &id))
            .await?
            .ok_or_else(|| not_found(&card_id))?;
        if row.session_id.is_some() {
            return self.card_view(card_id).await;
        }
        let path = if row.data.workspace_path.trim().is_empty() {
            row.workspace_key.clone()
        } else {
            row.data.workspace_path.clone()
        };
        let is_repo =
            crate::snapshot::git_text(Path::new(&path), &["rev-parse", "--is-inside-work-tree"])
                .await
                .is_ok_and(|out| out.trim() == "true");
        let identity = (row.workspace_key != path).then(|| row.workspace_key.clone());
        let summary = self
            .do_create_session(CreateSession {
                workspace_path: path,
                workspace_identity: identity,
                title: Some(row.data.title.clone()),
                agent: None,
                worktree: is_repo.then(|| WorktreeRequest {
                    branch: Some(branch_for(&row.data.title, &row.id)),
                    base: None,
                }),
            })
            .await?;
        let session = summary.id.clone();
        let id = card_id.clone();
        let linked = session.clone();
        self.ctx
            .store
            .run(move |c| {
                let Some(mut row) = load(c, &id)? else {
                    return Ok(());
                };
                row.session_id = Some(linked);
                row.data.state = CardState::Idle;
                row.data.agent = Some(summary.active_agent.clone());
                place(c, &mut row, BoardLane::InProgress, None)
            })
            .await?;
        self.board_changed(&row.workspace_key);
        let text = if row.data.body.trim().is_empty() {
            row.data.title.clone()
        } else {
            format!("{}\n\n{}", row.data.title, row.data.body)
        };
        let sent = self
            .send_message(SendMessage {
                session_id: session,
                text,
                agent: None,
                client_message_id: None,
                context: Vec::new(),
            })
            .await;
        if let Err(error) = sent {
            let id = card_id.clone();
            self.ctx
                .store
                .run(move |c| {
                    if let Some(mut row) = load(c, &id)? {
                        row.data.state = CardState::Failed;
                        row.updated_at = now_ms();
                        save(c, &row)?;
                    }
                    Ok(())
                })
                .await?;
            self.board_changed(&row.workspace_key);
            return Err(error);
        }
        self.card_view(card_id).await
    }
}

enum Change {
    Started(AgentId),
    Approval,
    Resumed,
    Finished(TurnOutcome),
}

fn apply(c: &Connection, session: &SessionId, change: &Change) -> Result<Vec<String>, HubError> {
    let mut keys = Vec::new();
    for mut row in by_session(c, session)? {
        let before = (row.lane, row.data.state, row.data.agent.clone());
        let mut lane = row.lane;
        match change {
            Change::Started(agent) => {
                row.data.state = CardState::Running;
                row.data.agent = Some(agent.clone());
                if matches!(row.lane, BoardLane::Todo | BoardLane::Review) {
                    lane = BoardLane::InProgress;
                }
            }
            Change::Approval => row.data.state = CardState::NeedsApproval,
            Change::Resumed => {
                if row.data.state == CardState::NeedsApproval {
                    row.data.state = CardState::Running;
                }
            }
            Change::Finished(TurnOutcome::Completed { .. }) => {
                row.data.state = CardState::Idle;
                if matches!(row.lane, BoardLane::Todo | BoardLane::InProgress) {
                    lane = BoardLane::Review;
                }
            }
            Change::Finished(TurnOutcome::Failed { .. }) => row.data.state = CardState::Failed,
            Change::Finished(TurnOutcome::Cancelled) => row.data.state = CardState::Idle,
        }
        if before == (lane, row.data.state, row.data.agent.clone()) {
            continue;
        }
        if lane == row.lane {
            row.updated_at = now_ms();
            save(c, &row)?;
        } else {
            place(c, &mut row, lane, None)?;
        }
        if !keys.contains(&row.workspace_key) {
            keys.push(row.workspace_key);
        }
    }
    Ok(keys)
}

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
        let Some(session) = event.session_id.clone() else {
            continue;
        };
        let change = match event.kind {
            EventKind::TurnStarted { agent, .. } => Change::Started(agent),
            EventKind::PermissionRequested { .. } => Change::Approval,
            EventKind::PermissionResolved { .. } => Change::Resumed,
            EventKind::TurnFinished { outcome, .. } => Change::Finished(outcome),
            _ => continue,
        };
        let Some(ctx) = ctx.upgrade() else {
            return;
        };
        let refresh_cost = matches!(change, Change::Finished(_));
        let result = ctx
            .store
            .run(move |c| {
                let mut keys = apply(c, &session, &change)?;
                if refresh_cost {
                    for row in by_session(c, &session)? {
                        if !keys.contains(&row.workspace_key) {
                            keys.push(row.workspace_key);
                        }
                    }
                }
                Ok(keys)
            })
            .await;
        match result {
            Ok(keys) => {
                for key in keys {
                    ctx.events
                        .emit(None, EventKind::BoardChanged { workspace_key: key });
                }
            }
            Err(error) => tracing::warn!(%error, "board status not saved"),
        }
    }
}
