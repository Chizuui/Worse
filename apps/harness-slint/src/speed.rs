//! Generation speed of a turn ("tok/s"), shown beside the working line and under the finished answer
//! (Settings › Appearance › "Show generation speed").
//!
//! Two figures:
//!
//! - While the agent writes: an estimate from the text that arrives, at four characters per token, over the
//!   time text was actually arriving. No agent reports token counts in the middle of a turn. Shown with "≈".
//! - After the turn: the exact figure for agents that report the output tokens of a prompt (ACP
//!   `session/prompt` result `usage`: the built-in API agent, a Hermes gateway, and any other agent that
//!   returns it). Agents that report nothing keep the estimate, still marked "≈".
//!
//! The clock only runs while text arrives: a gap of more than two seconds (a tool is running, the agent waits
//! for an approval) is not generation time.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use slint::ComponentHandle;

use crate::core::ItemId;
use crate::ui::{AppWindow, ChatUi};

/// Characters per token of the estimate.
const CHARS_PER_TOKEN: f64 = 4.0;
/// A pause longer than this between two pieces of text is not generation time.
const GAP: Duration = Duration::from_secs(2);
/// The live label is refreshed at most this often.
const REFRESH: Duration = Duration::from_millis(400);
/// Shorter bursts give no meaningful rate.
const MIN_STREAM: Duration = Duration::from_millis(300);

#[derive(Default)]
struct State {
    /// Characters of answer and thinking text of the running turn.
    chars: usize,
    /// Time during which text was arriving.
    streaming: Duration,
    last: Option<Instant>,
    shown: Option<Instant>,
    /// Row of the answer being written; it gets the final figure.
    row: Option<ItemId>,
    /// A finished turn whose exact token count may still arrive: its row and its streaming time.
    awaiting: Option<(ItemId, Duration)>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::default();
}

/// Tokens per second for `tokens` over `streaming`, or `None` when the burst was too short to say.
fn rate(tokens: f64, streaming: Duration) -> Option<f64> {
    (streaming >= MIN_STREAM && tokens > 0.0).then(|| tokens / streaming.as_secs_f64())
}

fn round(rate: f64) -> String {
    if rate >= 10.0 {
        format!("{rate:.0}")
    } else {
        format!("{rate:.1}")
    }
}

/// Label of an estimate: "≈ 38 tok/s".
fn estimated(chars: usize, streaming: Duration) -> Option<String> {
    rate(chars as f64 / CHARS_PER_TOKEN, streaming).map(|r| format!("≈ {} tok/s", round(r)))
}

/// Label of a reported count: "41 tok/s · 85 tokens".
fn exact(tokens: u64, streaming: Duration) -> Option<String> {
    rate(tokens as f64, streaming).map(|r| format!("{} tok/s · {tokens} tokens", round(r)))
}

/// A turn started: the count starts over.
pub(crate) fn start(window: &AppWindow) {
    STATE.with(|cell| {
        let mut s = cell.borrow_mut();
        s.chars = 0;
        s.streaming = Duration::ZERO;
        s.last = None;
        s.shown = None;
        s.row = None;
    });
    window.global::<ChatUi>().set_speed(slint::SharedString::new());
}

/// Another session is shown: nothing of the previous one is pending.
pub(crate) fn reset(window: &AppWindow) {
    STATE.with(|cell| *cell.borrow_mut() = State::default());
    window.global::<ChatUi>().set_speed(slint::SharedString::new());
}

/// Text of the running turn arrived: answer text of `row`, or thinking text (`None`).
pub(crate) fn text(window: &AppWindow, row: Option<ItemId>, chars: usize) {
    let label = STATE.with(|cell| {
        let mut s = cell.borrow_mut();
        let now = Instant::now();
        if let Some(last) = s.last {
            let gap = now.duration_since(last);
            if gap <= GAP {
                s.streaming += gap;
            }
        }
        s.last = Some(now);
        s.chars += chars;
        if row.is_some() {
            s.row = row;
        }
        if s.shown.is_some_and(|t| now.duration_since(t) < REFRESH) {
            return None;
        }
        s.shown = Some(now);
        estimated(s.chars, s.streaming)
    });
    if let Some(label) = label {
        window.global::<ChatUi>().set_speed(label.as_str().into());
    }
}

/// Once a second while a turn runs: the live label goes away when no text arrived for a while (a tool is
/// running), so it never shows a stale rate.
pub(crate) fn tick(window: &AppWindow) {
    let stale = STATE.with(|cell| cell.borrow().last.is_none_or(|t| t.elapsed() > GAP));
    let ui = window.global::<ChatUi>();
    if stale && !ui.get_speed().is_empty() {
        ui.set_speed(slint::SharedString::new());
    }
}

/// The turn ended: the estimate for its answer row, which stays until an exact count arrives ([`reported`]).
pub(crate) fn finish(window: &AppWindow) -> Option<(ItemId, String)> {
    window.global::<ChatUi>().set_speed(slint::SharedString::new());
    STATE.with(|cell| {
        let mut s = cell.borrow_mut();
        let row = s.row.take()?;
        let streaming = s.streaming;
        s.awaiting = Some((row, streaming));
        estimated(s.chars, streaming).map(|label| (row, label))
    })
}

/// The agent reported the output tokens of the turn that just ended: the exact figure for its answer row.
pub(crate) fn reported(tokens: u64) -> Option<(ItemId, String)> {
    STATE.with(|cell| {
        let (row, streaming) = cell.borrow_mut().awaiting.take()?;
        exact(tokens, streaming).map(|label| (row, label))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_uses_four_characters_per_token() {
        // 800 characters in 5 s: 200 tokens, 40 a second.
        assert_eq!(estimated(800, Duration::from_secs(5)).as_deref(), Some("≈ 40 tok/s"));
        // Slow rates keep one decimal.
        assert_eq!(estimated(60, Duration::from_secs(4)).as_deref(), Some("≈ 3.8 tok/s"));
    }

    #[test]
    fn exact_shows_the_reported_count() {
        assert_eq!(exact(85, Duration::from_secs(2)).as_deref(), Some("42 tok/s · 85 tokens"));
    }

    #[test]
    fn too_short_or_empty_gives_nothing() {
        assert_eq!(estimated(400, Duration::from_millis(100)), None);
        assert_eq!(estimated(0, Duration::from_secs(3)), None);
        assert_eq!(exact(0, Duration::from_secs(3)), None);
    }
}
