#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

use std::fmt::Write as _;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use serde_json::{json, Value};

const MAX_SIDE: u32 = 1568;
/// Widest screenshot copy kept for the app's live view (`machine_view.rs`).
const MIRROR_WIDTH: u32 = 1280;
/// The event log starts over when it grows past this.
const EVENTS_MAX: u64 = 512 * 1024;
const OFF: &str =
    "Machine mode is off for this session. Ask the user to switch the mode to Machine.";

#[derive(Clone, Copy)]
struct Frame {
    x: i32,
    y: i32,
    scale: f64,
}

struct Server {
    home: PathBuf,
    session: String,
    frame: Option<Frame>,
    enigo: Option<Enigo>,
    /// Size of the last screenshot in its own pixels (the unit of every tool coordinate).
    shot: (u32, u32),
    seq: u64,
}

/// Directory the app's live view reads: `events.jsonl` (one line per action) and `screen.jpg` (the last
/// screenshot). Kept apart from the mode gate files, which are listed as "sessions with Machine mode on".
pub(crate) fn live_dir(home: &std::path::Path, session: &str) -> PathBuf {
    home.join("machine-live")
        .join(harness_agent_hub::machine::gate_name(session))
}

pub fn run() -> ExitCode {
    let Some(home) = std::env::var_os("HARNESS_HOME").filter(|v| !v.is_empty()) else {
        eprintln!("harness computer-mcp: HARNESS_HOME is not set");
        return ExitCode::FAILURE;
    };
    let mut server = Server {
        home: PathBuf::from(home),
        session: std::env::var("HARNESS_MACHINE_SESSION").unwrap_or_default(),
        frame: None,
        enigo: None,
        shot: (0, 0),
        seq: 0,
    };
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let reply = match method {
            "initialize" => json!({"jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": params.get("protocolVersion").cloned().unwrap_or_else(|| json!("2024-11-05")),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "harness-computer", "version": env!("CARGO_PKG_VERSION")}
            }}),
            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools()}}),
            "tools/call" => json!({"jsonrpc": "2.0", "id": id, "result": server.call(&params)}),
            other => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("unknown method {other}")}})
            }
        };
        if writeln!(out, "{reply}").and_then(|()| out.flush()).is_err() {
            break;
        }
    }
    ExitCode::SUCCESS
}

fn point_schema(extra: &Value) -> Value {
    let mut properties = json!({
        "x": {"type": "number", "description": "X in pixels of the last screenshot"},
        "y": {"type": "number", "description": "Y in pixels of the last screenshot"}
    });
    if let (Some(base), Some(more)) = (properties.as_object_mut(), extra.as_object()) {
        for (k, v) in more {
            base.insert(k.clone(), v.clone());
        }
    }
    json!({"type": "object", "properties": properties, "required": ["x", "y"]})
}

fn tools() -> Value {
    json!([
        {"name": "screenshot", "description": "Capture a monitor of this computer (primary by default). Coordinates of the other tools are pixels of the last screenshot.",
         "inputSchema": {"type": "object", "properties": {"monitor": {"type": "integer", "description": "Monitor index from the screenshot text"}}}},
        {"name": "click", "description": "Click at a point of the last screenshot.",
         "inputSchema": point_schema(&json!({"button": {"type": "string", "enum": ["left", "right", "middle"]}, "count": {"type": "integer", "description": "1 = click, 2 = double click"}}))},
        {"name": "move", "description": "Move the mouse to a point of the last screenshot.", "inputSchema": point_schema(&json!({}))},
        {"name": "drag", "description": "Drag with the left button from (x, y) to (to_x, to_y).",
         "inputSchema": {"type": "object", "properties": {"x": {"type": "number"}, "y": {"type": "number"}, "to_x": {"type": "number"}, "to_y": {"type": "number"}}, "required": ["x", "y", "to_x", "to_y"]}},
        {"name": "scroll", "description": "Scroll at a point; dy > 0 scrolls down, dx > 0 scrolls right (wheel notches).",
         "inputSchema": point_schema(&json!({"dx": {"type": "integer"}, "dy": {"type": "integer"}}))},
        {"name": "type", "description": "Type text into the focused control.",
         "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
        {"name": "key", "description": "Press keys, e.g. \"enter\", \"ctrl+c\", \"alt+tab\", \"ctrl+shift+t\". Several combos may be separated by spaces.",
         "inputSchema": {"type": "object", "properties": {"keys": {"type": "string"}}, "required": ["keys"]}},
        {"name": "windows", "description": "List visible windows: title, app and screen rectangle.", "inputSchema": {"type": "object", "properties": {}}},
        {"name": "cursor", "description": "Current mouse position in pixels of the last screenshot.", "inputSchema": {"type": "object", "properties": {}}}
    ])
}

fn text(message: &str) -> Value {
    json!({"content": [{"type": "text", "text": message}]})
}

fn failure(message: &str) -> Value {
    json!({"content": [{"type": "text", "text": message}], "isError": true})
}

fn num(args: &Value, key: &str) -> Result<f64, String> {
    args.get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("missing number {key}"))
}

fn int(args: &Value, key: &str, default: i64) -> i64 {
    args.get(key).and_then(Value::as_i64).unwrap_or(default)
}

fn pause() {
    std::thread::sleep(Duration::from_millis(60));
}

/// Moves the pointer to (x, y) along a short eased path instead of jumping there, so a person watching the
/// screen can follow what the agent does. `HARNESS_MACHINE_GLIDE=0` restores the jump.
fn glide(input: &mut Enigo, x: i32, y: i32) -> Result<(), String> {
    let jump = std::env::var("HARNESS_MACHINE_GLIDE")
        .is_ok_and(|v| v == "0" || v.eq_ignore_ascii_case("false"));
    if let (false, Ok((sx, sy))) = (jump, input.location()) {
        let (dx, dy) = (f64::from(x - sx), f64::from(y - sy));
        let distance = dx.hypot(dy);
        if distance >= 6.0 {
            let (steps, each) = glide_plan(distance);
            for i in 1..steps {
                // Smoothstep: slow start, slow arrival.
                let t = f64::from(i) / f64::from(steps);
                let e = t * t * (3.0 - 2.0 * t);
                input
                    .move_mouse(
                        sx + (dx * e).round() as i32,
                        sy + (dy * e).round() as i32,
                        Coordinate::Abs,
                    )
                    .map_err(|e| e.to_string())?;
                std::thread::sleep(each);
            }
        }
    }
    input
        .move_mouse(x, y, Coordinate::Abs)
        .map_err(|e| e.to_string())
}

/// Steps and pause per step of a glide over `distance` pixels: 120 ms for a short hop, 360 ms across the
/// screen, about one step per 16 ms.
fn glide_plan(distance: f64) -> (i32, Duration) {
    let total_ms = (120.0 + distance * 0.2).clamp(120.0, 360.0);
    let steps = (total_ms / 16.0).round().clamp(6.0, 24.0) as i32;
    (
        steps,
        Duration::from_micros((total_ms * 1000.0 / f64::from(steps)) as u64),
    )
}

/// First characters of typed text for the live view (one line, no more than 60 characters).
fn typed_preview(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut out: String = flat.chars().take(60).collect();
    if flat.chars().count() > 60 {
        out.push('…');
    }
    out
}

impl Server {
    fn call(&mut self, params: &Value) -> Value {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if self.session.is_empty()
            || !harness_agent_hub::machine::gate_path(&self.home, &self.session).exists()
        {
            return failure(OFF);
        }
        let result = match name {
            "screenshot" => self.screenshot(&args),
            "click" => self.click(&args),
            "move" => self.point(&args).and_then(|(x, y)| {
                let at = (num(&args, "x")?, num(&args, "y")?);
                self.emit("move", Some(at), &format!("to {}, {}", at.0, at.1));
                glide(self.input()?, x, y)?;
                Ok(text(&format!("Moved to {}, {}", at.0, at.1)))
            }),
            "drag" => self.drag(&args),
            "scroll" => self.scroll(&args),
            "type" => self.type_text(&args),
            "key" => self.keys(&args),
            "windows" => self.windows(),
            "cursor" => self.cursor(),
            other => Err(format!("unknown tool {other}")),
        };
        result.unwrap_or_else(|error| failure(&error))
    }

    /// Tells the app's live view what is about to happen (never fails the tool: the view is optional).
    /// `at` is a point in screenshot pixels.
    fn emit(&mut self, kind: &str, at: Option<(f64, f64)>, text: &str) {
        self.seq += 1;
        let dir = live_dir(&self.home, &self.session);
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = dir.join("events.jsonl");
        let fresh = std::fs::metadata(&path).is_ok_and(|m| m.len() > EVENTS_MAX);
        let mut event = json!({
            "seq": self.seq,
            "t": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            "kind": kind,
            "text": text,
            "w": self.shot.0,
            "h": self.shot.1,
        });
        if let Some((x, y)) = at {
            event["x"] = json!(x);
            event["y"] = json!(y);
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true);
        if fresh {
            options.write(true).truncate(true);
        } else {
            options.append(true);
        }
        if let Ok(mut file) = options.open(&path) {
            let _ = writeln!(file, "{event}");
        }
    }

    /// Copy of the screenshot for the live view, written whole and then renamed into place.
    fn mirror(&self, image: &image::RgbaImage) {
        let dir = live_dir(&self.home, &self.session);
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let (w, h) = image.dimensions();
        let rgb = if w > MIRROR_WIDTH {
            let height = ((u64::from(h) * u64::from(MIRROR_WIDTH) / u64::from(w)) as u32).max(1);
            image::DynamicImage::ImageRgba8(image::imageops::thumbnail(image, MIRROR_WIDTH, height))
                .into_rgb8()
        } else {
            image::DynamicImage::ImageRgba8(image.clone()).into_rgb8()
        };
        let tmp = dir.join("screen.jpg.tmp");
        let written = std::fs::File::create(&tmp).is_ok_and(|file| {
            let mut out = std::io::BufWriter::new(file);
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 72)
                .encode_image(&rgb)
                .is_ok()
        });
        if written {
            let _ = std::fs::rename(&tmp, dir.join("screen.jpg"));
        }
    }

    fn input(&mut self) -> Result<&mut Enigo, String> {
        if self.enigo.is_none() {
            self.enigo = Some(Enigo::new(&Settings::default()).map_err(|e| e.to_string())?);
        }
        self.enigo
            .as_mut()
            .ok_or_else(|| "input unavailable".to_owned())
    }

    fn frame(&self) -> Result<Frame, String> {
        self.frame
            .ok_or_else(|| "Take a screenshot first so coordinates can be mapped.".to_owned())
    }

    fn map(&self, x: f64, y: f64) -> Result<(i32, i32), String> {
        let f = self.frame()?;
        Ok((
            f.x + (x / f.scale).round() as i32,
            f.y + (y / f.scale).round() as i32,
        ))
    }

    fn point(&self, args: &Value) -> Result<(i32, i32), String> {
        self.map(num(args, "x")?, num(args, "y")?)
    }

    fn screenshot(&mut self, args: &Value) -> Result<Value, String> {
        let monitors = xcap::Monitor::all().map_err(|e| e.to_string())?;
        let chosen = match args.get("monitor").and_then(Value::as_u64) {
            Some(i) => usize::try_from(i).ok().and_then(|i| monitors.get(i)),
            None => monitors
                .iter()
                .find(|m| m.is_primary().unwrap_or(false))
                .or_else(|| monitors.first()),
        }
        .ok_or_else(|| "no such monitor".to_owned())?;
        let image = chosen.capture_image().map_err(|e| e.to_string())?;
        let (w, h) = image.dimensions();
        let long = w.max(h).max(1);
        let scale = if long > MAX_SIDE {
            f64::from(MAX_SIDE) / f64::from(long)
        } else {
            1.0
        };
        let image = if scale < 1.0 {
            image::imageops::resize(
                &image,
                ((f64::from(w) * scale).round() as u32).max(1),
                ((f64::from(h) * scale).round() as u32).max(1),
                image::imageops::FilterType::Triangle,
            )
        } else {
            image
        };
        let mut png = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        self.shot = image.dimensions();
        self.mirror(&image);
        self.emit(
            "screenshot",
            None,
            &format!("{}x{} px", image.width(), image.height()),
        );
        let (mx, my) = (
            chosen.x().map_err(|e| e.to_string())?,
            chosen.y().map_err(|e| e.to_string())?,
        );
        self.frame = Some(Frame {
            x: mx,
            y: my,
            scale,
        });
        let mut info = format!(
            "Screenshot {}x{} px (scale {:.3} of the monitor). Monitors:",
            image.width(),
            image.height(),
            scale
        );
        for (i, m) in monitors.iter().enumerate() {
            let _ = write!(
                info,
                "\n{i}: {} {}x{} at {},{}{}",
                m.name().unwrap_or_default(),
                m.width().unwrap_or_default(),
                m.height().unwrap_or_default(),
                m.x().unwrap_or_default(),
                m.y().unwrap_or_default(),
                if m.is_primary().unwrap_or(false) {
                    " (primary)"
                } else {
                    ""
                }
            );
        }
        Ok(json!({"content": [
            {"type": "image", "data": base64(&png), "mimeType": "image/png"},
            {"type": "text", "text": info}
        ]}))
    }

    fn click(&mut self, args: &Value) -> Result<Value, String> {
        let (x, y) = self.point(args)?;
        let button = match args.get("button").and_then(Value::as_str).unwrap_or("left") {
            "right" => Button::Right,
            "middle" => Button::Middle,
            _ => Button::Left,
        };
        let count = int(args, "count", 1).clamp(1, 3);
        let at = (num(args, "x")?, num(args, "y")?);
        let (kind, name) = match (button, count) {
            (Button::Right, _) => ("right", "right"),
            (Button::Middle, _) => ("click", "middle"),
            (_, 1) => ("click", "left"),
            _ => ("double", "left"),
        };
        self.emit(kind, Some(at), &format!("{name} at {}, {}", at.0, at.1));
        let input = self.input()?;
        glide(input, x, y)?;
        pause();
        for _ in 0..count {
            input
                .button(button, Direction::Click)
                .map_err(|e| e.to_string())?;
        }
        Ok(text(&format!(
            "Clicked {} time(s) at {}, {}",
            count,
            num(args, "x")?,
            num(args, "y")?
        )))
    }

    fn drag(&mut self, args: &Value) -> Result<Value, String> {
        let (x, y) = self.point(args)?;
        let (tx, ty) = self.map(num(args, "to_x")?, num(args, "to_y")?)?;
        let from = (num(args, "x")?, num(args, "y")?);
        let to = (num(args, "to_x")?, num(args, "to_y")?);
        self.emit(
            "drag",
            Some(from),
            &format!("from {}, {} to {}, {}", from.0, from.1, to.0, to.1),
        );
        glide(self.input()?, x, y)?;
        pause();
        self.emit("drop", Some(to), &format!("at {}, {}", to.0, to.1));
        let input = self.input()?;
        input
            .button(Button::Left, Direction::Press)
            .map_err(|e| e.to_string())?;
        for step in 1..=12 {
            let px = x + (tx - x) * step / 12;
            let py = y + (ty - y) * step / 12;
            input
                .move_mouse(px, py, Coordinate::Abs)
                .map_err(|e| e.to_string())?;
            std::thread::sleep(Duration::from_millis(25));
        }
        input
            .button(Button::Left, Direction::Release)
            .map_err(|e| e.to_string())?;
        Ok(text("Dragged"))
    }

    fn scroll(&mut self, args: &Value) -> Result<Value, String> {
        let (x, y) = self.point(args)?;
        let dx = i32::try_from(int(args, "dx", 0)).unwrap_or(0);
        let dy = i32::try_from(int(args, "dy", 0)).unwrap_or(0);
        let at = (num(args, "x")?, num(args, "y")?);
        let way = match (dy, dx) {
            (d, _) if d > 0 => format!("down {d}"),
            (d, _) if d < 0 => format!("up {}", -d),
            (_, d) if d > 0 => format!("right {d}"),
            (_, d) => format!("left {}", -d),
        };
        self.emit("scroll", Some(at), &format!("{way} at {}, {}", at.0, at.1));
        let input = self.input()?;
        glide(input, x, y)?;
        pause();
        if dy != 0 {
            input
                .scroll(dy, Axis::Vertical)
                .map_err(|e| e.to_string())?;
        }
        if dx != 0 {
            input
                .scroll(dx, Axis::Horizontal)
                .map_err(|e| e.to_string())?;
        }
        Ok(text("Scrolled"))
    }

    fn type_text(&mut self, args: &Value) -> Result<Value, String> {
        let value = args
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing text".to_owned())?
            .to_owned();
        self.emit("type", None, &format!("“{}”", typed_preview(&value)));
        self.input()?.text(&value).map_err(|e| e.to_string())?;
        Ok(text(&format!("Typed {} characters", value.chars().count())))
    }

    fn keys(&mut self, args: &Value) -> Result<Value, String> {
        let keys = args
            .get("keys")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing keys".to_owned())?
            .to_owned();
        let combos = keys
            .split_whitespace()
            .map(|combo| combo.split('+').map(key_of).collect::<Result<Vec<_>, _>>())
            .collect::<Result<Vec<_>, _>>()?;
        self.emit("key", None, &keys);
        let input = self.input()?;
        for combo in combos {
            let Some((last, mods)) = combo.split_last() else {
                continue;
            };
            for m in mods {
                input.key(*m, Direction::Press).map_err(|e| e.to_string())?;
            }
            let result = input
                .key(*last, Direction::Click)
                .map_err(|e| e.to_string());
            for m in mods.iter().rev() {
                let _ = input.key(*m, Direction::Release);
            }
            result?;
            pause();
        }
        Ok(text(&format!("Pressed {keys}")))
    }

    fn windows(&self) -> Result<Value, String> {
        let windows = xcap::Window::all().map_err(|e| e.to_string())?;
        let mut lines = Vec::new();
        for w in windows {
            if w.is_minimized().unwrap_or(false) {
                continue;
            }
            let title = w.title().unwrap_or_default();
            if title.trim().is_empty() {
                continue;
            }
            let (x, y) = (w.x().unwrap_or_default(), w.y().unwrap_or_default());
            let (width, height) = (
                w.width().unwrap_or_default(),
                w.height().unwrap_or_default(),
            );
            let shot = self.frame.map_or_else(String::new, |f| {
                format!(
                    " | screenshot {},{} {}x{}",
                    (f64::from(x - f.x) * f.scale).round(),
                    (f64::from(y - f.y) * f.scale).round(),
                    (f64::from(width) * f.scale).round(),
                    (f64::from(height) * f.scale).round()
                )
            });
            lines.push(format!(
                "{title} [{}]{} screen {x},{y} {width}x{height}{shot}",
                w.app_name().unwrap_or_default(),
                if w.is_focused().unwrap_or(false) {
                    " (focused)"
                } else {
                    ""
                }
            ));
        }
        Ok(text(&lines.join("\n")))
    }

    fn cursor(&mut self) -> Result<Value, String> {
        let frame = self.frame;
        let (x, y) = self.input()?.location().map_err(|e| e.to_string())?;
        Ok(match frame {
            Some(f) => text(&format!(
                "{}, {}",
                (f64::from(x - f.x) * f.scale).round(),
                (f64::from(y - f.y) * f.scale).round()
            )),
            None => text(&format!(
                "screen {x}, {y} (take a screenshot to map coordinates)"
            )),
        })
    }
}

fn key_of(name: &str) -> Result<Key, String> {
    let lower = name.trim().to_ascii_lowercase();
    Ok(match lower.as_str() {
        "ctrl" | "control" => Key::Control,
        "shift" => Key::Shift,
        "alt" | "option" => Key::Alt,
        "win" | "windows" | "super" | "cmd" | "command" | "meta" => Key::Meta,
        "enter" | "return" => Key::Return,
        "tab" => Key::Tab,
        "esc" | "escape" => Key::Escape,
        "space" => Key::Space,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "capslock" => Key::CapsLock,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        "plus" => Key::Unicode('+'),
        _ => {
            let mut chars = lower.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Key::Unicode(c),
                _ => return Err(format!("unknown key {name}")),
            }
        }
    })
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(char::from(TABLE[(n >> 18) as usize & 63]));
        out.push(char::from(TABLE[(n >> 12) as usize & 63]));
        out.push(if chunk.len() > 1 {
            char::from(TABLE[(n >> 6) as usize & 63])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(TABLE[n as usize & 63])
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn glide_is_quick_for_short_hops_and_bounded_for_long_ones() {
        let (steps, each) = glide_plan(10.0);
        assert!((6..=9).contains(&steps), "{steps}");
        assert!(each * u32::try_from(steps).unwrap() <= Duration::from_millis(130));
        let (steps, each) = glide_plan(4000.0);
        assert!(steps <= 24);
        assert!(each * u32::try_from(steps).unwrap() <= Duration::from_millis(361));
    }

    #[test]
    fn typed_preview_is_one_short_line() {
        assert_eq!(typed_preview("ls -la\n"), "ls -la ");
        let long = "x".repeat(200);
        let shown = typed_preview(&long);
        assert_eq!(shown.chars().count(), 61);
        assert!(shown.ends_with('…'));
    }

    #[test]
    fn live_dir_is_apart_from_the_gate_files() {
        let home = std::path::Path::new("/h");
        let dir = live_dir(home, "sess:1/2");
        assert_eq!(dir, std::path::Path::new("/h/machine-live/sess_1_2"));
        assert!(!dir.starts_with(harness_agent_hub::machine::gate_dir(home)));
    }
}
