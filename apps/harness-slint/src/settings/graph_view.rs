//! The picture of the project graph on Settings › Memory › Graph, and over the whole window when asked.
//!
//! `harness-graph-view` draws it, pixel by pixel, on a thread of its own: the window stays quick while a
//! frame is worked out. The page sends that thread what happens (the pointer, the wheel, the frame clock,
//! a new size) and gets finished frames back, each with the places where names go. One frame is on its way
//! at a time: the next is drawn when the window has shown the last, so a slow computer shows fewer frames
//! instead of falling behind. Nothing is drawn while the picture is off screen: the frame clock belongs to
//! the canvas and stops with it.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use harness_graph_view::{Engine, Graph, Overlay, Palette, Style};
use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

use super::graph::grouped;
use crate::ui::{AppWindow, GraphHit, GraphLabel, GraphUi, Theme};

/// Nodes listed at most: search results, and a chosen node's neighbours.
const LIST: usize = 60;
const FRAME_BUDGET: Duration = Duration::from_millis(16);

enum Cmd {
    Graph(Option<Arc<Graph>>),
    Style(Style),
    Palette(Palette),
    /// The canvas in device pixels, and how many of those make a logical pixel.
    Size(u32, u32, f32),
    /// The frame clock.
    Tick,
    /// The window has shown the last frame.
    Shown,
    /// 0 moved, 1 pressed, 2 dragged, 3 released, 4 left; in logical pixels.
    Pointer(i32, f32, f32),
    Zoom(f32),
    Reset,
    /// Hold still (paused, window in the background, reduced motion); with `calm`, also skip the arrival.
    Rest(bool, bool),
    Focus(Option<usize>),
    /// Choose a node from a list.
    Pick(Option<usize>),
    /// Room a panel takes on the right, in logical pixels.
    Inset(f32),
}

/// A finished frame and what goes over it.
struct Shot {
    buffer: SharedPixelBuffer<Rgba8Pixel>,
    overlay: Overlay,
    busy: bool,
    focus: Option<usize>,
    fps: f32,
}

struct View {
    commands: Sender<Cmd>,
    graph: Option<Arc<Graph>>,
    labels: Rc<VecModel<GraphLabel>>,
    /// What the detail panel and the tooltip show now, to fill them only when it changes.
    selected: Option<usize>,
    hover: Option<usize>,
    prefs: PathBuf,
    workspace: PathBuf,
}

thread_local! {
    static VIEW: RefCell<Option<View>> = const { RefCell::new(None) };
}

fn send(cmd: Cmd) {
    VIEW.with(|cell| {
        if let Some(view) = cell.borrow().as_ref() {
            let _ = view.commands.send(cmd);
        }
    });
}

/// The style and whether it is paused, kept between runs.
fn load_prefs(path: &Path) -> (Style, bool) {
    let prefs: serde_json::Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let style = prefs
        .get("style")
        .and_then(serde_json::Value::as_str)
        .map_or(Style::Space, Style::named);
    let paused = prefs
        .get("paused")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    (style, paused)
}

fn save_prefs(ui: &GraphUi<'_>) {
    let Some(path) = VIEW.with(|cell| cell.borrow().as_ref().map(|view| view.prefs.clone())) else {
        return;
    };
    let prefs = serde_json::json!({
        "style": ui.get_view_style().as_str(),
        "paused": ui.get_view_paused(),
    });
    let _ = std::thread::Builder::new()
        .name("harness-graph-prefs".into())
        .spawn(move || {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, prefs.to_string());
        });
}

fn rgb(color: slint::Color) -> [f32; 3] {
    [
        f32::from(color.red()) / 255.0,
        f32::from(color.green()) / 255.0,
        f32::from(color.blue()) / 255.0,
    ]
}

/// The theme's colours for the solar system.
fn palette(window: &AppWindow) -> Palette {
    let theme = window.global::<Theme>();
    Palette {
        ink: rgb(theme.get_foreground()),
        line: rgb(theme.get_foreground_subtlest()),
        sun: if theme.get_dark() {
            [0.97, 0.78, 0.36]
        } else {
            [0.88, 0.6, 0.12]
        },
        tones: [
            rgb(theme.get_accent_fg()),
            rgb(theme.get_info()),
            rgb(theme.get_agent_agy()),
            rgb(theme.get_agent_claude()),
            rgb(theme.get_warning()),
            rgb(theme.get_success()),
            if theme.get_dark() {
                [0.93, 0.5, 0.66]
            } else {
                [0.78, 0.25, 0.45]
            },
        ],
        quiet: rgb(theme.get_foreground_subtlest()),
    }
}

fn hit(graph: &Graph, node: usize) -> GraphHit {
    let tone = graph
        .nodes
        .get(node)
        .and_then(|n| graph.clusters.get(n.cluster))
        .map_or(-1, |cluster| cluster.tone);
    GraphHit {
        index: i32::try_from(node).unwrap_or(-1),
        title: graph
            .nodes
            .get(node)
            .map(|n| n.label.as_str())
            .unwrap_or_default()
            .into(),
        note: graph.note(node).into(),
        tone,
    }
}

fn hits(graph: &Graph, nodes: &[usize]) -> ModelRc<GraphHit> {
    let rows: Vec<GraphHit> = nodes.iter().take(LIST).map(|&i| hit(graph, i)).collect();
    ModelRc::new(VecModel::from(rows))
}

/// "300 of 4,210 nodes · 520 links".
fn summary(graph: &Graph) -> String {
    let nodes = if graph.total_nodes > graph.nodes.len() {
        format!(
            "{} of {} nodes",
            grouped(graph.nodes.len()),
            grouped(graph.total_nodes)
        )
    } else {
        format!("{} nodes", grouped(graph.total_nodes))
    };
    format!("{nodes} · {} links", grouped(graph.total_links))
}

pub fn install(window: &AppWindow, home: &Path, workspace: &Path) {
    let (commands, inbox) = mpsc::channel();
    let prefs = home.join("memory").join("graph-view.json");
    let (style, paused) = load_prefs(&prefs);
    let ui = window.global::<GraphUi>();
    ui.set_view_style(style.name().into());
    ui.set_view_paused(paused);
    ui.set_view_title(
        workspace
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into(),
    );
    let labels = Rc::new(VecModel::<GraphLabel>::default());
    ui.set_view_labels(ModelRc::from(labels.clone()));
    let _ = commands.send(Cmd::Style(style));
    let _ = commands.send(Cmd::Palette(palette(window)));
    VIEW.with(|cell| {
        *cell.borrow_mut() = Some(View {
            commands,
            graph: None,
            labels,
            selected: None,
            hover: None,
            prefs,
            workspace: workspace.to_path_buf(),
        });
    });
    let weak = window.as_weak();
    let _ = std::thread::Builder::new()
        .name("harness-graph-view".into())
        .spawn(move || work(&inbox, &weak));

    let weak = window.as_weak();
    ui.on_view_resized(move |width, height| {
        let Some(window) = weak.upgrade() else { return };
        let scale = window.window().scale_factor();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let size = |logical: f32| (logical * scale).round().max(0.0) as u32;
        send(Cmd::Palette(palette(&window)));
        send(Cmd::Size(size(width), size(height), scale));
    });
    let weak = window.as_weak();
    ui.on_view_themed(move || {
        if let Some(window) = weak.upgrade() {
            send(Cmd::Palette(palette(&window)));
        }
    });
    ui.on_view_pointer(|kind, x, y| send(Cmd::Pointer(kind, x, y)));
    ui.on_view_zoom(|factor| send(Cmd::Zoom(factor)));
    ui.on_view_tick(|| send(Cmd::Tick));
    ui.on_view_reset(|| send(Cmd::Reset));
    ui.on_view_rest(|resting, calm| send(Cmd::Rest(resting, calm)));
    ui.on_view_focus_cluster(|cluster| send(Cmd::Focus(usize::try_from(cluster).ok())));
    ui.on_view_pick(|node| send(Cmd::Pick(usize::try_from(node).ok())));
    ui.on_view_inset(|width| send(Cmd::Inset(width)));

    let weak = window.as_weak();
    ui.on_view_set_style(move |name| {
        let Some(window) = weak.upgrade() else { return };
        let ui = window.global::<GraphUi>();
        let style = Style::named(name.as_str());
        ui.set_view_style(style.name().into());
        save_prefs(&ui);
        send(Cmd::Palette(palette(&window)));
        send(Cmd::Style(style));
    });
    let weak = window.as_weak();
    ui.on_view_set_paused(move |paused| {
        let Some(window) = weak.upgrade() else { return };
        let ui = window.global::<GraphUi>();
        ui.set_view_paused(paused);
        save_prefs(&ui);
    });
    let weak = window.as_weak();
    ui.on_view_search(move |query| {
        let Some(window) = weak.upgrade() else { return };
        VIEW.with(|cell| {
            if let Some(graph) = cell.borrow().as_ref().and_then(|view| view.graph.clone()) {
                let found = graph.search(query.as_str(), LIST);
                window
                    .global::<GraphUi>()
                    .set_list_hits(hits(&graph, &found));
            }
        });
    });
    ui.on_view_open_source(|| {
        let path = VIEW.with(|cell| {
            let cell = cell.borrow();
            let view = cell.as_ref()?;
            let node = view.graph.as_ref()?.nodes.get(view.selected?)?;
            (!node.source.is_empty()).then(|| view.workspace.join(&node.source))
        });
        if let Some(path) = path {
            super::graph::open_path(&path);
        }
    });
}

pub fn set_workspace(window: &AppWindow, workspace: &Path) {
    window.global::<GraphUi>().set_view_title(
        workspace
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into(),
    );
    VIEW.with(|cell| {
        if let Some(view) = cell.borrow_mut().as_mut() {
            view.workspace = workspace.to_path_buf();
        }
    });
    show(window, None);
}

/// Shows `graph` (or clears the picture); on the UI thread.
pub fn show(window: &AppWindow, graph: Option<Arc<Graph>>) {
    let ui = window.global::<GraphUi>();
    VIEW.with(|cell| {
        let mut cell = cell.borrow_mut();
        let Some(view) = cell.as_mut() else { return };
        let rows: Vec<GraphLabel> = graph.as_ref().map_or_else(Vec::new, |graph| {
            graph
                .clusters
                .iter()
                .enumerate()
                .map(|(i, cluster)| GraphLabel {
                    cluster: i32::try_from(i).unwrap_or(-1),
                    x: 0.0,
                    y: 0.0,
                    alpha: 0.0,
                    title: cluster.name.as_str().into(),
                    note: format!("{} nodes", grouped(cluster.size)).into(),
                    tone: cluster.tone,
                })
                .collect()
        });
        view.labels.set_vec(rows);
        view.selected = None;
        view.hover = None;
        ui.set_view_note(graph.as_deref().map(summary).unwrap_or_default().into());
        ui.set_view_selected(-1);
        ui.set_tip_shown(false);
        ui.set_view_focus(-1);
        ui.set_list_query("".into());
        if let Some(graph) = &graph {
            ui.set_list_hits(hits(graph, &graph.search("", LIST)));
        } else {
            ui.set_list_hits(ModelRc::default());
            ui.set_view_ready(false);
        }
        view.graph.clone_from(&graph);
        let _ = view.commands.send(Cmd::Graph(graph));
    });
}

/// Puts a finished frame on the page; on the UI thread.
fn present(window: &AppWindow, shot: Shot) {
    let ui = window.global::<GraphUi>();
    VIEW.with(|cell| {
        let mut cell = cell.borrow_mut();
        let Some(view) = cell.as_mut() else { return };
        let Some(graph) = view.graph.clone() else {
            return;
        };
        ui.set_view_frame(Image::from_rgba8_premultiplied(shot.buffer));
        ui.set_view_ready(true);
        ui.set_view_busy(shot.busy);
        ui.set_view_fps(if shot.busy && shot.fps > 1.0 {
            format!("{:.0} fps", shot.fps).into()
        } else {
            "".into()
        });
        for row in 0..view.labels.row_count() {
            let Some(mut label) = view.labels.row_data(row) else {
                continue;
            };
            match shot
                .overlay
                .labels
                .iter()
                .find(|(cluster, _)| *cluster == row)
            {
                Some((_, marker)) => {
                    label.x = marker.x;
                    label.y = marker.y;
                    label.alpha = marker.alpha;
                }
                None => label.alpha = 0.0,
            }
            view.labels.set_row_data(row, label);
        }
        if let Some(core) = shot.overlay.core {
            ui.set_core_x(core.x);
            ui.set_core_y(core.y);
            ui.set_core_alpha(core.alpha);
        } else {
            ui.set_core_alpha(0.0);
        }
        ui.set_view_focus(shot.focus.and_then(|c| i32::try_from(c).ok()).unwrap_or(-1));
        ui.set_view_focus_name(
            shot.focus
                .and_then(|c| graph.clusters.get(c))
                .map(|cluster| cluster.name.as_str())
                .unwrap_or_default()
                .into(),
        );
        let hover = shot.overlay.hover.map(|(node, _)| node);
        if let Some((node, marker)) = shot.overlay.hover {
            ui.set_tip_x(marker.x);
            ui.set_tip_y(marker.y);
            if view.hover != hover {
                let row = hit(&graph, node);
                ui.set_tip_title(row.title);
                ui.set_tip_note(row.note);
            }
        }
        ui.set_tip_shown(hover.is_some());
        view.hover = hover;
        let selected = shot.overlay.selected.map(|(node, _)| node);
        if let Some((node, marker)) = shot.overlay.selected {
            ui.set_sel_x(marker.x);
            ui.set_sel_y(marker.y);
            if view.selected != selected {
                let row = hit(&graph, node);
                ui.set_sel_title(row.title);
                ui.set_sel_note(row.note);
                let chosen = graph.nodes.get(node);
                ui.set_sel_group(
                    chosen
                        .and_then(|n| graph.clusters.get(n.cluster))
                        .map(|cluster| cluster.name.as_str())
                        .unwrap_or_default()
                        .into(),
                );
                ui.set_sel_has_source(chosen.is_some_and(|n| !n.source.is_empty()));
                ui.set_sel_links(hits(&graph, graph.neighbours(node)));
            }
        }
        ui.set_view_selected(selected.and_then(|n| i32::try_from(n).ok()).unwrap_or(-1));
        view.selected = selected;
        let _ = view.commands.send(Cmd::Shown);
    });
}

/// The drawing thread: takes what happened, draws a frame when one is due, hands it to the window.
#[allow(clippy::too_many_lines)]
fn work(inbox: &Receiver<Cmd>, window: &slint::Weak<AppWindow>) {
    let mut engine: Option<Engine> = None;
    let mut style = Style::Space;
    let mut palette = Palette::default();
    let mut size = (0_u32, 0_u32, 1.0_f32);
    let mut resting = false;
    let mut inset = 0.0_f32;
    // A frame is on its way to the window; a change came in that no frame shows yet.
    let mut waiting = false;
    let mut dirty = false;
    let mut last = Instant::now();
    let mut fps = 0.0_f32;
    while let Ok(first) = inbox.recv() {
        let mut tick = false;
        for cmd in std::iter::once(first).chain(inbox.try_iter()) {
            match cmd {
                Cmd::Tick => tick = true,
                Cmd::Shown => {
                    waiting = false;
                    tick = true;
                }
                Cmd::Graph(graph) => {
                    engine = graph.map(|graph| {
                        let mut engine = Engine::new(graph, style);
                        engine.set_palette(palette);
                        engine.resize(size.0 as usize, size.1 as usize, size.2);
                        engine.set_paused(resting);
                        engine.set_inset(inset);
                        engine
                    });
                    dirty = true;
                }
                Cmd::Style(next) => {
                    style = next;
                    if let Some(engine) = &mut engine {
                        engine.set_style(next);
                    }
                    dirty = true;
                }
                Cmd::Palette(next) => {
                    if palette != next {
                        palette = next;
                        if let Some(engine) = &mut engine {
                            engine.set_palette(next);
                        }
                        dirty = true;
                    }
                }
                Cmd::Size(width, height, scale) => {
                    if size != (width, height, scale) {
                        size = (width, height, scale);
                        if let Some(engine) = &mut engine {
                            engine.resize(width as usize, height as usize, scale);
                        }
                        dirty = true;
                    }
                }
                Cmd::Rest(rest, calm) => {
                    resting = rest;
                    if let Some(engine) = &mut engine {
                        engine.set_paused(rest);
                        if calm {
                            engine.arrive();
                        }
                    }
                    dirty = true;
                }
                Cmd::Pointer(kind, x, y) => {
                    if let Some(engine) = &mut engine {
                        dirty |= match kind {
                            1 => {
                                engine.pointer_down(x, y);
                                false
                            }
                            2 => {
                                engine.pointer_drag(x, y);
                                true
                            }
                            3 => engine.pointer_up(x, y),
                            4 => engine.pointer_leave(),
                            _ => engine.pointer_move(x, y),
                        };
                    }
                }
                Cmd::Zoom(factor) => {
                    if let Some(engine) = &mut engine {
                        engine.zoom_by(factor);
                    }
                    dirty = true;
                }
                Cmd::Reset => {
                    if let Some(engine) = &mut engine {
                        engine.reset();
                    }
                    dirty = true;
                }
                Cmd::Focus(cluster) => {
                    if let Some(engine) = &mut engine {
                        engine.set_focus(cluster);
                    }
                    dirty = true;
                }
                Cmd::Pick(node) => {
                    if let Some(engine) = &mut engine {
                        engine.select(node, true);
                    }
                    dirty = true;
                }
                Cmd::Inset(width) => {
                    inset = width;
                    if let Some(engine) = &mut engine {
                        engine.set_inset(width);
                    }
                    dirty = true;
                }
            }
        }
        let Some(engine) = engine.as_mut() else {
            continue;
        };
        if waiting || !(dirty || (tick && engine.busy())) {
            continue;
        }
        let (width, height) = engine.size();
        let (Ok(buffer_w), Ok(buffer_h)) = (u32::try_from(width), u32::try_from(height)) else {
            continue;
        };
        if width == 0 || height == 0 {
            continue;
        }
        if let Some(rest) = FRAME_BUDGET.checked_sub(last.elapsed()) {
            std::thread::sleep(rest);
        }
        let now = Instant::now();
        let passed = now.duration_since(last).as_secs_f32();
        last = now;
        engine.step(passed);
        if passed > 0.001 && passed < 0.5 {
            fps = if fps > 1.0 {
                fps * 0.9 + 0.1 / passed
            } else {
                1.0 / passed
            };
        }
        let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(buffer_w, buffer_h);
        engine.render(buffer.make_mut_bytes());
        let shot = Shot {
            buffer,
            overlay: engine.overlay(),
            busy: engine.busy(),
            focus: engine.focus(),
            fps,
        };
        dirty = false;
        waiting = true;
        let weak = window.clone();
        let sent = slint::invoke_from_event_loop(move || {
            if let Some(window) = weak.upgrade() {
                present(&window, shot);
            }
        });
        if sent.is_err() {
            return;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_graph_is_summed_up_and_its_nodes_listed() {
        let graph = Graph::parse(
            r#"{"nodes":[{"id":"a","label":"a.rs","source_file":"src/a.rs","community":0},{"id":"b","community":0}],"links":[{"source":"a","target":"b"}]}"#,
        )
        .expect("graph");
        assert_eq!(summary(&graph), "2 nodes · 1 links");
        let row = hit(&graph, 0);
        assert_eq!((row.index, row.tone), (0, 0));
        assert_eq!(row.note.as_str(), "1 link · src/a.rs");
        assert_eq!(hit(&graph, 99).index, 99);
        assert_eq!(hit(&graph, 99).title.as_str(), "");
    }

    #[test]
    fn preferences_default_to_the_galaxy_and_survive_a_bad_file() {
        let dir = std::env::temp_dir().join(format!("harness-graph-view-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("graph-view.json");
        assert_eq!(load_prefs(&path), (Style::Space, false));
        std::fs::write(&path, "not json").expect("write");
        assert_eq!(load_prefs(&path), (Style::Space, false));
        std::fs::write(&path, r#"{"style":"orbit","paused":true}"#).expect("write");
        assert_eq!(load_prefs(&path), (Style::Orbit, true));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
