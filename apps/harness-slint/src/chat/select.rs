//! Selecting text across blocks of the chat.
//!
//! Every paragraph, heading, list item and code block is a text field of its own, and a text field's
//! selection ends at its edge: dragging from one paragraph into the next used to stop there. This module
//! follows the pointer through the window's own events (a field that is being dragged in keeps the mouse to
//! itself, so nothing else in the page hears where the pointer is) and tells the page the span between the
//! field the drag began in and the pointer. Fields in that span select themselves whole and report their
//! text; Ctrl+C in the first field then copies its part and theirs, top to bottom.

use std::cell::RefCell;
use std::collections::BTreeMap;

use slint::ComponentHandle;

use crate::ui::{AppWindow, ChatUi};

#[derive(Default)]
struct Drag {
    /// The left button is down, and where the pointer is (logical pixels from the window's top).
    down: bool,
    y: f32,
    /// The field the selection began in: its top and bottom. `None` until it has a selection.
    origin: Option<(f32, f32)>,
    /// The span told to the page.
    span: (f32, f32),
    /// The fields that joined, by (top, left) in whole pixels, and their text.
    joined: BTreeMap<(i32, i32), String>,
}

thread_local! {
    static DRAG: RefCell<Drag> = RefCell::default();
}

/// The part of the page between the first field and the pointer: below the field when the pointer is under
/// it, above when over it, nothing while the pointer is still inside.
fn span(origin: (f32, f32), y: f32) -> (f32, f32) {
    if y > origin.1 {
        (origin.1, y)
    } else if y < origin.0 {
        (y, origin.0)
    } else {
        (0.0, 0.0)
    }
}

fn show(ui: &ChatUi<'_>, drag: &mut Drag) {
    let Some(origin) = drag.origin else { return };
    let next = span(origin, drag.y);
    if next != drag.span {
        drag.span = next;
        ui.set_drag_lo(next.0);
        ui.set_drag_hi(next.1);
    }
}

/// The pointer moved; `y` in logical pixels.
pub fn moved(window: &AppWindow, y: f32) {
    DRAG.with(|cell| {
        let mut drag = cell.borrow_mut();
        drag.y = y;
        if drag.down && drag.origin.is_some() {
            show(&window.global::<ChatUi>(), &mut drag);
        }
    });
}

/// The left button went down or came up. A new press ends the selection that stood.
pub fn button(window: &AppWindow, down: bool) {
    DRAG.with(|cell| {
        let mut drag = cell.borrow_mut();
        drag.down = down;
        if down && drag.origin.take().is_some() {
            drag.span = (0.0, 0.0);
            drag.joined.clear();
            let ui = window.global::<ChatUi>();
            ui.set_drag_on(false);
            ui.set_drag_lo(0.0);
            ui.set_drag_hi(0.0);
        }
    });
}

/// The text of everything selected, top to bottom: `origin` is the first field's place and selected part.
fn gather(joined: &BTreeMap<(i32, i32), String>, origin: ((i32, i32), &str)) -> String {
    let mut parts: Vec<((i32, i32), &str)> = joined
        .iter()
        .map(|(at, text)| (*at, text.as_str()))
        .collect();
    parts.push(origin);
    parts.sort_by_key(|(at, _)| *at);
    parts
        .iter()
        .map(|(_, text)| text.trim_end())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn install(window: &AppWindow, copy: impl Fn(&str) + 'static) {
    let ui = window.global::<ChatUi>();
    let weak = window.as_weak();
    ui.on_drag_begin(move |top, bottom| {
        let Some(window) = weak.upgrade() else { return };
        DRAG.with(|cell| {
            let mut drag = cell.borrow_mut();
            if !drag.down || drag.origin.is_some() {
                return;
            }
            drag.origin = Some((top, bottom));
            let ui = window.global::<ChatUi>();
            show(&ui, &mut drag);
            ui.set_drag_on(true);
        });
    });
    ui.on_drag_join(|top, left, text| {
        DRAG.with(|cell| {
            cell.borrow_mut().joined.insert((top, left), text.into());
        });
    });
    ui.on_drag_part(|top, left| {
        DRAG.with(|cell| {
            cell.borrow_mut().joined.remove(&(top, left));
        });
    });
    ui.on_drag_copy(move |text, from, to, top, left| {
        let (from, to) = (from.min(to), from.max(to));
        let part = usize::try_from(from)
            .ok()
            .zip(usize::try_from(to).ok())
            .and_then(|(from, to)| text.as_str().get(from..to))
            .unwrap_or_default();
        let all = DRAG.with(|cell| gather(&cell.borrow().joined, ((top, left), part)));
        copy(&all);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_span_runs_from_the_first_field_to_the_pointer() {
        let origin = (100.0, 140.0);
        assert_eq!(span(origin, 120.0), (0.0, 0.0));
        assert_eq!(span(origin, 300.0), (140.0, 300.0));
        assert_eq!(span(origin, 40.0), (40.0, 100.0));
    }

    #[test]
    fn what_is_copied_reads_top_to_bottom() {
        let mut joined = BTreeMap::new();
        joined.insert((300, 20), "third paragraph\n".to_owned());
        joined.insert((200, 20), "second paragraph".to_owned());
        joined.insert((200, 260), "beside it".to_owned());
        joined.insert((400, 20), "  ".to_owned());
        assert_eq!(
            gather(&joined, ((100, 20), "end of the first")),
            "end of the first\nsecond paragraph\nbeside it\nthird paragraph"
        );
        // Dragging upwards: the first field is the last to read.
        assert_eq!(
            gather(&joined, ((350, 20), "start of the last"))
                .lines()
                .last(),
            Some("start of the last")
        );
    }
}
