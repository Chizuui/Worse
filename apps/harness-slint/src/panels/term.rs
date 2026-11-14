//! Terminal emulation for the Terminal panel: `alacritty_terminal::Term` (VT parser + grid, scrollback 2 000)
//! fed with the PTY output in order; snapshots of the visible screen become styled runs for Slint.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};

pub(crate) const SCROLLBACK: usize = 2_000;

/// Colour code shared with `PanelTerminal` (see `globals.slint`): -1 default, -2 opposite default, -3 rgb,
/// 0..15 theme ANSI palette.
pub(crate) const DEFAULT: i32 = -1;
const OPPOSITE: i32 = -2;
const RGB: i32 = -3;

#[derive(Debug, Clone, Copy)]
struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// Collects terminal replies (`PtyWrite`: cursor position reports, device attributes) for the PTY.
#[derive(Clone, Default)]
struct Listener {
    replies: Arc<Mutex<Vec<String>>>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            if let Ok(mut replies) = self.replies.lock() {
                replies.push(text);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Run {
    pub col: u16,
    pub cells: u16,
    pub text: String,
    pub fg: i32,
    pub fg_rgb: (u8, u8, u8),
    pub bg: i32,
    pub bg_rgb: (u8, u8, u8),
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub(crate) struct Line {
    pub text: String,
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Screen {
    pub lines: Vec<Line>,
    pub cursor: Option<(usize, usize)>,
}

pub(crate) struct Emulator {
    term: Term<Listener>,
    parser: Processor,
    listener: Listener,
}

impl Emulator {
    pub(crate) fn new(cols: u16, rows: u16) -> Self {
        let listener = Listener::default();
        let size = Size {
            cols: usize::from(cols.max(2)),
            rows: usize::from(rows.max(2)),
        };
        let config = Config {
            scrolling_history: SCROLLBACK,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size, listener.clone()),
            parser: Processor::new(),
            listener,
        }
    }

    /// Feeds PTY output; returns replies the terminal wants written back to the PTY.
    pub(crate) fn feed(&mut self, data: &str) -> Vec<String> {
        self.parser.advance(&mut self.term, data.as_bytes());
        self.listener
            .replies
            .lock()
            .map(|mut r| std::mem::take(&mut *r))
            .unwrap_or_default()
    }

    pub(crate) fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(Size {
            cols: usize::from(cols.max(2)),
            rows: usize::from(rows.max(2)),
        });
    }

    /// Scrolls the viewport into the scrollback (positive = up / older).
    pub(crate) fn scroll(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
    }

    /// Back to the live screen (on input).
    pub(crate) fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// Whether the application asked for application cursor keys (`DECCKM`).
    pub(crate) fn app_cursor(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    /// Visible screen as styled runs (trailing blank cells dropped).
    pub(crate) fn screen(&self) -> Screen {
        let content = self.term.renderable_content();
        let rows = self.term.screen_lines();
        let offset = i32::try_from(content.display_offset).unwrap_or(0);
        let mut builders: Vec<Vec<Run>> = vec![Vec::new(); rows];
        for indexed in content.display_iter {
            let Ok(row) = usize::try_from(indexed.point.line.0 + offset) else {
                continue;
            };
            let Some(runs) = builders.get_mut(row) else {
                continue;
            };
            let cell = indexed.cell;
            let col = u16::try_from(indexed.point.column.0).unwrap_or(u16::MAX);
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                if let Some(last) = runs.last_mut() {
                    last.cells = last.cells.saturating_add(1);
                }
                continue;
            }
            let (mut fg, mut fg_rgb) = color(cell.fg);
            let (mut bg, mut bg_rgb) = color(cell.bg);
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
                std::mem::swap(&mut fg_rgb, &mut bg_rgb);
                if fg == DEFAULT {
                    fg = OPPOSITE;
                }
                if bg == DEFAULT {
                    bg = OPPOSITE;
                }
            }
            let ch = if cell.flags.contains(Flags::HIDDEN) || cell.c == '\t' {
                ' '
            } else {
                cell.c
            };
            let bold = cell.flags.contains(Flags::BOLD);
            let italic = cell.flags.contains(Flags::ITALIC);
            let underline = cell.flags.contains(Flags::UNDERLINE);
            match runs.last_mut() {
                Some(last)
                    if last.fg == fg
                        && last.fg_rgb == fg_rgb
                        && last.bg == bg
                        && last.bg_rgb == bg_rgb
                        && last.bold == bold
                        && last.italic == italic
                        && last.underline == underline
                        && last.col.saturating_add(last.cells) == col =>
                {
                    last.text.push(ch);
                    last.cells = last.cells.saturating_add(1);
                }
                _ => runs.push(Run {
                    col,
                    cells: 1,
                    text: ch.to_string(),
                    fg,
                    fg_rgb,
                    bg,
                    bg_rgb,
                    bold,
                    italic,
                    underline,
                }),
            }
        }
        let lines = builders.into_iter().map(finish_line).collect();
        let cursor = (content.display_offset == 0 && content.mode.contains(TermMode::SHOW_CURSOR))
            .then(|| {
                let point = content.cursor.point;
                usize::try_from(point.line.0)
                    .ok()
                    .map(|row| (row, point.column.0))
            })
            .flatten();
        Screen { lines, cursor }
    }
}

/// Drops trailing blank cells (they draw nothing) and sets the row's plain text.
fn finish_line(mut runs: Vec<Run>) -> Line {
    while runs
        .last()
        .is_some_and(|r| r.bg == DEFAULT && !r.underline && r.text.trim().is_empty())
    {
        runs.pop();
    }
    if let Some(last) = runs.last_mut() {
        if last.bg == DEFAULT && !last.underline {
            let trimmed = last.text.trim_end().chars().count();
            let dropped = last.text.chars().count() - trimmed;
            last.text = last.text.chars().take(trimmed).collect();
            last.cells = last
                .cells
                .saturating_sub(u16::try_from(dropped).unwrap_or(0));
        }
    }
    let text: String = runs.iter().map(|r| r.text.as_str()).collect();
    Line {
        text: text.trim_end().to_owned(),
        runs,
    }
}

/// 256-colour cube / grey ramp entry.
fn indexed_rgb(index: u8) -> (u8, u8, u8) {
    if index >= 232 {
        let level = 8 + (index - 232) * 10;
        return (level, level, level);
    }
    let i = index - 16;
    let step = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
    (step(i / 36), step((i / 6) % 6), step(i % 6))
}

fn color(value: Color) -> (i32, (u8, u8, u8)) {
    match value {
        Color::Spec(rgb) => (RGB, (rgb.r, rgb.g, rgb.b)),
        Color::Indexed(i) if i < 16 => (i32::from(i), (0, 0, 0)),
        Color::Indexed(i) => (RGB, indexed_rgb(i)),
        Color::Named(named) => {
            let n = named as usize;
            if n < 16 {
                (i32::try_from(n).unwrap_or(DEFAULT), (0, 0, 0))
            } else {
                match named {
                    NamedColor::DimBlack => (0, (0, 0, 0)),
                    NamedColor::DimRed => (1, (0, 0, 0)),
                    NamedColor::DimGreen => (2, (0, 0, 0)),
                    NamedColor::DimYellow => (3, (0, 0, 0)),
                    NamedColor::DimBlue => (4, (0, 0, 0)),
                    NamedColor::DimMagenta => (5, (0, 0, 0)),
                    NamedColor::DimCyan => (6, (0, 0, 0)),
                    NamedColor::DimWhite => (7, (0, 0, 0)),
                    _ => (DEFAULT, (0, 0, 0)),
                }
            }
        }
    }
}

/// Maps a Slint key event (text + modifiers) to bytes for the PTY.
pub(crate) fn key_bytes(text: &str, ctrl: bool, alt: bool, app_cursor: bool) -> Option<String> {
    use slint::platform::Key;
    let mut chars = text.chars();
    let first = chars.next()?;
    let single = chars.next().is_none();
    let cursor = |c: char| {
        if app_cursor {
            format!("\x1bO{c}")
        } else {
            format!("\x1b[{c}")
        }
    };
    let special = [
        (Key::Return, "\r".to_owned()),
        (Key::Backspace, "\x7f".to_owned()),
        (Key::Tab, "\t".to_owned()),
        (Key::Escape, "\x1b".to_owned()),
        (Key::UpArrow, cursor('A')),
        (Key::DownArrow, cursor('B')),
        (Key::RightArrow, cursor('C')),
        (Key::LeftArrow, cursor('D')),
        (Key::Home, "\x1b[H".to_owned()),
        (Key::End, "\x1b[F".to_owned()),
        (Key::Delete, "\x1b[3~".to_owned()),
        (Key::PageUp, "\x1b[5~".to_owned()),
        (Key::PageDown, "\x1b[6~".to_owned()),
        (Key::Insert, "\x1b[2~".to_owned()),
    ];
    if single {
        if let Some((_, bytes)) = special.iter().find(|(key, _)| char::from(*key) == first) {
            return Some(bytes.clone());
        }
        // Other Slint function / modifier keys live in the private-use area: nothing to send.
        if ('\u{f700}'..='\u{f8ff}').contains(&first) || matches!(first, '\u{10}'..='\u{18}') {
            return None;
        }
        if ctrl && first.is_ascii_alphabetic() {
            let code = u8::try_from(first.to_ascii_lowercase()).unwrap_or(b'a') & 0x1f;
            return Some(char::from(code).to_string());
        }
        if ctrl && first == ' ' {
            return Some("\0".to_owned());
        }
    }
    if text == "\n" {
        return Some("\r".to_owned());
    }
    Some(if alt {
        format!("\x1b{text}")
    } else {
        text.to_owned()
    })
}
