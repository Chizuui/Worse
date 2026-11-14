//! Whiteboard and image annotation (13A): freehand strokes over a white board or an attached PNG, rasterised to a
//! PNG attachment chip. Strokes live here (board coordinates 640 x 400); Slint draws them as `Path` commands.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::rc::Rc;

use image::{imageops, Rgba, RgbaImage};
use slint::{ComponentHandle, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};

use crate::ui::{AppWindow, ChatUi, WbStroke, WhiteboardUi};

const BOARD_W: f32 = 640.0;
const BOARD_H: f32 = 400.0;
/// Export resolution = board x 2 (sharp on high-DPI screens).
const SCALE: f32 = 2.0;
/// Stroke radius in board units (Slint draws a 3px line).
const RADIUS: f32 = 1.5;

struct Stroke {
    color: String,
    points: Vec<(f32, f32)>,
}

struct Board {
    window: AppWindow,
    strokes: Vec<Stroke>,
    model: Rc<VecModel<WbStroke>>,
    live: Option<Stroke>,
    background: Option<RgbaImage>,
    /// File of the image chip being annotated (that chip is replaced by the result on attach).
    annotating: Option<String>,
}

thread_local! {
    static BOARD: RefCell<Option<Board>> = const { RefCell::new(None) };
}

fn with_board<T>(f: impl FnOnce(&mut Board) -> T) -> Option<T> {
    BOARD.with(|cell| {
        cell.try_borrow_mut()
            .ok()
            .and_then(|mut guard| guard.as_mut().map(f))
    })
}

fn commands(points: &[(f32, f32)]) -> String {
    let mut out = String::new();
    for (i, (x, y)) in points.iter().enumerate() {
        let _ = write!(out, "{}{x:.1} {y:.1} ", if i == 0 { "M" } else { "L" });
    }
    // 单击（只有一个点）也要画出一个点：补一段极短的线。
    if let [(x, y)] = points {
        let _ = write!(out, "L{:.1} {y:.1}", x + 0.1);
    }
    out
}

fn color_rgba(color: &str) -> Rgba<u8> {
    match color {
        "black" => Rgba([0x1c, 0x20, 0x24, 0xff]),
        "blue" => Rgba([0x00, 0x90, 0xff, 0xff]),
        "green" => Rgba([0x30, 0xa4, 0x6c, 0xff]),
        "yellow" => Rgba([0xe2, 0xa3, 0x36, 0xff]),
        _ => Rgba([0xe5, 0x48, 0x4d, 0xff]),
    }
}

fn clamp_point(x: f32, y: f32) -> (f32, f32) {
    (x.clamp(0.0, BOARD_W), y.clamp(0.0, BOARD_H))
}

impl Board {
    fn ui(&self) -> WhiteboardUi<'_> {
        self.window.global::<WhiteboardUi>()
    }

    fn sync(&self) {
        self.model.set_vec(
            self.strokes
                .iter()
                .map(|s| WbStroke {
                    commands: SharedString::from(commands(&s.points)),
                    color: SharedString::from(s.color.as_str()),
                })
                .collect::<Vec<_>>(),
        );
        let live = self
            .live
            .as_ref()
            .map(|s| commands(&s.points))
            .unwrap_or_default();
        self.ui().set_live(SharedString::from(live));
    }

    fn reset(&mut self) {
        self.strokes.clear();
        self.live = None;
        self.background = None;
        self.annotating = None;
        let ui = self.ui();
        ui.set_has_background(false);
        ui.set_background(slint::Image::default());
        ui.set_annotating(SharedString::new());
        self.sync();
    }

    /// Board pixels at export scale: white, then the background (contain), then the strokes.
    fn render(&self) -> RgbaImage {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (w, h) = ((BOARD_W * SCALE) as u32, (BOARD_H * SCALE) as u32);
        let mut canvas = RgbaImage::from_pixel(w, h, Rgba([0xff, 0xff, 0xff, 0xff]));
        if let Some(bg) = &self.background {
            let fit =
                (f64::from(w) / f64::from(bg.width())).min(f64::from(h) / f64::from(bg.height()));
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let (bw, bh) = (
                ((f64::from(bg.width()) * fit) as u32).max(1),
                ((f64::from(bg.height()) * fit) as u32).max(1),
            );
            let scaled = imageops::resize(bg, bw, bh, imageops::FilterType::Triangle);
            imageops::overlay(
                &mut canvas,
                &scaled,
                i64::from((w - bw) / 2),
                i64::from((h - bh) / 2),
            );
        }
        for stroke in &self.strokes {
            let ink = color_rgba(&stroke.color);
            let mut last: Option<(f32, f32)> = None;
            for &(x, y) in &stroke.points {
                let (x0, y0) = last.unwrap_or((x, y));
                let steps = ((x - x0).hypot(y - y0) * SCALE).ceil().max(1.0);
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                for i in 0..=(steps as u32) {
                    #[allow(clippy::cast_precision_loss)]
                    let t = i as f32 / steps;
                    stamp(
                        &mut canvas,
                        (x0 + (x - x0) * t) * SCALE,
                        (y0 + (y - y0) * t) * SCALE,
                        ink,
                    );
                }
                last = Some((x, y));
            }
        }
        canvas
    }
}

/// Filled disk of the stroke radius at (cx, cy) in export pixels.
fn stamp(canvas: &mut RgbaImage, cx: f32, cy: f32, ink: Rgba<u8>) {
    let r = RADIUS * SCALE;
    #[allow(clippy::cast_possible_truncation)]
    let (x0, x1, y0, y1) = (
        (cx - r).floor() as i64,
        (cx + r).ceil() as i64,
        (cy - r).floor() as i64,
        (cy + r).ceil() as i64,
    );
    for py in y0..=y1 {
        for px in x0..=x1 {
            #[allow(clippy::cast_precision_loss)]
            let inside = (px as f32 + 0.5 - cx).hypot(py as f32 + 0.5 - cy) <= r;
            if let (true, Ok(ux), Ok(uy)) = (inside, u32::try_from(px), u32::try_from(py)) {
                if ux < canvas.width() && uy < canvas.height() {
                    canvas.put_pixel(ux, uy, ink);
                }
            }
        }
    }
}

fn to_slint(image: &RgbaImage) -> slint::Image {
    let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
        image.as_raw(),
        image.width(),
        image.height(),
    );
    slint::Image::from_rgba8(buffer)
}

/// Opens an empty whiteboard (`index` < 0) or annotates the image chip at `index` (PNG / BMP files).
pub fn open(index: i32) {
    let chip = usize::try_from(index).ok().and_then(super::chip_image);
    with_board(|b| {
        b.reset();
        b.ui().set_open(true);
    });
    let Some((path, label)) = chip else {
        return;
    };
    with_board(|b| {
        b.annotating = Some(path.clone());
        b.ui().set_annotating(SharedString::from(label.as_str()));
    });
    // 读取与解码放到独立线程；只支持 PNG / BMP（image 仅启用了这两种解码器）。
    std::thread::spawn(move || {
        let decoded = std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|bytes| image::load_from_memory(&bytes).map_err(|e| e.to_string()))
            .map(|img| img.to_rgba8());
        let _ = slint::invoke_from_event_loop(move || match decoded {
            Ok(img) => {
                with_board(|b| {
                    let ui = b.ui();
                    ui.set_background(to_slint(&img));
                    ui.set_has_background(true);
                    b.background = Some(img);
                });
            }
            Err(error) => {
                with_board(|b| b.window.set_error_text(SharedString::from(error.as_str())));
            }
        });
    });
}

fn attach() {
    let Some((png, annotating)) = with_board(|b| {
        let canvas = b.render();
        let mut out = std::io::Cursor::new(Vec::new());
        let png = canvas
            .write_to(&mut out, image::ImageFormat::Png)
            .ok()
            .map(|()| out.into_inner());
        let annotating = b.annotating.take();
        b.ui().set_open(false);
        b.reset();
        png.map(|p| (p, annotating))
    })
    .flatten() else {
        return;
    };
    // 修复：按文件路径而不是下标移除原图片附件；白板打开期间用户可能已删除其他附件，下标会错位。
    if let Some(original) = &annotating {
        super::remove_image_chip(original);
    }
    let Some(dir) = super::attachments_dir() else {
        return;
    };
    super::save_attachment(
        dir,
        png,
        if annotating.is_some() {
            "annotated"
        } else {
            "whiteboard"
        },
    );
}

pub fn install(window: &AppWindow) {
    let model = Rc::new(VecModel::default());
    let ui = window.global::<WhiteboardUi>();
    ui.set_strokes(ModelRc::from(model.clone()));
    BOARD.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            *guard = Some(Board {
                window: window.clone_strong(),
                strokes: Vec::new(),
                model,
                live: None,
                background: None,
                annotating: None,
            });
        }
    });
    ui.on_begin(|x, y| {
        with_board(|b| {
            let color = b.ui().get_color().to_string();
            b.live = Some(Stroke {
                color,
                points: vec![clamp_point(x, y)],
            });
            b.sync();
        });
    });
    ui.on_move(|x, y| {
        with_board(|b| {
            if let Some(stroke) = b.live.as_mut() {
                let p = clamp_point(x, y);
                // 相邻点距离过近时不记录，控制路径长度。
                if stroke
                    .points
                    .last()
                    .map_or(true, |l| (l.0 - p.0).hypot(l.1 - p.1) >= 1.5)
                {
                    stroke.points.push(p);
                    let live = commands(&stroke.points);
                    b.ui().set_live(SharedString::from(live));
                }
            }
        });
    });
    ui.on_end(|| {
        with_board(|b| {
            if let Some(stroke) = b.live.take() {
                b.strokes.push(stroke);
            }
            b.sync();
        });
    });
    ui.on_undo(|| {
        with_board(|b| {
            b.strokes.pop();
            b.sync();
        });
    });
    ui.on_clear(|| {
        with_board(|b| {
            b.strokes.clear();
            b.sync();
        });
    });
    ui.on_attach(attach);
    window.global::<ChatUi>().on_annotate_chip(open);
}
