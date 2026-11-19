//! Lazy chat history: opening a session shows only its last [`WINDOW`] items; older items stay here until the
//! user scrolls to the top of the chat (`ChatUi.load-earlier`), then come in [`WINDOW`]-sized pages.

use std::collections::{HashMap, HashSet};

use slint::{ComponentHandle, Model};

use harness_core_api::{ChangeDirection, ToolContent, ToolLocation};

use super::{with_state, UiState};
use crate::core::{Item, ItemId};
use crate::ui::{ChatItem, ChatUi};

pub(super) const WINDOW: usize = 100;

pub(super) struct Detail {
    pub(super) kind: Option<String>,
    pub(super) content: Vec<ToolContent>,
    pub(super) locations: Vec<ToolLocation>,
    pub(super) raw_input: Option<String>,
    pub(super) raw_output: Option<String>,
}

#[derive(Default)]
pub(super) struct History {
    items: Vec<(ItemId, Item)>,
    ids: HashSet<ItemId>,
    details: HashMap<ItemId, Detail>,
}

impl History {
    pub(super) fn hides(&self, id: ItemId) -> bool {
        self.ids.contains(&id)
    }

    pub(super) fn keep_detail(&mut self, id: ItemId, detail: Detail) {
        self.details.insert(id, detail);
    }
}

impl UiState {
    pub(super) fn split_history<'a>(
        &mut self,
        items: &'a [(ItemId, Item)],
    ) -> &'a [(ItemId, Item)] {
        let start = items.len().saturating_sub(WINDOW);
        let older = &items[..start];
        self.history = History {
            items: older.to_vec(),
            ids: older.iter().map(|(id, _)| *id).collect(),
            details: HashMap::new(),
        };
        self.show_has_earlier();
        &items[start..]
    }

    pub(super) fn clear_history(&mut self) {
        self.history = History::default();
        self.show_has_earlier();
    }

    pub(super) fn load_earlier(&mut self, count: usize) {
        let take = count.min(self.history.items.len());
        if take == 0 {
            return;
        }
        let at = self.history.items.len() - take;
        let chunk = self.history.items.split_off(at);
        let rows: Vec<ChatItem> = chunk
            .iter()
            .map(|(id, item)| self.chat_item(*id, item))
            .collect();
        for index in self.rows.values_mut() {
            *index += take;
        }
        self.turn.shift_rows(take);
        for (offset, ((id, _), row)) in chunk.iter().zip(rows).enumerate() {
            self.history.ids.remove(id);
            self.rows.insert(*id, offset);
            self.chat.insert(offset, row);
        }
        for (id, item) in &chunk {
            if let Some(d) = self.history.details.remove(id) {
                self.tool_detail(
                    *id,
                    d.kind.as_deref(),
                    &d.content,
                    &d.locations,
                    d.raw_input.as_deref(),
                    d.raw_output.as_deref(),
                );
            }
            if matches!(item, Item::Assistant { .. }) {
                self.plan_changes(*id, ChangeDirection::Undo);
            }
        }
        for index in 0..=take.min(self.chat.row_count().saturating_sub(1)) {
            self.regroup(index);
        }
        let limit = self.chat.row_count();
        let mut index = 0;
        let mut boundary = false;
        while index < limit {
            let kind = self.chat.row_data(index).map_or(0, |r| r.kind);
            if index >= take && (boundary || kind == 0 || kind == 6) {
                break;
            }
            if index >= take && kind == 1 {
                boundary = true;
            }
            self.mark_continued(index);
            index += 1;
        }
        self.show_has_earlier();
    }

    pub(super) fn load_all_earlier(&mut self) {
        self.load_earlier(usize::MAX);
    }

    fn show_has_earlier(&self) {
        self.window
            .global::<ChatUi>()
            .set_has_earlier(!self.history.items.is_empty());
    }
}

pub(super) fn wire(window: &crate::ui::AppWindow) {
    window
        .global::<ChatUi>()
        .on_load_earlier(|| with_state(|s| s.load_earlier(WINDOW)));
}
