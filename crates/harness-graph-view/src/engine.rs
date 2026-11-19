//! One picture of a graph that can be looked around in: it owns the camera, moves it smoothly towards where
//! it was asked to go, answers the pointer and draws frames in either style.
//!
//! Positions that come in from the pointer and go out for labels are in logical pixels (the unit a UI lays
//! out in); the frame itself may have more pixels than that, or fewer when it would be too many to draw.

use std::sync::Arc;

use crate::camera::{Camera, Seen};
use crate::math::{lerp, Vec3, TAU};
use crate::model::Graph;
use crate::orbit::{Orbit, OrbitView, Palette, SLANT};
use crate::raster::{Blend, Frame};
use crate::space::{Space, SpaceView};

/// A frame is never larger than this many pixels; a bigger canvas is drawn smaller and stretched.
pub const MAX_PIXELS: usize = 1_500_000;
/// Seconds the picture takes to arrive.
const ARRIVE: f32 = 1.3;
/// The galaxy turns this fast on its own, in radians a second.
const SPIN: f32 = 0.055;
/// How far the pointer may move between press and release and still count as a click.
const CLICK_SLOP: f32 = 4.0;
/// How near a star or a body the pointer must be to pick it.
const PICK_REACH: f32 = 11.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Style {
    /// The flat solar system.
    #[default]
    Orbit,
    /// The galaxy around a black hole, in 3D.
    Space,
}

impl Style {
    /// "space" is the galaxy; anything else is the solar system.
    #[must_use]
    pub fn named(name: &str) -> Self {
        if name == "space" {
            Self::Space
        } else {
            Self::Orbit
        }
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Orbit => "orbit",
            Self::Space => "space",
        }
    }
}

/// A place on the canvas, in logical pixels, and how strongly to show what stands there.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Marker {
    pub x: f32,
    pub y: f32,
    pub alpha: f32,
}

/// What a UI lays over a frame.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Overlay {
    /// Where each community's name goes: (index into the graph's clusters, place).
    pub labels: Vec<(usize, Marker)>,
    /// Where the name of the middle (the hole, the sun) goes.
    pub core: Option<Marker>,
    pub hover: Option<(usize, Marker)>,
    pub selected: Option<(usize, Marker)>,
}

struct Press {
    from: (f32, f32),
    last: (f32, f32),
    moved: bool,
}

pub struct Engine {
    graph: Arc<Graph>,
    style: Style,
    space: Option<Space>,
    orbit: Option<Orbit>,
    palette: Palette,
    width: usize,
    height: usize,
    unit: f32,
    time: f32,
    birth: f32,
    camera: Camera,
    goal: Camera,
    focus: Option<usize>,
    focus_mix: f32,
    hover: Option<usize>,
    selected: Option<usize>,
    paused: bool,
    press: Option<Press>,
    /// Room kept free on the right for a panel, in logical pixels, and how much of it the picture has
    /// made so far.
    inset: f32,
    made: f32,
}

fn home(style: Style) -> Camera {
    match style {
        Style::Space => Camera {
            yaw: 0.6,
            pitch: 0.11,
            zoom: 1.0,
            target: Vec3::ZERO,
        },
        Style::Orbit => Camera {
            yaw: 0.0,
            pitch: SLANT,
            zoom: 1.0,
            target: Vec3::ZERO,
        },
    }
}

fn zoom_range(style: Style) -> (f32, f32) {
    match style {
        Style::Space => (0.55, 3.0),
        Style::Orbit => (0.6, 8.0),
    }
}

impl Engine {
    #[must_use]
    pub fn new(graph: Arc<Graph>, style: Style) -> Self {
        Self {
            graph,
            style,
            space: None,
            orbit: None,
            palette: Palette::default(),
            width: 0,
            height: 0,
            unit: 1.0,
            time: 0.0,
            birth: 0.0,
            camera: home(style),
            goal: home(style),
            focus: None,
            focus_mix: 0.0,
            hover: None,
            selected: None,
            paused: false,
            press: None,
            inset: 0.0,
            made: 0.0,
        }
    }

    #[must_use]
    pub fn graph(&self) -> &Arc<Graph> {
        &self.graph
    }

    #[must_use]
    pub fn style(&self) -> Style {
        self.style
    }

    /// Switches the picture; it arrives anew, seen from its home view.
    pub fn set_style(&mut self, style: Style) {
        if style == self.style {
            return;
        }
        self.style = style;
        self.camera = home(style);
        self.goal = home(style);
        self.birth = 0.0;
        self.focus = None;
        self.focus_mix = 0.0;
        self.hover = None;
        self.selected = None;
        self.press = None;
    }

    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
    }

    /// The canvas: its size in device pixels and how many of those make a logical pixel.
    pub fn resize(&mut self, width: usize, height: usize, scale: f32) {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        let pixels = width * height;
        let shrink = if pixels > MAX_PIXELS {
            (MAX_PIXELS as f32 / pixels as f32).sqrt()
        } else {
            1.0
        };
        self.width = (width as f32 * shrink).round() as usize;
        self.height = (height as f32 * shrink).round() as usize;
        self.unit = scale * shrink;
    }

    /// The size of a frame in pixels: [`Engine::render`] wants four bytes for each.
    #[must_use]
    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    #[must_use]
    pub fn focus(&self) -> Option<usize> {
        self.focus
    }

    #[must_use]
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    #[must_use]
    pub fn hover(&self) -> Option<usize> {
        self.hover
    }

    #[must_use]
    pub fn paused(&self) -> bool {
        self.paused
    }

    /// Stops the clock: nothing turns or flies until it is started again. Looking around still works.
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    fn space(&mut self) -> &mut Space {
        let graph = &self.graph;
        self.space.get_or_insert_with(|| Space::new(graph))
    }

    fn orbit(&mut self) -> &mut Orbit {
        let graph = &self.graph;
        self.orbit.get_or_insert_with(|| Orbit::new(graph))
    }

    /// Keeps `width` logical pixels on the right free (a panel lies there): the picture moves over so its
    /// middle is the middle of what is left.
    pub fn set_inset(&mut self, width: f32) {
        self.inset = if width.is_finite() {
            width.max(0.0)
        } else {
            0.0
        };
    }

    fn slide(&self) -> f32 {
        -self.made * self.unit * 0.5
    }

    /// Skips the arrival: the picture is all there at once, for those who asked for less motion.
    pub fn arrive(&mut self) {
        self.birth = 1.0;
        self.camera = self.goal;
        self.made = self.inset;
    }

    /// Looks at one community (the others dim), or at everything again.
    pub fn set_focus(&mut self, cluster: Option<usize>) {
        let cluster = cluster.filter(|&c| c < self.graph.clusters.len());
        self.focus = cluster;
        match (self.style, cluster) {
            (Style::Space, Some(c)) => {
                // Bring it to the near side, a little to one side of the hole, and step closer.
                let anchor = self.space().anchor(c);
                let facing = anchor.x.atan2(anchor.z) - 0.5;
                let turns = ((self.goal.yaw - facing) / TAU).round();
                self.goal.yaw = facing + turns * TAU;
                self.goal.target = anchor * 0.3;
                self.goal.zoom = 1.25;
            }
            (Style::Orbit, Some(c)) => {
                self.goal.zoom = self.orbit().close_zoom(c);
            }
            (_, None) => {
                self.goal.target = Vec3::ZERO;
                self.goal.zoom = 1.0;
            }
        }
    }

    /// Chooses a node (its links light up), or none. With `show`, the view also turns to it: for a node
    /// picked from a list rather than on the picture.
    pub fn select(&mut self, node: Option<usize>, show: bool) {
        let node = node.filter(|&n| n < self.graph.nodes.len());
        self.selected = node;
        let Some(node) = node.filter(|_| show) else {
            return;
        };
        let cluster = self.graph.nodes[node].cluster;
        if self.focus.is_some_and(|c| c != cluster) {
            self.set_focus(Some(cluster));
        }
        if self.style == Style::Space {
            if let Some(at) = self.space().place(node) {
                // Bring the star to the near side, a little off the hole.
                let facing = at.x.atan2(at.z) + 0.35;
                let turns = ((self.goal.yaw - facing) / TAU).round();
                self.goal.yaw = facing + turns * TAU;
            }
        }
    }

    /// Back to the home view, nothing chosen.
    pub fn reset(&mut self) {
        let yaw = self.goal.yaw;
        self.goal = home(self.style);
        if self.style == Style::Space {
            // Keep turning from where it is instead of spinning all the way back.
            self.goal.yaw = yaw;
        }
        self.focus = None;
        self.selected = None;
    }

    /// Zooms by a factor (above 1 comes closer).
    pub fn zoom_by(&mut self, factor: f32) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let (low, high) = zoom_range(self.style);
        self.goal.zoom = (self.goal.zoom * factor).clamp(low, high);
    }

    pub fn pointer_down(&mut self, x: f32, y: f32) {
        self.press = Some(Press {
            from: (x, y),
            last: (x, y),
            moved: false,
        });
    }

    /// The pointer moved with the button held: the galaxy turns, the solar system slides.
    pub fn pointer_drag(&mut self, x: f32, y: f32) {
        let Some(press) = &mut self.press else {
            return;
        };
        let (dx, dy) = (x - press.last.0, y - press.last.1);
        press.last = (x, y);
        if !press.moved && (x - press.from.0).hypot(y - press.from.1) < CLICK_SLOP {
            return;
        }
        press.moved = true;
        match self.style {
            Style::Space => {
                self.goal.yaw -= dx * 0.008;
                self.goal.pitch = (self.goal.pitch + dy * 0.008).clamp(-1.4, 1.4);
                // Under the hand the view follows at once.
                self.camera.yaw = self.goal.yaw;
                self.camera.pitch = self.goal.pitch;
            }
            Style::Orbit => {
                let (width, height, unit) = (self.width as f32, self.height as f32, self.unit);
                let camera = self.camera;
                let lens = self.orbit().lens(&camera, width, height, unit);
                let Some(scale) = lens.see(camera.target).map(|seen| seen.scale / unit) else {
                    return;
                };
                let (right, up) = lens.axes();
                let slide = (right * -dx + up * dy) * (1.0 / scale.max(1e-3));
                // Sliding by hand lets go of a planet that was being followed.
                self.focus = None;
                self.goal.target = self.goal.target + slide;
                self.camera.target = self.camera.target + slide;
            }
        }
    }

    /// The button came up. A press that did not move is a click: on a node it chooses it (again: lets go of
    /// it), elsewhere it lets go of the chosen one. Returns whether it was a click.
    pub fn pointer_up(&mut self, x: f32, y: f32) -> bool {
        let Some(press) = self.press.take() else {
            return false;
        };
        if press.moved {
            return false;
        }
        let hit = self.pick(x, y);
        self.selected = match hit {
            Some(node) if self.selected == Some(node) => None,
            other => other,
        };
        true
    }

    /// The pointer moved with no button held; returns whether what it points at changed.
    pub fn pointer_move(&mut self, x: f32, y: f32) -> bool {
        let hit = self.pick(x, y);
        let changed = hit != self.hover;
        self.hover = hit;
        changed
    }

    pub fn pointer_leave(&mut self) -> bool {
        let changed = self.hover.is_some();
        self.hover = None;
        changed
    }

    fn pick(&self, x: f32, y: f32) -> Option<usize> {
        let (x, y, reach) = (x * self.unit, y * self.unit, PICK_REACH * self.unit);
        match self.style {
            Style::Space => {
                self.space
                    .as_ref()?
                    .pick(x, y, reach, self.focus.filter(|_| self.focus_mix > 0.5))
            }
            Style::Orbit => self.orbit.as_ref()?.pick(x, y, reach * 0.6),
        }
    }

    /// Moves time on by `seconds` and the camera towards where it was asked to go.
    pub fn step(&mut self, seconds: f32) {
        let dt = if seconds.is_finite() {
            seconds.clamp(0.0, 0.1)
        } else {
            0.0
        };
        self.birth = (self.birth + dt / ARRIVE).min(1.0);
        if !self.paused {
            self.time += dt;
            if self.style == Style::Space && self.press.is_none() {
                self.goal.yaw += SPIN * dt * if self.focus.is_some() { 0.45 } else { 1.0 };
            }
        }
        if let (Style::Orbit, Some(cluster)) = (self.style, self.focus) {
            let time = self.time;
            self.goal.target = self.orbit().anchor(cluster, time);
        }
        let ease = |rate: f32| 1.0 - (-rate * dt).exp();
        self.camera.yaw = lerp(self.camera.yaw, self.goal.yaw, ease(9.0));
        self.camera.pitch = lerp(self.camera.pitch, self.goal.pitch, ease(9.0));
        self.camera.zoom *= (self.goal.zoom / self.camera.zoom.max(1e-3)).powf(ease(6.0));
        self.camera.target = self.camera.target.lerp(self.goal.target, ease(6.0));
        let wanted = if self.focus.is_some() { 1.0 } else { 0.0 };
        self.focus_mix = lerp(self.focus_mix, wanted, ease(5.0));
        if (self.focus_mix - wanted).abs() < 0.004 {
            self.focus_mix = wanted;
        }
        self.made = lerp(self.made, self.inset, ease(7.0));
        if (self.made - self.inset).abs() < 0.5 {
            self.made = self.inset;
        }
    }

    /// Whether the picture is still changing by itself: a UI keeps drawing frames while it is.
    #[must_use]
    pub fn busy(&self) -> bool {
        if !self.paused {
            return true;
        }
        let wanted = if self.focus.is_some() { 1.0 } else { 0.0 };
        self.birth < 1.0
            || (self.camera.yaw - self.goal.yaw).abs() > 2e-3
            || (self.camera.pitch - self.goal.pitch).abs() > 2e-3
            || (self.camera.zoom / self.goal.zoom - 1.0).abs() > 2e-3
            || (self.camera.target - self.goal.target).len() > 0.02
            || (self.focus_mix - wanted).abs() > 0.004
            || (self.made - self.inset).abs() > 0.5
    }

    /// Draws a frame into `data`: RGBA with premultiplied alpha, `size()` pixels, rows from the top. The
    /// galaxy is opaque; the solar system is see-through around its bodies.
    pub fn render(&mut self, data: &mut [u8]) {
        let (width, height) = (self.width, self.height);
        if width == 0 || height == 0 || data.len() < width * height * 4 {
            return;
        }
        let graph = Arc::clone(&self.graph);
        match self.style {
            Style::Space => {
                let view = SpaceView {
                    camera: self.camera,
                    time: self.time,
                    birth: self.birth,
                    focus: self.focus,
                    focus_mix: self.focus_mix,
                    hover: self.hover,
                    selected: self.selected,
                    unit: self.unit,
                    slide: self.slide(),
                };
                let mut frame = Frame::new(data, width, height, Blend::Add);
                self.space().draw(&mut frame, &graph, &view);
            }
            Style::Orbit => {
                let view = OrbitView {
                    camera: self.camera,
                    time: self.time,
                    birth: self.birth,
                    focus: self.focus,
                    focus_mix: self.focus_mix,
                    hover: self.hover,
                    selected: self.selected,
                    unit: self.unit,
                    slide: self.slide(),
                    palette: self.palette,
                };
                let mut frame = Frame::new(data, width, height, Blend::Over);
                self.orbit().draw(&mut frame, &graph, &view);
            }
        }
    }

    fn marker(&self, seen: &Seen, below: f32, alpha: f32) -> Marker {
        Marker {
            x: seen.x / self.unit,
            y: (seen.y + below) / self.unit,
            alpha: alpha.clamp(0.0, 1.0),
        }
    }

    /// Where the names go over the frame drawn last.
    #[must_use]
    pub fn overlay(&self) -> Overlay {
        let mut overlay = Overlay::default();
        let (width, height) = (self.width as f32, self.height as f32);
        if width < 1.0 || height < 1.0 {
            return overlay;
        }
        let arrived = ((self.birth - 0.35) / 0.65).clamp(0.0, 1.0);
        let point =
            |seen: Option<Seen>, node: usize| seen.map(|seen| (node, self.marker(&seen, 0.0, 1.0)));
        match self.style {
            Style::Space => {
                let Some(space) = &self.space else {
                    return overlay;
                };
                let lens = Space::lens(&self.camera, width, height).slid(self.slide(), 0.0);
                let grow = 0.7 + 0.3 * self.birth;
                let stand = lens.distance();
                let hole = space.hole_seen();
                for cluster in 0..self.graph.clusters.len() {
                    let Some(mut seen) = lens
                        .see(space.anchor(cluster) * grow)
                        .and_then(|seen| space.shown(seen))
                    else {
                        continue;
                    };
                    // A name keeps clear of the hole and its disc.
                    if let Some(hole) = hole {
                        let (dx, dy) = (seen.x - hole.x, seen.y - hole.y);
                        let (away, clear) = (dx.hypot(dy), hole.scale * 5.0);
                        if away < clear {
                            let (dx, dy) = if away < 1.0 {
                                (0.0, -1.0)
                            } else {
                                (dx / away, dy / away)
                            };
                            seen.x = hole.x + dx * clear;
                            seen.y = hole.y + dy * clear;
                        }
                    }
                    // Names on the far side are fainter.
                    let depth = ((seen.depth - stand) / crate::space::GALAXY).clamp(-1.0, 1.0);
                    let alpha = arrived * (1.0 - self.focus_mix) * (0.8 - 0.3 * depth);
                    overlay
                        .labels
                        .push((cluster, self.marker(&seen, 0.0, alpha)));
                }
                // Two names never lie on top of each other: the later one steps down or up.
                for i in 1..overlay.labels.len() {
                    for j in 0..i {
                        let other = overlay.labels[j].1;
                        let label = &mut overlay.labels[i].1;
                        if (label.x - other.x).abs() < 96.0 && (label.y - other.y).abs() < 30.0 {
                            label.y = other.y + if label.y < other.y { -30.0 } else { 30.0 };
                        }
                    }
                }
                overlay.core = space.hole_seen().map(|seen| {
                    self.marker(
                        &seen,
                        seen.scale * 1.95,
                        arrived * (1.0 - 0.7 * self.focus_mix),
                    )
                });
                overlay.hover = self.hover.and_then(|node| point(space.seen(node), node));
                overlay.selected = self.selected.and_then(|node| point(space.seen(node), node));
            }
            Style::Orbit => {
                let Some(orbit) = &self.orbit else {
                    return overlay;
                };
                let lens = orbit
                    .lens(&self.camera, width, height, self.unit)
                    .slid(self.slide(), 0.0);
                let flat = self.camera.pitch.sin().abs();
                let grow = 0.35 + 0.65 * self.birth;
                for cluster in 0..self.graph.clusters.len() {
                    let reach = orbit.system_reach(cluster);
                    if reach <= 0.0 {
                        continue;
                    }
                    let Some(seen) = lens.see(orbit.anchor(cluster, self.time) * grow) else {
                        continue;
                    };
                    let quiet = match self.focus {
                        Some(focus) if focus != cluster => lerp(1.0, 0.35, self.focus_mix),
                        _ => 1.0,
                    };
                    overlay.labels.push((
                        cluster,
                        self.marker(
                            &seen,
                            reach * seen.scale * flat + 5.0 * self.unit,
                            arrived * quiet,
                        ),
                    ));
                }
                overlay.core = orbit
                    .sun_seen()
                    .map(|seen| self.marker(&seen, 15.0 * seen.scale + 6.0 * self.unit, arrived));
                overlay.hover = self.hover.and_then(|node| point(orbit.seen(node), node));
                overlay.selected = self.selected.and_then(|node| point(orbit.seen(node), node));
            }
        }
        overlay
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tests::sample;

    fn engine(style: Style) -> Engine {
        let graph = Arc::new(Graph::parse(&sample(4, 30)).expect("graph"));
        let mut engine = Engine::new(graph, style);
        engine.resize(800, 440, 1.0);
        engine
    }

    fn frame(engine: &mut Engine) -> Vec<u8> {
        let (width, height) = engine.size();
        let mut data = vec![0u8; width * height * 4];
        engine.render(&mut data);
        data
    }

    fn settle(engine: &mut Engine) {
        for _ in 0..120 {
            engine.step(0.05);
        }
    }

    #[test]
    fn both_styles_draw_and_name_every_community() {
        for style in [Style::Orbit, Style::Space] {
            let mut engine = engine(style);
            settle(&mut engine);
            let data = frame(&mut engine);
            assert!(data.iter().any(|b| *b > 100), "{style:?}");
            let overlay = engine.overlay();
            assert!(overlay.labels.len() >= 3, "{style:?}");
            assert!(
                overlay.labels.iter().all(|(_, m)| m.alpha > 0.1),
                "{style:?}"
            );
            let core = overlay.core.expect("core");
            assert!(
                (core.x - 400.0).abs() < 2.0 && core.y > 220.0,
                "{style:?} {core:?}"
            );
        }
        assert_eq!(Style::named("space"), Style::Space);
        assert_eq!(Style::named("anything"), Style::Orbit);
        assert_eq!(Style::named(Style::Orbit.name()), Style::Orbit);
    }

    #[test]
    fn a_click_chooses_a_node_and_a_second_click_lets_go() {
        for style in [Style::Orbit, Style::Space] {
            let mut engine = engine(style);
            engine.set_paused(true);
            settle(&mut engine);
            frame(&mut engine);
            // Find a node on the picture by asking where the hovered one is.
            let mut found = None;
            'search: for y in (20..420).step_by(6) {
                for x in (20..780).step_by(6) {
                    if engine.pointer_move(x as f32, y as f32) {
                        if let Some(node) = engine.hover() {
                            found = Some((node, x as f32, y as f32));
                            break 'search;
                        }
                    }
                }
            }
            let (node, x, y) = found.expect("a node under the pointer somewhere");
            engine.pointer_down(x, y);
            assert!(engine.pointer_up(x, y));
            assert_eq!(engine.selected(), Some(node), "{style:?}");
            frame(&mut engine);
            let at = engine.overlay().selected.expect("marker").1;
            assert!((at.x - x).hypot(at.y - y) < 20.0, "{style:?}");
            engine.pointer_down(x, y);
            engine.pointer_up(x, y);
            assert_eq!(engine.selected(), None, "{style:?}");
        }
    }

    #[test]
    fn dragging_turns_the_galaxy_and_is_not_a_click() {
        let mut engine = engine(Style::Space);
        settle(&mut engine);
        let before = engine.camera;
        engine.pointer_down(300.0, 200.0);
        engine.pointer_drag(360.0, 240.0);
        assert!(!engine.pointer_up(360.0, 240.0));
        assert!(engine.camera.yaw < before.yaw && engine.camera.pitch > before.pitch);
        engine.pointer_down(300.0, 200.0);
        engine.pointer_drag(300.0, 5000.0);
        engine.pointer_up(300.0, 5000.0);
        assert!(engine.camera.pitch <= 1.4);
    }

    #[test]
    fn dragging_slides_the_solar_system_with_the_hand() {
        let mut engine = engine(Style::Orbit);
        engine.set_paused(true);
        settle(&mut engine);
        frame(&mut engine);
        let before = engine.overlay().core.expect("core");
        engine.pointer_down(300.0, 200.0);
        engine.pointer_drag(350.0, 230.0);
        engine.pointer_up(350.0, 230.0);
        frame(&mut engine);
        let after = engine.overlay().core.expect("core");
        assert!(
            (after.x - before.x - 50.0).abs() < 1.5,
            "{before:?} {after:?}"
        );
        assert!(
            (after.y - before.y - 30.0).abs() < 1.5,
            "{before:?} {after:?}"
        );
    }

    #[test]
    fn it_rests_when_paused_and_settled_and_moves_again_when_asked() {
        let mut engine = engine(Style::Space);
        assert!(engine.busy());
        engine.set_paused(true);
        assert!(engine.busy(), "still arriving");
        settle(&mut engine);
        assert!(!engine.busy());
        let still = frame(&mut engine);
        engine.step(0.05);
        assert_eq!(
            still,
            frame(&mut engine),
            "a paused picture does not change"
        );
        engine.zoom_by(1.5);
        assert!(engine.busy());
        engine.set_focus(Some(1));
        settle(&mut engine);
        assert!(!engine.busy());
        assert_eq!(engine.focus(), Some(1));
        assert!(
            engine.overlay().labels.iter().all(|(_, m)| m.alpha < 0.01),
            "names step back in focus"
        );
        engine.reset();
        assert_eq!((engine.focus(), engine.selected()), (None, None));
        engine.zoom_by(1000.0);
        settle(&mut engine);
        assert!(engine.camera.zoom <= 3.0 + 1e-3);
    }

    #[test]
    fn the_solar_system_follows_a_planet_in_focus() {
        let mut engine = engine(Style::Orbit);
        engine.set_focus(Some(0));
        settle(&mut engine);
        frame(&mut engine);
        let label = engine
            .overlay()
            .labels
            .into_iter()
            .find(|(cluster, _)| *cluster == 0)
            .expect("label")
            .1;
        assert!(
            (label.x - 400.0).abs() < 30.0,
            "the planet is kept in the middle: {label:?}"
        );
        assert!(engine.camera.zoom > 1.2);
    }

    #[test]
    fn a_panel_moves_the_middle_of_the_picture_over() {
        for style in [Style::Orbit, Style::Space] {
            let mut engine = engine(style);
            engine.set_paused(true);
            settle(&mut engine);
            frame(&mut engine);
            let before = engine.overlay().core.expect("core");
            engine.set_inset(300.0);
            assert!(engine.busy());
            settle(&mut engine);
            assert!(!engine.busy());
            frame(&mut engine);
            let after = engine.overlay().core.expect("core");
            assert!(
                (before.x - after.x - 150.0).abs() < 1.0,
                "{style:?} {before:?} {after:?}"
            );
        }
    }

    #[test]
    fn a_huge_canvas_is_drawn_smaller() {
        let mut engine = engine(Style::Space);
        engine.resize(3840, 2160, 2.0);
        let (width, height) = engine.size();
        assert!(width * height <= MAX_PIXELS + 4000, "{width}x{height}");
        settle(&mut engine);
        frame(&mut engine);
        let core = engine.overlay().core.expect("core");
        assert!(
            (core.x - 960.0).abs() < 2.0,
            "places stay in logical pixels: {core:?}"
        );
        engine.resize(0, 0, 1.0);
        frame(&mut engine);
        assert_eq!(engine.overlay(), Overlay::default());
    }

    #[test]
    fn choosing_from_a_list_turns_the_galaxy_to_the_node() {
        let mut engine = engine(Style::Space);
        engine.set_paused(true);
        settle(&mut engine);
        let node = 17;
        engine.select(Some(node), true);
        settle(&mut engine);
        frame(&mut engine);
        let lens = Space::lens(&engine.camera, 800.0, 440.0);
        let place = engine.space().place(node).expect("place");
        let seen = lens.see(place).expect("seen");
        assert!(
            seen.depth < lens.distance(),
            "the chosen star is on the near side"
        );
        engine.select(Some(9999), true);
        assert_eq!(engine.selected(), None);
    }
}
