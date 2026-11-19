use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use harness_core_api::{
    ApiError, ApiResult, Board, BoardLane, BoardSync, EventKind, RecvError, MAX_BOARD_CARDS,
    MAX_CARD_BODY, MAX_CARD_TITLE,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::board::{load, place, rows_of, save, CardData, Row};
use crate::error::HubError;
use crate::gh::gh;
use crate::hub::Hub;
use crate::util::{new_id, now_ms};

pub(crate) const TABLE: &str = "
CREATE TABLE IF NOT EXISTS board_links(workspace_key TEXT PRIMARY KEY, source_id TEXT NOT NULL, json TEXT NOT NULL);
";

const SYNC_EVERY: Duration = Duration::from_secs(120);
const DEBOUNCE: Duration = Duration::from_secs(2);
const SCOPE_HINT: &str =
    "GitHub Project access needs the project scope: run `gh auth refresh -s project`";

#[derive(Default)]
pub(crate) struct BoardSyncSlot {
    wake: Arc<Notify>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    lock: tokio::sync::Mutex<()>,
    linked: Mutex<HashSet<String>>,
}

impl BoardSyncSlot {
    pub(crate) fn wake(&self) {
        self.wake.notify_one();
    }

    pub(crate) fn stop(&self) {
        if let Ok(mut slot) = self.task.lock() {
            if let Some(task) = slot.take() {
                task.abort();
            }
        }
    }

    fn is_linked(&self, key: &str) -> bool {
        self.linked.lock().is_ok_and(|set| set.contains(key))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Link {
    repo: String,
    project_id: String,
    project_number: u64,
    project_url: String,
    project_title: String,
    status_field_id: String,
    options: BTreeMap<String, String>,
    linked_at: i64,
    #[serde(default)]
    last_sync: Option<i64>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    workflow_error: Option<String>,
}

impl Link {
    fn option_for(&self, lane: BoardLane) -> Option<&String> {
        self.options.get(lane.as_str())
    }

    fn lane_for(&self, option: &str) -> Option<BoardLane> {
        self.options
            .iter()
            .find(|(_, id)| id.as_str() == option)
            .and_then(|(lane, _)| BoardLane::parse(lane))
    }
}

fn load_link(c: &Connection, key: &str) -> Result<Option<(String, Link)>, HubError> {
    let row: Option<(String, String)> = c
        .query_row(
            "SELECT source_id, json FROM board_links WHERE workspace_key=?1",
            params![key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row.and_then(|(source, json)| serde_json::from_str(&json).ok().map(|l| (source, l))))
}

fn save_link(c: &Connection, key: &str, source: &str, link: &Link) -> Result<(), HubError> {
    c.execute(
        "INSERT INTO board_links(workspace_key, source_id, json) VALUES(?1, ?2, ?3)
         ON CONFLICT(workspace_key) DO UPDATE SET source_id=?2, json=?3",
        params![key, source, serde_json::to_string(link)?],
    )?;
    Ok(())
}

fn linked_keys(c: &Connection) -> Result<Vec<String>, HubError> {
    let mut statement = c.prepare("SELECT workspace_key FROM board_links")?;
    let keys = statement
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(Result::ok)
        .collect();
    Ok(keys)
}

pub(crate) fn is_linked(c: &Connection, key: &str) -> Result<bool, HubError> {
    Ok(load_link(c, key)?.is_some())
}

pub(crate) fn meta(
    c: &Connection,
    key: &str,
) -> Result<(Option<BoardSync>, Option<String>), HubError> {
    if let Some((_, link)) = load_link(c, key)? {
        let sync = BoardSync {
            repo: link.repo,
            project_title: link.project_title,
            project_url: link.project_url,
            last_sync: link.last_sync,
            error: link.error,
        };
        return Ok((Some(sync), None));
    }
    let repo = crate::inbox::sources(c)?
        .into_iter()
        .find(|s| s.workspace_key == key)
        .map(|s| s.repo);
    Ok((None, repo))
}

fn normalize(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn aliases(lane: BoardLane) -> &'static [&'static str] {
    match lane {
        BoardLane::Backlog => &["backlog"],
        BoardLane::Todo => &["todo", "ready"],
        BoardLane::InProgress => &["inprogress", "doing"],
        BoardLane::Review => &["review", "inreview"],
        BoardLane::Done => &["done"],
    }
}

fn lane_color(lane: BoardLane) -> &'static str {
    match lane {
        BoardLane::Backlog => "GRAY",
        BoardLane::Todo => "BLUE",
        BoardLane::InProgress => "YELLOW",
        BoardLane::Review => "PURPLE",
        BoardLane::Done => "GREEN",
    }
}

fn friendly(message: &str) -> String {
    let lower = message.to_lowercase();
    if lower.contains("insufficient_scopes")
        || lower.contains("read:project")
        || lower.contains("'project' scope")
        || lower.contains("project scope")
    {
        return SCOPE_HINT.to_owned();
    }
    message.trim().chars().take(400).collect()
}

async fn graphql(query: &str, variables: Value) -> Result<Value, String> {
    let body = json!({ "query": query, "variables": variables }).to_string();
    let out = gh(&["api", "graphql", "--input", "-"], Some(body))
        .await
        .map_err(|e| friendly(&e))?;
    let value: Value =
        serde_json::from_str(&out).map_err(|e| format!("GitHub answered with bad JSON: {e}"))?;
    if let Some(errors) = value["errors"].as_array().filter(|e| !e.is_empty()) {
        let text = errors
            .iter()
            .map(|e| {
                format!(
                    "{} {}",
                    e["type"].as_str().unwrap_or_default(),
                    e["message"].as_str().unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(friendly(&text));
    }
    Ok(value["data"].clone())
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

const REPO_QUERY: &str = "query($o:String!,$r:String!){ repository(owner:$o,name:$r){ id name owner{ id login } projectsV2(first:20){ nodes{ id number title url closed } } } }";
const CREATE_PROJECT: &str = "mutation($o:ID!,$r:ID!,$t:String!){ createProjectV2(input:{ownerId:$o,repositoryId:$r,title:$t}){ projectV2{ id number title url } } }";
const STATUS_FIELD: &str = "query($p:ID!){ node(id:$p){ ... on ProjectV2 { field(name:\"Status\"){ ... on ProjectV2SingleSelectField { id options{ id name color description } } } } } }";
const UPDATE_FIELD: &str = "mutation($f:ID!,$o:[ProjectV2SingleSelectFieldOptionInput!]!){ updateProjectV2Field(input:{fieldId:$f,singleSelectOptions:$o}){ projectV2Field{ ... on ProjectV2SingleSelectField { id options{ id name } } } } }";
const ITEMS: &str = "query($p:ID!,$after:String){ node(id:$p){ ... on ProjectV2 { items(first:100, after:$after){ pageInfo{ hasNextPage endCursor } nodes{ id isArchived fieldValueByName(name:\"Status\"){ ... on ProjectV2ItemFieldSingleSelectValue { optionId } } content{ __typename ... on Issue { number title url body repository{ nameWithOwner } } ... on DraftIssue { title body } } } } } } }";
const SET_STATUS: &str = "mutation($p:ID!,$i:ID!,$f:ID!,$o:String!){ updateProjectV2ItemFieldValue(input:{projectId:$p,itemId:$i,fieldId:$f,value:{singleSelectOptionId:$o}}){ projectV2Item{ id } } }";
const WORKFLOWS: &str =
    "query($p:ID!){ node(id:$p){ ... on ProjectV2 { workflows(first:50){ nodes{ id name } } } } }";
const DELETE_WORKFLOW: &str =
    "mutation($w:ID!){ deleteProjectV2Workflow(input:{workflowId:$w}){ clientMutationId } }";
const FIGHTING_WORKFLOWS: [&str; 2] = ["Pull request linked to issue", "Item added to project"];
const CONVERT_DRAFT: &str = "mutation($i:ID!,$r:ID!){ convertProjectV2DraftIssueItemToIssue(input:{itemId:$i,repositoryId:$r}){ item{ id content{ ... on Issue { number url } } } } }";
const DELETE_ITEM: &str = "mutation($p:ID!,$i:ID!){ deleteProjectV2Item(input:{projectId:$p,itemId:$i}){ deletedItemId } }";
const ADD_ITEM: &str = "mutation($p:ID!,$c:ID!){ addProjectV2ItemById(input:{projectId:$p,contentId:$c}){ item{ id } } }";

struct Remote {
    item_id: String,
    number: Option<u64>,
    title: String,
    body: String,
    url: Option<String>,
    option: Option<String>,
}

async fn fetch_items(link: &Link) -> Result<Vec<Remote>, String> {
    let mut out = Vec::new();
    let mut after = Value::Null;
    for _ in 0..=(MAX_BOARD_CARDS / 100) {
        let data = graphql(ITEMS, json!({ "p": link.project_id, "after": after })).await?;
        let items = &data["node"]["items"];
        for node in items["nodes"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            if node["isArchived"].as_bool() == Some(true) {
                continue;
            }
            let content = &node["content"];
            let kind = content["__typename"].as_str().unwrap_or_default();
            let (number, url) = match kind {
                "Issue" => {
                    if !content["repository"]["nameWithOwner"]
                        .as_str()
                        .is_some_and(|r| r.eq_ignore_ascii_case(&link.repo))
                    {
                        continue;
                    }
                    (content["number"].as_u64(), Some(text(&content["url"])))
                }
                "DraftIssue" => (None, None),
                _ => continue,
            };
            out.push(Remote {
                item_id: text(&node["id"]),
                number,
                title: text(&content["title"]),
                body: text(&content["body"]),
                url,
                option: node["fieldValueByName"]["optionId"]
                    .as_str()
                    .map(str::to_owned),
            });
        }
        if items["pageInfo"]["hasNextPage"].as_bool() != Some(true) {
            break;
        }
        after = items["pageInfo"]["endCursor"].clone();
    }
    Ok(out)
}

async fn set_status(link: &Link, item: &str, lane: BoardLane) -> Result<(), String> {
    let option = link
        .option_for(lane)
        .ok_or_else(|| format!("the project has no Status option for {}", lane.label()))?;
    graphql(
        SET_STATUS,
        json!({ "p": link.project_id, "i": item, "f": link.status_field_id, "o": option }),
    )
    .await
    .map(|_| ())
}

async fn ensure_status_field(
    project_id: &str,
) -> Result<(String, BTreeMap<String, String>), String> {
    let data = graphql(STATUS_FIELD, json!({ "p": project_id })).await?;
    let field = &data["node"]["field"];
    let field_id = text(&field["id"]);
    if field_id.is_empty() {
        return Err("the project has no single-select Status field".to_owned());
    }
    let existing: Vec<Value> = field["options"].as_array().cloned().unwrap_or_default();
    let mut mapping = BTreeMap::new();
    let mut used = HashSet::new();
    for lane in BoardLane::ALL {
        if let Some(option) = existing.iter().find(|o| {
            let id = text(&o["id"]);
            !used.contains(&id) && aliases(lane).contains(&normalize(&text(&o["name"])).as_str())
        }) {
            let id = text(&option["id"]);
            used.insert(id.clone());
            mapping.insert(lane.as_str().to_owned(), id);
        }
    }
    if mapping.len() == BoardLane::ALL.len() {
        return Ok((field_id, mapping));
    }
    let mut list = Vec::new();
    for lane in BoardLane::ALL {
        match mapping
            .get(lane.as_str())
            .and_then(|id| existing.iter().find(|o| text(&o["id"]) == *id))
        {
            Some(option) => list.push(json!({
                "id": option["id"], "name": option["name"], "color": option["color"],
                "description": option["description"].as_str().unwrap_or_default(),
            })),
            None => list.push(
                json!({ "name": lane.label(), "color": lane_color(lane), "description": "" }),
            ),
        }
    }
    for option in existing.iter().filter(|o| !used.contains(&text(&o["id"]))) {
        list.push(json!({
            "id": option["id"], "name": option["name"], "color": option["color"],
            "description": option["description"].as_str().unwrap_or_default(),
        }));
    }
    let data = graphql(UPDATE_FIELD, json!({ "f": field_id, "o": list })).await?;
    let options = data["updateProjectV2Field"]["projectV2Field"]["options"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut mapping = BTreeMap::new();
    for lane in BoardLane::ALL {
        if let Some(option) = options
            .iter()
            .find(|o| aliases(lane).contains(&normalize(&text(&o["name"])).as_str()))
        {
            mapping.insert(lane.as_str().to_owned(), text(&option["id"]));
        }
    }
    if mapping.len() != BoardLane::ALL.len() {
        return Err("could not add the Harness lanes to the project's Status field".to_owned());
    }
    Ok((field_id, mapping))
}

fn split_repo(repo: &str) -> Result<(&str, &str), String> {
    repo.split_once('/')
        .filter(|(o, n)| !o.is_empty() && !n.is_empty())
        .ok_or_else(|| format!("bad repository name {repo}"))
}

struct Plan {
    changed: bool,
    pushes: Vec<(String, String, BoardLane)>,
    uploads: Vec<(String, Option<u64>, String, String, BoardLane)>,
    started: Vec<String>,
}

#[allow(clippy::too_many_lines)]
fn plan(
    c: &Connection,
    key: &str,
    path: &str,
    link: &Link,
    remote: &[Remote],
) -> Result<Plan, HubError> {
    let mut rows = rows_of(c, key)?;
    let mut plan = Plan {
        changed: false,
        pushes: Vec::new(),
        uploads: Vec::new(),
        started: Vec::new(),
    };
    let mut seen = HashSet::new();
    for item in remote {
        seen.insert(item.item_id.clone());
        let remote_lane = item.option.as_deref().and_then(|o| link.lane_for(o));
        let found = rows
            .iter()
            .position(|r| r.data.item_id.as_deref() == Some(item.item_id.as_str()))
            .or_else(|| {
                item.number.and_then(|n| {
                    rows.iter()
                        .position(|r| r.data.item_id.is_none() && r.data.issue_number == Some(n))
                })
            });
        let Some(index) = found else {
            if rows.len() >= MAX_BOARD_CARDS {
                continue;
            }
            let lane = remote_lane.unwrap_or_default();
            let title = match item.number {
                Some(n) => format!("#{n} {}", item.title),
                None => item.title.clone(),
            };
            let mut row = Row {
                id: new_id("card"),
                workspace_key: key.to_owned(),
                data: CardData {
                    title: title.chars().take(MAX_CARD_TITLE).collect(),
                    body: item
                        .url
                        .clone()
                        .unwrap_or_else(|| item.body.clone())
                        .chars()
                        .take(MAX_CARD_BODY)
                        .collect(),
                    workspace_path: path.to_owned(),
                    issue_number: item.number,
                    url: item.url.clone(),
                    item_id: Some(item.item_id.clone()),
                    synced_lane: Some(lane),
                    ..CardData::default()
                },
                lane: BoardLane::Backlog,
                position: 0,
                session_id: None,
                updated_at: now_ms(),
            };
            place(c, &mut row, lane, None)?;
            if remote_lane.is_none() {
                plan.pushes
                    .push((row.id.clone(), item.item_id.clone(), lane));
            }
            plan.changed = true;
            rows.push(row);
            continue;
        };
        let row = &mut rows[index];
        if row.data.item_id.is_none() {
            row.data.item_id = Some(item.item_id.clone());
            row.data.local_only = false;
            save(c, row)?;
        }
        match remote_lane {
            Some(remote) if Some(remote) != row.data.synced_lane => {
                row.data.synced_lane = Some(remote);
                if row.lane == remote {
                    save(c, row)?;
                } else {
                    place(c, row, remote, None)?;
                    plan.changed = true;
                    if remote == BoardLane::Todo && row.session_id.is_none() {
                        plan.started.push(row.id.clone());
                    }
                }
            }
            _ => {
                if row.data.synced_lane != Some(row.lane) || remote_lane.is_none() {
                    plan.pushes
                        .push((row.id.clone(), item.item_id.clone(), row.lane));
                }
            }
        }
    }
    for row in &mut rows {
        if let Some(item) = row.data.item_id.clone() {
            if !seen.contains(&item) {
                row.data.item_id = None;
                row.data.local_only = true;
                row.updated_at = now_ms();
                save(c, row)?;
                plan.changed = true;
            }
        } else if !row.data.local_only {
            plan.uploads.push((
                row.id.clone(),
                row.data.issue_number,
                row.data.title.clone(),
                row.data.body.clone(),
                row.lane,
            ));
        }
    }
    Ok(plan)
}

impl Hub {
    pub(crate) async fn ensure_board_sync(&self) {
        let keys = self.ctx.store.run(linked_keys).await.unwrap_or_default();
        if let Ok(mut set) = self.board_sync.linked.lock() {
            *set = keys.iter().cloned().collect();
        }
        if keys.is_empty() || self.check_open().is_err() {
            self.board_sync.stop();
            return;
        }
        let Ok(mut slot) = self.board_sync.task.lock() else {
            return;
        };
        if slot.as_ref().is_some_and(|t| !t.is_finished()) {
            self.board_sync.wake();
            return;
        }
        *slot = Some(tokio::spawn(run_loop(
            self.me.clone(),
            Arc::clone(&self.board_sync.wake),
        )));
    }

    async fn board_source(&self, key: &str) -> ApiResult<harness_core_api::InboxSource> {
        let wanted = key.to_owned();
        self.ctx
            .store
            .run(move |c| {
                Ok(crate::inbox::sources(c)?
                    .into_iter()
                    .find(|s| s.workspace_key == wanted))
            })
            .await?
            .ok_or_else(|| {
                ApiError::InvalidArgument(
                    "add this project's repository in Settings › Inbox first".into(),
                )
            })
    }

    pub(crate) async fn do_link_board(&self, workspace_key: String) -> ApiResult<Board> {
        self.check_open()?;
        let source = self.board_source(&workspace_key).await?;
        let link = create_link(&source.repo)
            .await
            .map_err(ApiError::InvalidArgument)?;
        let key = workspace_key.clone();
        let source_id = source.id.clone();
        self.ctx
            .store
            .run(move |c| {
                for mut row in rows_of(c, &key)? {
                    if row.data.item_id.is_none() && row.data.issue_number.is_none() {
                        row.data.local_only = true;
                        save(c, &row)?;
                    }
                }
                save_link(c, &key, &source_id, &link)
            })
            .await?;
        self.ensure_board_sync().await;
        self.sync_now(&workspace_key).await;
        self.board_changed(&workspace_key);
        self.do_board(workspace_key).await
    }

    pub(crate) async fn do_unlink_board(&self, workspace_key: String) -> ApiResult<()> {
        self.check_open()?;
        let key = workspace_key.clone();
        self.ctx
            .store
            .run(move |c| {
                c.execute(
                    "DELETE FROM board_links WHERE workspace_key=?1",
                    params![key],
                )?;
                Ok(())
            })
            .await?;
        self.ensure_board_sync().await;
        self.board_changed(&workspace_key);
        Ok(())
    }

    pub(crate) async fn do_sync_board(&self, workspace_key: String) -> ApiResult<Board> {
        self.check_open()?;
        self.sync_now(&workspace_key).await;
        self.do_board(workspace_key).await
    }

    async fn sync_now(&self, key: &str) {
        let _guard = self.board_sync.lock.lock().await;
        let result = self.sync_one(key).await;
        let wanted = key.to_owned();
        let error = result.as_ref().err().cloned();
        let saved = self
            .ctx
            .store
            .run(move |c| {
                if let Some((source, mut link)) = load_link(c, &wanted)? {
                    if error.is_none() {
                        link.last_sync = Some(now_ms());
                    }
                    link.error = error.or_else(|| link.workflow_error.clone());
                    save_link(c, &wanted, &source, &link)?;
                }
                Ok(())
            })
            .await;
        if let Err(error) = saved {
            tracing::warn!(%error, "board link state not saved");
        }
        if let Err(error) = result {
            tracing::info!(%error, "board sync failed");
            self.board_changed(key);
        }
    }

    async fn sync_one(&self, key: &str) -> Result<(), String> {
        let wanted = key.to_owned();
        let Some((source_id, link)) = self
            .ctx
            .store
            .run(move |c| load_link(c, &wanted))
            .await
            .map_err(|e| e.to_string())?
        else {
            return Ok(());
        };
        let path = self
            .ctx
            .store
            .run(move |c| {
                Ok(crate::inbox::sources(c)?
                    .into_iter()
                    .find(|s| s.id == source_id))
            })
            .await
            .map_err(|e| e.to_string())?
            .map_or_else(|| key.to_owned(), |s| s.workspace_path);
        let remote = fetch_items(&link).await?;
        let wanted = key.to_owned();
        let plan_link = link.clone();
        let mut plan = self
            .ctx
            .store
            .run(move |c| plan(c, &wanted, &path, &plan_link, &remote))
            .await
            .map_err(|e| e.to_string())?;
        let mut changed = plan.changed;
        let mut first_error: Option<String> = None;
        for (card, item, lane) in plan.pushes {
            match set_status(&link, &item, lane).await {
                Ok(()) => {
                    self.update_card(&card, move |row, _| {
                        row.data.synced_lane = Some(lane);
                        Ok(false)
                    })
                    .await?;
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        for (card, number, title, body, lane) in plan.uploads {
            match self.upload(&link, &card, number, &title, &body).await {
                Ok((number, url, item)) => {
                    let pushed = set_status(&link, &item, lane).await;
                    let synced = pushed.is_ok();
                    if let Err(error) = pushed {
                        first_error.get_or_insert(error);
                    }
                    let fresh = self
                        .update_card(&card, move |row, _| {
                            row.data.issue_number = Some(number);
                            row.data.url = Some(url);
                            row.data.item_id = Some(item);
                            if synced {
                                row.data.synced_lane = Some(lane);
                            }
                            Ok(row.session_id.is_none() && row.lane == BoardLane::Todo)
                        })
                        .await?;
                    if fresh {
                        plan.started.push(card);
                    }
                    changed = true;
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if changed {
            self.board_changed(key);
        }
        for card in plan.started {
            if let Err(error) = self.do_start_card(card).await {
                first_error.get_or_insert(error.to_string());
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    pub(crate) async fn remove_board_item(&self, key: &str, item: &str) -> Result<(), String> {
        let _guard = self.board_sync.lock.lock().await;
        let wanted = key.to_owned();
        let Some((_, link)) = self
            .ctx
            .store
            .run(move |c| load_link(c, &wanted))
            .await
            .map_err(|e| e.to_string())?
        else {
            return Ok(());
        };
        graphql(DELETE_ITEM, json!({ "p": link.project_id, "i": item }))
            .await
            .map(|_| ())
            .map_err(|e| format!("could not remove the card from the GitHub project: {e}"))
    }

    pub(crate) async fn convert_draft(
        &self,
        key: &str,
        card: &str,
        item: &str,
    ) -> Result<u64, String> {
        let wanted = key.to_owned();
        let (_, link) = self
            .ctx
            .store
            .run(move |c| load_link(c, &wanted))
            .await
            .map_err(|e| e.to_string())?
            .ok_or("the board is not linked to a GitHub project")?;
        let (owner, name) = split_repo(&link.repo)?;
        let repo = graphql(REPO_QUERY, json!({ "o": owner, "r": name })).await?;
        let repo_id = text(&repo["repository"]["id"]);
        if repo_id.is_empty() {
            return Err(format!("repository {} not found", link.repo));
        }
        let data = graphql(CONVERT_DRAFT, json!({ "i": item, "r": repo_id })).await?;
        let content = &data["convertProjectV2DraftIssueItemToIssue"]["item"]["content"];
        let number = content["number"]
            .as_u64()
            .ok_or("GitHub did not turn the draft into an issue")?;
        let url = text(&content["url"]);
        self.update_card(card, move |row, _| {
            row.data.issue_number = Some(number);
            row.data.url = Some(url);
            Ok(false)
        })
        .await?;
        Ok(number)
    }

    async fn update_card(
        &self,
        id: &str,
        change: impl FnOnce(&mut Row, &Connection) -> Result<bool, HubError> + Send + 'static,
    ) -> Result<bool, String> {
        let id = id.to_owned();
        self.ctx
            .store
            .run(move |c| {
                let Some(mut row) = load(c, &id)? else {
                    return Ok(false);
                };
                let placed = change(&mut row, c)?;
                if !placed {
                    row.updated_at = now_ms();
                    save(c, &row)?;
                }
                Ok(placed || row.lane == BoardLane::Todo && row.session_id.is_none())
            })
            .await
            .map_err(|e| e.to_string())
    }

    async fn upload(
        &self,
        link: &Link,
        card: &str,
        number: Option<u64>,
        title: &str,
        body: &str,
    ) -> Result<(u64, String, String), String> {
        let created = number.is_none();
        let issue = if let Some(n) = number {
            gh(&["api", &format!("repos/{}/issues/{n}", link.repo)], None).await?
        } else {
            let payload = json!({ "title": title, "body": body }).to_string();
            gh(
                &[
                    "api",
                    "-X",
                    "POST",
                    &format!("repos/{}/issues", link.repo),
                    "--input",
                    "-",
                ],
                Some(payload),
            )
            .await?
        };
        let issue: Value = serde_json::from_str(&issue).map_err(|e| e.to_string())?;
        let number = issue["number"]
            .as_u64()
            .ok_or("GitHub issue has no number")?;
        let node = text(&issue["node_id"]);
        let url = text(&issue["html_url"]);
        if created {
            let saved_url = url.clone();
            self.update_card(card, move |row, _| {
                row.data.issue_number = Some(number);
                row.data.url = Some(saved_url);
                Ok(false)
            })
            .await?;
        }
        let vars = json!({ "p": link.project_id, "c": node });
        let data = if let Ok(data) = graphql(ADD_ITEM, vars.clone()).await {
            data
        } else {
            tokio::time::sleep(Duration::from_secs(3)).await;
            graphql(ADD_ITEM, vars).await?
        };
        let item = text(&data["addProjectV2ItemById"]["item"]["id"]);
        if item.is_empty() {
            return Err("GitHub did not add the issue to the project".to_owned());
        }
        Ok((number, url, item))
    }
}

async fn remove_fighting_workflows(project_id: &str) -> Result<(), String> {
    let data = graphql(WORKFLOWS, json!({ "p": project_id })).await?;
    let nodes = data["node"]["workflows"]["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for node in nodes
        .iter()
        .filter(|n| FIGHTING_WORKFLOWS.contains(&n["name"].as_str().unwrap_or_default()))
    {
        graphql(DELETE_WORKFLOW, json!({ "w": node["id"] }))
            .await
            .map_err(|e| {
                format!(
                    "could not turn off the \"{}\" project workflow: {e}",
                    text(&node["name"])
                )
            })?;
    }
    Ok(())
}

async fn create_link(repo: &str) -> Result<Link, String> {
    let (owner, name) = split_repo(repo)?;
    let data = graphql(REPO_QUERY, json!({ "o": owner, "r": name })).await?;
    let repository = &data["repository"];
    if repository.is_null() {
        return Err(format!("repository {repo} not found or not visible to gh"));
    }
    let existing = repository["projectsV2"]["nodes"]
        .as_array()
        .and_then(|list| {
            list.iter()
                .find(|p| p["closed"].as_bool() != Some(true))
                .cloned()
        });
    let project = if let Some(project) = existing {
        project
    } else {
        let created = graphql(
            CREATE_PROJECT,
            json!({
                "o": repository["owner"]["id"],
                "r": repository["id"],
                "t": format!("{} board", text(&repository["name"])),
            }),
        )
        .await?;
        created["createProjectV2"]["projectV2"].clone()
    };
    let project_id = text(&project["id"]);
    if project_id.is_empty() {
        return Err("GitHub did not return a project".to_owned());
    }
    let (status_field_id, options) = ensure_status_field(&project_id).await?;
    let workflow_error = remove_fighting_workflows(&project_id).await.err();
    Ok(Link {
        repo: repo.to_owned(),
        project_id,
        project_number: project["number"].as_u64().unwrap_or_default(),
        project_url: text(&project["url"]),
        project_title: text(&project["title"]),
        status_field_id,
        options,
        linked_at: now_ms(),
        last_sync: None,
        error: None,
        workflow_error,
    })
}

async fn run_loop(hub: Weak<Hub>, wake: Arc<Notify>) {
    loop {
        let keys = {
            let Some(hub) = hub.upgrade() else { return };
            if hub.check_open().is_err() {
                return;
            }
            hub.ctx.store.run(linked_keys).await.unwrap_or_default()
        };
        if keys.is_empty() {
            return;
        }
        for key in keys {
            let Some(hub) = hub.upgrade() else { return };
            hub.sync_now(&key).await;
        }
        tokio::select! {
            () = tokio::time::sleep(SYNC_EVERY) => {}
            () = wake.notified() => {
                tokio::time::sleep(DEBOUNCE).await;
            }
        }
    }
}

pub(crate) async fn track(hub: Weak<Hub>) {
    let Some(mut events) = hub.upgrade().map(|h| h.ctx.events.subscribe()) else {
        return;
    };
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return,
        };
        let EventKind::BoardChanged { workspace_key } = event.kind else {
            continue;
        };
        let Some(hub) = hub.upgrade() else { return };
        if hub.board_sync.is_linked(&workspace_key) {
            hub.board_sync.wake();
        }
    }
}
