use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use serde_json::{json, Value};

const MAX_SIDE: u32 = 1568;
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

fn point_schema(extra: Value) -> Value {
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
         "inputSchema": point_schema(json!({"button": {"type": "string", "enum": ["left", "right", "middle"]}, "count": {"type": "integer", "description": "1 = click, 2 = double click"}}))},
        {"name": "move", "description": "Move the mouse to a point of the last screenshot.", "inputSchema": point_schema(json!({}))},
        {"name": "drag", "description": "Drag with the left button from (x, y) to (to_x, to_y).",
         "inputSchema": {"type": "object", "properties": {"x": {"type": "number"}, "y": {"type": "number"}, "to_x": {"type": "number"}, "to_y": {"type": "number"}}, "required": ["x", "y", "to_x", "to_y"]}},
        {"name": "scroll", "description": "Scroll at a point; dy > 0 scrolls down, dx > 0 scrolls right (wheel notches).",
         "inputSchema": point_schema(json!({"dx": {"type": "integer"}, "dy": {"type": "integer"}}))},
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
                self.input()?
                    .move_mouse(x, y, Coordinate::Abs)
                    .map_err(|e| e.to_string())?;
                Ok(text(&format!(
                    "Moved to {}, {}",
                    num(&args, "x")?,
                    num(&args, "y")?
                )))
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
            info.push_str(&format!(
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
            ));
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
        let input = self.input()?;
        input
            .move_mouse(x, y, Coordinate::Abs)
            .map_err(|e| e.to_string())?;
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
        let input = self.input()?;
        input
            .move_mouse(x, y, Coordinate::Abs)
            .map_err(|e| e.to_string())?;
        pause();
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
        let input = self.input()?;
        input
            .move_mouse(x, y, Coordinate::Abs)
            .map_err(|e| e.to_string())?;
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
