use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use harness_core_api::{ApiError, Board, BoardLane, CardState, CardView, NewCard, UpdateCard};

use super::{with_state, UiState};
use crate::core::{ChatCall, Command, Reply};
use crate::ui::{BoardCard, BoardColumn, BoardUi};

#[derive(Default)]
pub(super) struct BoardState {
    key: String,
    path: String,
    cards: Vec<CardView>,
}

fn sv(text: &str) -> SharedString {
    SharedString::from(text)
}

fn to_i32(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn state_id(state: CardState) -> &'static str {
    match state {
        CardState::None => "",
        CardState::Running => "running",
        CardState::NeedsApproval => "needsApproval",
        CardState::Idle => "idle",
        CardState::Failed => "failed",
    }
}

fn cost_text(card: &CardView) -> String {
    card.cost
        .as_ref()
        .and_then(|c| c.amount.trim().parse::<f64>().ok())
        .filter(|amount| *amount > 0.0)
        .map(|amount| format!("${amount:.2}"))
        .unwrap_or_default()
}

fn short_branch(branch: &str) -> &str {
    branch.strip_prefix("harness/").unwrap_or(branch)
}

impl UiState {
    fn board_ui(&self) -> BoardUi<'_> {
        self.window.global::<BoardUi>()
    }

    fn board_call<T: 'static>(
        &self,
        make: impl FnOnce(Reply<T>) -> ChatCall,
        done: impl FnOnce(&mut UiState, T) + 'static,
    ) {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.send(Command::Chat(make(reply)));
        let spawned = slint::spawn_local(async move {
            let result = match answer.await {
                Ok(result) => result,
                Err(_) => Err(ApiError::ShutDown),
            };
            with_state(|s| match result {
                Ok(value) => {
                    s.board_ui().set_error(SharedString::new());
                    done(s, value);
                }
                Err(error) => {
                    s.board_ui().set_error(sv(&error.to_string()));
                    s.load_board();
                }
            });
        });
        if spawned.is_err() {
            self.board_ui().set_error(sv("event loop unavailable"));
        }
    }

    fn board_target(&self) -> (String, String) {
        let active = self
            .active
            .as_ref()
            .and_then(|id| self.summaries.iter().find(|s| &s.id == id));
        let workspace = self.workspace.to_string_lossy().into_owned();
        let key = active
            .map(|s| s.project.clone())
            .filter(|k| !k.trim().is_empty())
            .unwrap_or_else(|| workspace.clone());
        let path = if key == workspace {
            workspace
        } else {
            self.summaries
                .iter()
                .find(|s| s.project == key && s.branch.is_none() && !s.workspace_path.is_empty())
                .map_or_else(|| key.clone(), |s| s.workspace_path.clone())
        };
        (key, path)
    }

    fn project_name(key: &str) -> String {
        std::path::Path::new(key)
            .file_name()
            .map_or_else(|| key.to_string(), |n| n.to_string_lossy().into_owned())
    }

    pub(super) fn open_board(&mut self) {
        let (key, path) = self.board_target();
        self.board.key = key;
        self.board.path = path;
        let ui = self.board_ui();
        ui.set_project(sv(&Self::project_name(&self.board.key)));
        ui.set_error(SharedString::new());
        ui.set_open(true);
        self.load_board();
    }

    pub(super) fn board_event(&mut self, workspace_key: &str) {
        if self.board_ui().get_open() && workspace_key == self.board.key {
            self.load_board();
        }
    }

    fn load_board(&self) {
        if self.board.key.is_empty() {
            return;
        }
        let key = self.board.key.clone();
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.send(Command::Chat(ChatCall::Board(key, reply)));
        let spawned = slint::spawn_local(async move {
            if let Ok(Ok(board)) = answer.await {
                with_state(|s| s.show_board(board));
            }
        });
        if spawned.is_err() {
            self.board_ui().set_error(sv("event loop unavailable"));
        }
    }

    fn show_board(&mut self, board: Board) {
        if board.workspace_key != self.board.key {
            return;
        }
        let mut counts = [0usize; 5];
        let mut rows = Vec::with_capacity(board.cards.len());
        for card in &board.cards {
            let lane = card.lane.index();
            let index = counts[lane];
            counts[lane] += 1;
            let preview = card
                .body
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or_default();
            let agent = card
                .agent
                .as_ref()
                .map(|a| self.agent_name(a.as_str()))
                .unwrap_or_default();
            rows.push(BoardCard {
                id: sv(&card.id),
                lane: to_i32(lane),
                index: to_i32(index),
                title: sv(&match card.issue_number {
                    Some(n) if !card.title.starts_with('#') => format!("#{n} {}", card.title),
                    _ => card.title.clone(),
                }),
                preview: sv(preview),
                state: sv(state_id(card.state)),
                agent: sv(&agent),
                branch: sv(card.branch.as_deref().map(short_branch).unwrap_or_default()),
                cost: sv(&cost_text(card)),
                has_session: card.session_id.is_some(),
            });
        }
        let columns: Vec<BoardColumn> = BoardLane::ALL
            .iter()
            .map(|lane| BoardColumn {
                label: sv(lane.label()),
                count: to_i32(counts[lane.index()]),
            })
            .collect();
        let ui = self.board_ui();
        let (mode, sync_text, sync_error) = sync_line(&board);
        ui.set_mode(sv(mode));
        ui.set_sync_text(sv(&sync_text));
        ui.set_sync_error(sync_error);
        ui.set_linked(board.sync.is_some());
        ui.set_can_link(board.link_repo.is_some());
        ui.set_busy(false);
        ui.set_columns(ModelRc::new(VecModel::from(columns)));
        ui.set_cards(ModelRc::new(VecModel::from(rows)));
        ui.set_max_count(to_i32(counts.iter().copied().max().unwrap_or(0)));
        self.board.cards = board.cards;
    }

    fn card(&self, id: &str) -> Option<&CardView> {
        self.board.cards.iter().find(|c| c.id == id)
    }

    fn open_card_dialog(&self, id: &str) {
        let ui = self.board_ui();
        let (title, body) = self
            .card(id)
            .map(|c| (c.title.clone(), c.body.clone()))
            .unwrap_or_default();
        ui.set_dialog_id(sv(id));
        ui.set_dialog_title(sv(&title));
        ui.set_dialog_body(sv(&body));
        ui.set_error(SharedString::new());
        ui.set_dialog_open(true);
    }

    fn open_card_session(&self, id: &str) {
        let Some(session) = self.card(id).and_then(|c| c.session_id.clone()) else {
            return;
        };
        self.board_ui().set_open(false);
        self.send(Command::OpenSession {
            session: session.to_string(),
        });
    }

    pub(super) fn board_action(&mut self, action: &str, id: &str) {
        let id = id.to_string();
        match action {
            "open" => self.open_board(),
            "new" => self.open_card_dialog(""),
            "edit" => self.open_card_dialog(&id),
            "click" => {
                if self.card(&id).is_some_and(|c| c.session_id.is_some()) {
                    self.open_card_session(&id);
                } else {
                    self.open_card_dialog(&id);
                }
            }
            "open-session" | "session" => self.open_card_session(&id),
            "start" => self.board_call(
                |reply| ChatCall::StartCard(id, reply),
                |s, _| s.load_board(),
            ),
            "delete" => self.board_call(
                |reply| ChatCall::DeleteCard(id, reply),
                |s, ()| s.load_board(),
            ),
            "link" | "sync" => {
                let key = self.board.key.clone();
                self.board_ui().set_busy(true);
                if action == "link" {
                    self.board_call(
                        |reply| ChatCall::LinkBoard(key, reply),
                        super::UiState::show_board,
                    );
                } else {
                    self.board_call(
                        |reply| ChatCall::SyncBoard(key, reply),
                        super::UiState::show_board,
                    );
                }
            }
            "unlink" => {
                let key = self.board.key.clone();
                self.board_call(
                    |reply| ChatCall::UnlinkBoard(key, reply),
                    |s, ()| s.load_board(),
                );
            }
            _ => {}
        }
    }

    fn board_save(&mut self) {
        let ui = self.board_ui();
        let id = ui.get_dialog_id().to_string();
        let title = ui.get_dialog_title().trim().to_string();
        let body = ui.get_dialog_body().to_string();
        if title.is_empty() {
            ui.set_error(sv("Give the card a title."));
            return;
        }
        ui.set_dialog_open(false);
        if id.is_empty() {
            let request = NewCard {
                workspace_key: self.board.key.clone(),
                workspace_path: self.board.path.clone(),
                title,
                body,
                lane: BoardLane::Backlog,
            };
            self.board_call(
                |reply| ChatCall::CreateCard(request, reply),
                |s, _| s.load_board(),
            );
        } else {
            let request = UpdateCard {
                id,
                title: Some(title),
                body: Some(body),
            };
            self.board_call(
                |reply| ChatCall::UpdateCard(request, reply),
                |s, _| s.load_board(),
            );
        }
    }

    fn board_move(&mut self, id: &str, lane: i32, index: i32) {
        let Some(lane) = usize::try_from(lane)
            .ok()
            .and_then(|i| BoardLane::ALL.get(i).copied())
        else {
            return;
        };
        let Some(card) = self.card(id) else {
            return;
        };
        let index = u32::try_from(index.max(0)).unwrap_or(0);
        if card.lane == lane && card.position == index {
            return;
        }
        let card = id.to_string();
        self.board_call(
            |reply| ChatCall::MoveCard {
                card,
                lane,
                index,
                reply,
            },
            |s, _| s.load_board(),
        );
    }
}

pub(super) fn wire(window: &crate::ui::AppWindow) {
    let ui = window.global::<BoardUi>();
    ui.on_action(|action, id| with_state(|s| s.board_action(&action, &id)));
    ui.on_save(|| with_state(UiState::board_save));
    ui.on_move(|id, lane, index| with_state(|s| s.board_move(&id, lane, index)));
}

fn sync_line(board: &Board) -> (&'static str, String, bool) {
    let Some(sync) = &board.sync else {
        return ("Local", String::new(), false);
    };
    if let Some(error) = &sync.error {
        return ("Sync error", error.clone(), true);
    }
    let ago = sync.last_sync.map_or_else(
        || "not synced yet".to_owned(),
        |at| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(at, |d| i64::try_from(d.as_millis()).unwrap_or(at));
            let secs = (now - at).max(0) / 1000;
            if secs < 60 {
                format!("synced {secs}s ago")
            } else {
                format!("synced {}m ago", secs / 60)
            }
        },
    );
    ("GitHub", format!("{} · {ago}", sync.project_title), false)
}
