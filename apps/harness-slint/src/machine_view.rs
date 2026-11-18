//! Computer use, live: the "Screen" view of the Live tab.
//!
//! The `harness-computer` MCP server runs in the agent's process tree, not here. Before each action it
//! appends one line to `<home>/machine-live/<session>/events.jsonl` and, after a screenshot, writes
//! `screen.jpg` next to it (`computer_mcp.rs`). While the open session has Machine mode on, this module reads
//! those files on a short timer and shows the screen the agent saw with its pointer on top: position, click
//! ripple, what it typed, and the agent's name on the pointer.
//!
//! Nothing is polled for sessions without Machine mode. The files of earlier runs are removed at startup.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use slint::{
    ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode, VecModel,
};

use crate::ui::{AppWindow, LiveUi, ScreenAct, ScreenUi};

/// How often the event log is checked while Machine mode is on.
const POLL: Duration = Duration::from_millis(150);
/// The view says "LIVE" for this long after the last action.
const BUSY: Duration = Duration::from_secs(5);
/// Actions listed under the screenshot.
const LOG: usize = 8;

#[derive(Default)]
struct State {
    home: PathBuf,
    /// Open session with Machine mode on ("" = none).
    session: String,
    /// Bytes of `events.jsonl` already read.
    offset: u64,
    log: VecDeque<(String, String)>,
    seq: i32,
    last_event: Option<Instant>,
    /// One automatic jump to the Screen view per turn.
    opened_this_turn: bool,
    timer: Timer,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::default();
}

/// Newest screenshot load; an older one that finishes later is dropped.
static SHOT: AtomicU64 = AtomicU64::new(0);

/// One parsed line of the event log.
#[derive(Debug, Clone, PartialEq)]
struct Event {
    kind: String,
    text: String,
    /// Pointer position as a fraction of the screenshot, when the action has one.
    at: Option<(f32, f32)>,
    /// Unix milliseconds.
    t: u64,
}

fn parse(line: &str) -> Option<Event> {
    let v: Value = serde_json::from_str(line).ok()?;
    let kind = v.get("kind")?.as_str()?.to_owned();
    let (w, h) = (
        v.get("w").and_then(Value::as_f64),
        v.get("h").and_then(Value::as_f64),
    );
    let at = match (
        v.get("x").and_then(Value::as_f64),
        v.get("y").and_then(Value::as_f64),
        w,
        h,
    ) {
        (Some(x), Some(y), Some(w), Some(h)) if w > 0.0 && h > 0.0 => Some((
            (x / w).clamp(0.0, 1.0) as f32,
            (y / h).clamp(0.0, 1.0) as f32,
        )),
        _ => None,
    };
    Some(Event {
        kind,
        text: v
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        at,
        t: v.get("t").and_then(Value::as_u64).unwrap_or(0),
    })
}

/// Text shown on a bubble by the pointer: what was typed, pressed or scrolled (clicks need no words).
fn caption(event: &Event) -> &str {
    match event.kind.as_str() {
        "type" | "key" | "scroll" => &event.text,
        _ => "",
    }
}

/// Remembers the data directory and removes what earlier runs left behind (screenshots of the screen should
/// not outlive the app).
pub(crate) fn install(_window: &AppWindow, home: &Path) {
    let _ = std::fs::remove_dir_all(home.join("machine-live"));
    STATE.with(|cell| cell.borrow_mut().home = home.to_path_buf());
}

/// The open session, its Machine mode and the agent at its controls changed (dock options).
pub(crate) fn set_session(
    window: &AppWindow,
    session: Option<&str>,
    machine: bool,
    agent: &str,
    agent_name: &str,
) {
    let screen = window.global::<ScreenUi>();
    screen.set_agent(agent.into());
    screen.set_agent_name(agent_name.into());
    let wanted = session.filter(|_| machine).unwrap_or_default();
    let changed = STATE.with(|cell| {
        let mut s = cell.borrow_mut();
        if s.session == wanted {
            return false;
        }
        wanted.clone_into(&mut s.session);
        s.offset = 0;
        s.log.clear();
        s.last_event = None;
        s.timer.stop();
        true
    });
    screen.set_available(!wanted.is_empty());
    if !changed {
        return;
    }
    SHOT.fetch_add(1, Ordering::SeqCst);
    screen.set_has_shot(false);
    screen.set_shot(Image::default());
    screen.set_busy(false);
    screen.set_caption(slint::SharedString::new());
    screen.set_log(ModelRc::new(VecModel::<ScreenAct>::default()));
    if wanted.is_empty() {
        // Nothing to mirror: the Live tab goes back to code.
        window.global::<LiveUi>().set_view("code".into());
        return;
    }
    let weak = window.as_weak();
    STATE.with(|cell| {
        cell.borrow()
            .timer
            .start(TimerMode::Repeated, POLL, move || {
                if let Some(window) = weak.upgrade() {
                    tick(&window);
                }
            });
    });
}

/// A new user turn: the first computer action of it may bring the Screen view to the front again.
pub(crate) fn new_turn() {
    STATE.with(|cell| cell.borrow_mut().opened_this_turn = false);
}

fn dir() -> Option<PathBuf> {
    STATE.with(|cell| {
        let s = cell.borrow();
        (!s.session.is_empty()).then(|| crate::computer_mcp::live_dir(&s.home, &s.session))
    })
}

fn tick(window: &AppWindow) {
    let Some(dir) = dir() else {
        return;
    };
    let events = read_new(&dir.join("events.jsonl"));
    let screen = window.global::<ScreenUi>();
    if events.is_empty() {
        let idle = STATE.with(|cell| cell.borrow().last_event.is_none_or(|t| t.elapsed() > BUSY));
        if idle && screen.get_busy() {
            screen.set_busy(false);
        }
        return;
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    let mut fresh = false;
    let mut shot = false;
    for event in &events {
        // Events read when a session is opened may be old: they fill the log but are not "live".
        let recent =
            now_ms.saturating_sub(event.t) < u64::try_from(BUSY.as_millis()).unwrap_or(5000);
        fresh |= recent;
        shot |= event.kind == "screenshot";
        if let Some((x, y)) = event.at {
            screen.set_px(x);
            screen.set_py(y);
        }
        screen.set_kind(event.kind.as_str().into());
        screen.set_caption(if recent {
            caption(event).into()
        } else {
            slint::SharedString::new()
        });
        STATE.with(|cell| {
            let mut s = cell.borrow_mut();
            s.log.push_front((event.kind.clone(), event.text.clone()));
            s.log.truncate(LOG);
            if recent {
                s.seq = s.seq.wrapping_add(1);
                s.last_event = Some(Instant::now());
            }
        });
    }
    let (rows, seq, open) = STATE.with(|cell| {
        let mut s = cell.borrow_mut();
        let rows: Vec<ScreenAct> = s
            .log
            .iter()
            .map(|(kind, text)| ScreenAct {
                kind: kind.as_str().into(),
                text: text.as_str().into(),
            })
            .collect();
        let open = fresh && !s.opened_this_turn;
        s.opened_this_turn |= fresh;
        (rows, s.seq, open)
    });
    screen.set_log(ModelRc::new(VecModel::from(rows)));
    screen.set_seq(seq);
    if fresh {
        screen.set_busy(true);
        // Following the agent: the Live tab shows the screen now, and opens once per turn.
        let live = window.global::<LiveUi>();
        if live.get_follow() {
            live.set_view("screen".into());
            if open {
                crate::live::request_open(window);
            }
        }
    }
    if shot || !screen.get_has_shot() {
        load_shot(window, dir.join("screen.jpg"));
    }
}

/// Complete lines appended to the log since the last read. A log that shrank was started over.
fn read_new(path: &Path) -> Vec<Event> {
    let Ok(meta) = std::fs::metadata(path) else {
        return Vec::new();
    };
    let offset = STATE.with(|cell| {
        let mut s = cell.borrow_mut();
        if meta.len() < s.offset {
            s.offset = 0;
        }
        s.offset
    });
    if meta.len() == offset {
        return Vec::new();
    }
    let mut text = String::new();
    let read = std::fs::File::open(path).and_then(|mut file| {
        file.seek(SeekFrom::Start(offset))?;
        file.take(meta.len() - offset).read_to_string(&mut text)
    });
    if read.is_err() {
        return Vec::new();
    }
    // A line still being written (no newline yet) is left for the next read.
    let complete = text.rfind('\n').map_or(0, |i| i + 1);
    STATE.with(|cell| cell.borrow_mut().offset = offset + complete as u64);
    text[..complete].lines().filter_map(parse).collect()
}

/// Decodes the screenshot copy off the UI thread and shows it.
fn load_shot(window: &AppWindow, path: PathBuf) {
    let generation = SHOT.fetch_add(1, Ordering::SeqCst) + 1;
    let weak = window.as_weak();
    std::thread::spawn(move || {
        let Ok(image) = image::open(&path) else {
            return;
        };
        let image = image.into_rgba8();
        let (w, h) = image.dimensions();
        if w == 0 || h == 0 {
            return;
        }
        let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(image.as_raw(), w, h);
        let _ = slint::invoke_from_event_loop(move || {
            if SHOT.load(Ordering::SeqCst) != generation {
                return;
            }
            if let Some(window) = weak.upgrade() {
                let screen = window.global::<ScreenUi>();
                screen.set_ratio(h as f32 / w as f32);
                screen.set_shot(Image::from_rgba8(buffer));
                screen.set_has_shot(true);
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_turns_pixels_into_fractions() {
        let e = parse(r#"{"seq":3,"t":1700000000000,"kind":"click","text":"left at 640, 360","w":1280,"h":720,"x":640.0,"y":360.0}"#).unwrap();
        assert_eq!(e.kind, "click");
        assert_eq!(e.at, Some((0.5, 0.5)));
        assert_eq!(e.t, 1_700_000_000_000);
        // No screenshot yet (w = h = 0): the action has no place on the picture.
        let e =
            parse(r#"{"seq":1,"t":1,"kind":"move","text":"to 5, 5","w":0,"h":0,"x":5.0,"y":5.0}"#)
                .unwrap();
        assert_eq!(e.at, None);
        assert!(parse("not json").is_none());
        assert!(parse(r#"{"text":"no kind"}"#).is_none());
    }

    #[test]
    fn caption_only_for_typing_keys_and_scrolling() {
        let event = |kind: &str| Event {
            kind: kind.to_owned(),
            text: "words".to_owned(),
            at: None,
            t: 0,
        };
        assert_eq!(caption(&event("type")), "words");
        assert_eq!(caption(&event("key")), "words");
        assert_eq!(caption(&event("scroll")), "words");
        assert_eq!(caption(&event("click")), "");
        assert_eq!(caption(&event("screenshot")), "");
    }
}
