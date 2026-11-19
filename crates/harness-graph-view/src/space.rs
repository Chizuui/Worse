//! The galaxy picture: every node a star, every community a constellation, all of it around a black hole
//! and seen through a camera that can circle it.
//!
//! Nothing here is simulated. Each community gets a patch of sky and grows from its best connected node
//! outwards, link by link, so linked stars stand close and the lines between them stay short. Dust, the
//! grains that circle the hole and the comets beyond the stars are scenery, drawn from one fixed seed: the
//! same graph is the same sky every time.

use std::collections::VecDeque;

use crate::camera::{Camera, Lens, Seen};
use crate::hole::{Hole, HoleView, BEND};
use crate::math::{lerp, smoothstep, Noise, Rng, Vec3, GOLDEN, TAU};
use crate::model::Graph;
use crate::raster::{Blend, Frame, Rgb};

/// How far the stars reach, in radii of the hole's shadow (the unit of this world).
pub const GALAXY: f32 = 9.5;
/// At zoom 1 the galaxy's radius is this share of the frame's shorter side.
const SPREAD: f32 = 0.43;
/// At zoom 1 the eye stands this many galaxy radii away.
const STAND: f32 = 3.4;
/// No star nearer the hole than this.
const CLEARING: f32 = 4.7;
/// The galaxy is a ball pressed a little flat.
const FLATTEN: f32 = 0.86;
/// The gas disc leans a little, so its bar tips one way and then the other as the camera goes round.
const DISC_LEAN: f32 = 0.09;
const WHITE: Rgb = [1.0, 1.0, 1.0];
const COOL: Rgb = [0.8, 0.87, 1.0];
const WARM: Rgb = [1.0, 0.93, 0.8];
/// The cloud layer is worked out at a quarter of the frame's size and stretched.
const CLOUD_STEP: usize = 4;

struct Star {
    at: Vec3,
    size: f32,
    light: f32,
    /// Length of the four rays of a bright star; 0 for none.
    flare: f32,
    tint: Rgb,
    cluster: usize,
    beat: f32,
    pace: f32,
}

struct Link {
    a: usize,
    b: usize,
    /// How bright the line is at each of its ends: brightest at the better connected star.
    light: (f32, f32),
}

struct Mote {
    at: Vec3,
    light: f32,
}

/// A grain of dust circling the hole, seen as a short dash along its path.
struct Grain {
    e1: Vec3,
    e2: Vec3,
    radius: f32,
    start: f32,
    pace: f32,
    trail: f32,
    light: f32,
}

/// A ring of evenly spaced dots turning around the hole: an orbit crowded with grains.
struct Shell {
    e1: Vec3,
    e2: Vec3,
    radius: f32,
    count: usize,
    pace: f32,
    light: f32,
}

/// A bright head on a wide circle around the whole galaxy, with a tail along its path. The circle always
/// faces the eye, a little squashed and turned, so the comets stay at the rim of the picture whichever way
/// the camera looks.
struct Comet {
    squash: f32,
    lean: f32,
    radius: f32,
    pace: f32,
    start: f32,
    tail: f32,
}

/// What one frame shows.
#[derive(Debug, Clone, Copy)]
pub struct SpaceView {
    pub camera: Camera,
    /// Seconds; drives the gas, the grains, the comets and the twinkle.
    pub time: f32,
    /// 0 to 1: the picture arriving.
    pub birth: f32,
    /// The community being looked at, and how far the others have dimmed (0 to 1).
    pub focus: Option<usize>,
    pub focus_mix: f32,
    pub hover: Option<usize>,
    pub selected: Option<usize>,
    /// Frame pixels per logical pixel: sizes are given in logical pixels.
    pub unit: f32,
    /// How far the middle of the picture is moved to the right, in frame pixels.
    pub slide: f32,
}

pub struct Space {
    stars: Vec<Star>,
    links: Vec<Link>,
    anchors: Vec<Vec3>,
    motes: Vec<Mote>,
    grains: Vec<Grain>,
    shells: Vec<Shell>,
    comets: Vec<Comet>,
    hole: Hole,
    clouds: Noise,
    backdrop: Vec<u8>,
    backdrop_size: (usize, usize),
    seen: Vec<Option<Seen>>,
    hole_seen: Option<Seen>,
}

/// `count` places spread evenly through the galaxy: each is the best of a few tries, the one furthest from
/// all the places before it.
fn scatter(count: usize, rng: &mut Rng) -> Vec<Vec3> {
    let mut places: Vec<Vec3> = Vec::with_capacity(count);
    for _ in 0..count {
        let mut best = (Vec3::ZERO, -1.0_f32);
        for _ in 0..10 {
            // Between even over the picture and even through the ball: a little denser towards the middle.
            let mut place = rng.direction() * (GALAXY * rng.unit().powf(1.0 / 2.4));
            place.y *= FLATTEN;
            if place.len() < CLEARING {
                place = place.unit() * (CLEARING + rng.unit());
            }
            let nearest = places
                .iter()
                .map(|other| {
                    let d = *other - place;
                    d.dot(d)
                })
                .fold(f32::MAX, f32::min);
            if nearest > best.1 {
                best = (place, nearest);
            }
        }
        places.push(best.0);
    }
    places
}

/// Two directions across `axis`, at right angles to each other: with the axis they make a right-handed set.
fn plane(axis: Vec3) -> (Vec3, Vec3) {
    let helper = if axis.y.abs() < 0.9 {
        Vec3::UP
    } else {
        Vec3::new(1.0, 0.0, 0.0)
    };
    let e1 = axis.cross(helper).unit();
    (e1, axis.cross(e1))
}

fn disc_axis() -> Vec3 {
    Vec3::new(DISC_LEAN.sin(), DISC_LEAN.cos(), 0.0)
}

/// Where something behind the hole is seen: its light is bent around the hole, so it appears pushed away
/// from the hole's middle, onto a ring when it stands right behind. `None` when it is hidden by the shadow.
fn bent(seen: Seen, hole: Option<&Seen>) -> Option<Seen> {
    let Some(hole) = hole else {
        return Some(seen);
    };
    let behind = seen.depth - hole.depth;
    if behind <= 0.0 {
        return Some(seen);
    }
    let (dx, dy) = (seen.x - hole.x, seen.y - hole.y);
    let away = dx.hypot(dy) / hole.scale;
    if away > 12.0 {
        return Some(seen);
    }
    let lifted = 0.5 * (away + (away * away + 4.0 * BEND * behind).sqrt());
    if lifted < 1.0 {
        return None;
    }
    let stretch = lifted / away.max(1e-3);
    Some(Seen {
        x: hole.x + dx * stretch,
        y: hole.y + dy * stretch,
        ..seen
    })
}

impl Space {
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn new(graph: &Graph) -> Self {
        let mut rng = Rng::new(0x5EED_CAFE);
        let count = graph.nodes.len();
        let groups = graph.clusters.len().max(1);
        // One place for every node, spread evenly through the galaxy.
        let places = scatter(count, &mut rng);
        // A patch of sky for each community: the largest near the galaxy's equator, where there is most
        // room, the smaller ones above and below, each a golden angle further round. Every place goes to
        // the community whose direction it is nearest to, as long as that community still needs places.
        let directions: Vec<Vec3> = (0..groups)
            .map(|c| {
                let side = if c % 2 == 0 { 1.0 } else { -1.0 };
                let height = if groups == 1 {
                    0.0
                } else {
                    side * ((c / 2) as f32 + 0.5) / groups.div_ceil(2) as f32 * 0.72
                };
                let around = c as f32 * GOLDEN + 0.6;
                let level = (1.0 - height * height).sqrt();
                Vec3::new(level * around.cos(), height, level * around.sin())
            })
            .collect();
        let mut choices: Vec<(f32, usize, usize)> = Vec::with_capacity(count * groups);
        for (p, place) in places.iter().enumerate() {
            let toward = place.unit();
            for (c, direction) in directions.iter().enumerate() {
                choices.push((1.0 - toward.dot(*direction), p, c));
            }
        }
        choices.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        let mut room: Vec<usize> = graph.clusters.iter().map(|c| c.members.len()).collect();
        let mut owned: Vec<Vec<usize>> = vec![Vec::new(); groups];
        let mut taken = vec![false; count];
        for &(_, p, c) in &choices {
            if !taken[p] && room.get(c).copied().unwrap_or(0) > 0 {
                taken[p] = true;
                room[c] -= 1;
                owned[c].push(p);
            }
        }
        // Inside its patch a community grows from its hub: each node takes the free place nearest to the
        // node it was reached from, so linked stars stand close together.
        let mut at = vec![Vec3::ZERO; count];
        let mut placed = vec![false; count];
        let mut anchors = Vec::with_capacity(groups);
        for (c, cluster) in graph.clusters.iter().enumerate() {
            let mut free: Vec<Vec3> = owned[c].iter().map(|&p| places[p]).collect();
            let middle =
                free.iter().fold(Vec3::ZERO, |sum, &p| sum + p) * (1.0 / free.len().max(1) as f32);
            anchors.push(middle);
            let mut take = |near: Vec3| -> Vec3 {
                let mut best = 0;
                let mut best_away = f32::MAX;
                for (i, place) in free.iter().enumerate() {
                    let away = (*place - near).len();
                    if away < best_away {
                        best = i;
                        best_away = away;
                    }
                }
                if free.is_empty() {
                    near
                } else {
                    free.swap_remove(best)
                }
            };
            for &root in &cluster.members {
                if placed[root] {
                    continue;
                }
                at[root] = take(middle);
                placed[root] = true;
                let mut queue = VecDeque::from([root]);
                while let Some(node) = queue.pop_front() {
                    for &next in graph.neighbours(node) {
                        if placed[next] || graph.nodes[next].cluster != c {
                            continue;
                        }
                        at[next] = take(at[node]);
                        placed[next] = true;
                        queue.push_back(next);
                    }
                }
            }
        }
        let top = graph
            .nodes
            .iter()
            .map(|n| n.degree)
            .max()
            .unwrap_or(1)
            .max(1) as f32;
        let rank = |node: usize| (graph.nodes[node].degree as f32 / top).sqrt();
        let stars: Vec<Star> = graph
            .nodes
            .iter()
            .enumerate()
            .zip(&at)
            .map(|((i, node), &at)| {
                let rank = rank(i);
                let warmth = rng.unit();
                Star {
                    at,
                    // A few large soft lights among many small ones.
                    size: 0.42
                        + 0.9 * rank * rank
                        + if node.hub { 0.35 } else { 0.0 }
                        + 0.25 * rng.unit(),
                    light: 0.3 + 0.7 * rank.sqrt(),
                    flare: if node.hub || rank > 0.6 {
                        0.35 + 0.65 * rank
                    } else {
                        0.0
                    },
                    tint: if warmth < 0.3 {
                        WARM
                    } else if warmth > 0.85 {
                        COOL
                    } else {
                        WHITE
                    },
                    cluster: node.cluster,
                    beat: rng.range(0.0, TAU),
                    pace: rng.range(0.7, 2.1),
                }
            })
            .collect();
        // The constellation lines: links between stars that stand near each other. A line is brightest at
        // its better connected end and fades towards the other, so a hub reads as a burst of rays. Long
        // links are left out, they would only cross the picture; choosing a star shows all of its links.
        let volume = 4.19 * (GALAXY.powi(3) - CLEARING.powi(3)) * FLATTEN;
        let apart = (volume / count.max(1) as f32).cbrt();
        let longest = (apart * 1.5).max(1.5);
        let links = graph
            .links
            .iter()
            .filter_map(|&(a, b)| {
                let long = (at[a] - at[b]).len() / longest;
                (long < 1.0).then(|| {
                    let base = 0.035 + 0.1 * (1.0 - long);
                    Link {
                        a,
                        b,
                        light: (base * (0.3 + 1.5 * rank(a)), base * (0.3 + 1.5 * rank(b))),
                    }
                })
            })
            .collect();

        let motes = (0..1700)
            .map(|_| {
                let mut at = rng.direction() * (GALAXY * (0.3 + 0.95 * rng.unit().powf(0.6)));
                at.y *= 0.8;
                let dim = rng.unit();
                Mote {
                    at,
                    light: 0.06 + 0.4 * dim * dim * dim,
                }
            })
            .collect();
        // Grains around the hole: most of them in the plane of the gas disc, out beyond its rim, where the
        // disc thins into single grains; the rest a loose ball around the hole. They turn the way the gas
        // does, the inner ones faster.
        let axis = disc_axis();
        let grains = (0..1500)
            .map(|i| {
                let flat = i % 5 != 0;
                let (normal, radius) = if flat {
                    (
                        (axis + rng.direction() * 0.06).unit(),
                        3.3 + 4.0 * rng.unit().powf(1.7),
                    )
                } else {
                    (rng.direction(), 1.8 + 4.4 * rng.unit().powf(1.3))
                };
                let (e1, e2) = plane(normal);
                Grain {
                    e1,
                    e2,
                    radius,
                    start: rng.range(0.0, TAU),
                    pace: -0.36 * radius.powf(-1.5),
                    trail: rng.range(0.006, 0.03),
                    light: 0.14 + 0.5 * rng.unit().powi(2),
                }
            })
            .collect();
        let shells = (0..9)
            .map(|i| {
                let flat = i < 6;
                let (normal, radius) = if flat {
                    ((axis + rng.direction() * 0.03).unit(), 4.4 + 0.5 * i as f32)
                } else {
                    (rng.direction(), 2.0 + 0.6 * (i - 6) as f32)
                };
                let (e1, e2) = plane(normal);
                Shell {
                    e1,
                    e2,
                    radius,
                    count: (TAU * radius / 0.24) as usize,
                    pace: -0.36 * radius.powf(-1.5),
                    light: 0.2 - 0.012 * i as f32,
                }
            })
            .collect();
        let comets = (0..11)
            .map(|i| Comet {
                squash: rng.range(0.82, 1.0),
                lean: rng.range(0.0, TAU),
                radius: GALAXY * (1.18 + 0.6 * rng.unit()),
                pace: rng.range(0.07, 0.14) * if i % 3 == 0 { -1.0 } else { 1.0 },
                start: rng.range(0.0, TAU),
                tail: rng.range(0.5, 1.2),
            })
            .collect();
        Self {
            stars,
            links,
            anchors,
            motes,
            grains,
            shells,
            comets,
            hole: Hole::new(),
            clouds: Noise::new(64, 0x00C1_00D5),
            backdrop: Vec::new(),
            backdrop_size: (0, 0),
            seen: Vec::new(),
            hole_seen: None,
        }
    }

    /// The camera for a frame of this size.
    #[must_use]
    pub fn lens(camera: &Camera, width: f32, height: f32) -> Lens {
        let fit = height.min(width * 0.62);
        Lens::deep(camera, width, height, GALAXY, SPREAD * fit, STAND)
    }

    /// The middle of community `cluster`, where its name goes.
    #[must_use]
    pub fn anchor(&self, cluster: usize) -> Vec3 {
        self.anchors.get(cluster).copied().unwrap_or(Vec3::ZERO)
    }

    /// Where node `node` stands in the galaxy.
    #[must_use]
    pub fn place(&self, node: usize) -> Option<Vec3> {
        self.stars.get(node).map(|star| star.at)
    }

    /// Where node `node` was in the last frame drawn; `None` also while the hole hides it.
    #[must_use]
    pub fn seen(&self, node: usize) -> Option<Seen> {
        self.seen.get(node).copied().flatten()
    }

    /// Where the hole was in the last frame drawn; its `scale` is the shadow's radius in pixels.
    #[must_use]
    pub fn hole_seen(&self) -> Option<Seen> {
        self.hole_seen
    }

    /// Where a point of the galaxy shows on the frame drawn last, its light bent around the hole like the
    /// stars'; `None` when the hole hides it.
    #[must_use]
    pub fn shown(&self, seen: Seen) -> Option<Seen> {
        bent(seen, self.hole_seen.as_ref())
    }

    /// The star under a pointer at `(x, y)`, within `reach` pixels: the nearest on the frame, and of two
    /// about as near, the one nearer the eye. With a community in focus only its stars count.
    #[must_use]
    pub fn pick(&self, x: f32, y: f32, reach: f32, focus: Option<usize>) -> Option<usize> {
        let mut best: Option<(usize, f32)> = None;
        for (i, seen) in self.seen.iter().enumerate() {
            let Some(seen) = seen else { continue };
            if focus.is_some_and(|c| self.stars[i].cluster != c) {
                continue;
            }
            let away = (seen.x - x).hypot(seen.y - y);
            if away > reach {
                continue;
            }
            // A star nearer the eye wins over one a couple of pixels closer to the pointer.
            let score = away + seen.depth * 0.08;
            if best.map_or(true, |(_, least)| score < least) {
                best = Some((i, score));
            }
        }
        best.map(|(i, _)| i)
    }

    fn paint_backdrop(&mut self, frame: &mut Frame, unit: f32) {
        let (width, height) = (frame.width(), frame.height());
        if self.backdrop_size != (width, height) {
            self.backdrop = clouds(&self.clouds, width, height, unit);
            self.backdrop_size = (width, height);
        }
        let bytes = frame.bytes();
        let len = bytes.len().min(self.backdrop.len());
        bytes[..len].copy_from_slice(&self.backdrop[..len]);
    }

    /// Draws one frame; the frame is opaque when it is done.
    #[allow(clippy::too_many_lines)]
    pub fn draw(&mut self, frame: &mut Frame, graph: &Graph, view: &SpaceView) {
        let (width, height) = (frame.width(), frame.height());
        if width == 0 || height == 0 {
            return;
        }
        frame.set_blend(Blend::Add);
        let unit = view.unit.max(0.25);
        self.paint_backdrop(frame, unit);
        let lens = Self::lens(&view.camera, width as f32, height as f32).slid(view.slide, 0.0);
        let birth = smoothstep(0.0, 1.0, view.birth);
        // The stars fly out to their places as the picture arrives.
        let grow = 0.7 + 0.3 * birth;
        let stand = lens.distance();
        let hole_seen = lens.see(Vec3::ZERO);
        self.hole_seen = hole_seen;
        let hole = hole_seen.as_ref();
        let hole_depth = hole_seen.map_or(f32::MAX, |seen| seen.depth);
        self.seen.clear();
        self.seen.extend(
            self.stars
                .iter()
                .map(|star| lens.see(star.at * grow).and_then(|seen| bent(seen, hole))),
        );
        // With a community in focus the others all but go out, and it shines a little more.
        let quiet = lerp(1.0, 0.07, view.focus_mix);
        let lit = lerp(1.0, 1.3, view.focus_mix);
        let mute = |cluster: usize| -> f32 {
            match view.focus {
                Some(focus) if focus != cluster => quiet,
                Some(_) => lit,
                None => 1.0,
            }
        };
        let scenery = birth * lerp(1.0, 0.5, view.focus_mix);
        // Nearer the eye: brighter and a little larger.
        let nearness = |seen: &Seen| (stand / seen.depth).clamp(0.4, 3.0);
        let line = unit.max(1.0);

        for far in [true, false] {
            for mote in &self.motes {
                let Some(seen) = lens.see(mote.at * grow).and_then(|seen| bent(seen, hole)) else {
                    continue;
                };
                if (seen.depth > hole_depth) == far {
                    frame.dot(
                        seen.x,
                        seen.y,
                        WHITE,
                        mote.light * scenery * nearness(&seen).powf(1.4),
                    );
                }
            }
            for grain in &self.grains {
                let angle = grain.start + grain.pace * view.time;
                let at = |angle: f32| -> Option<Seen> {
                    let (sin, cos) = angle.sin_cos();
                    lens.see((grain.e1 * cos + grain.e2 * sin) * (grain.radius * grow))
                        .and_then(|seen| bent(seen, hole))
                };
                let Some(head) = at(angle) else { continue };
                if (head.depth > hole_depth) != far {
                    continue;
                }
                let gain = grain.light * scenery * nearness(&head);
                match at(angle + grain.trail) {
                    Some(tail) => frame.line(
                        head.x,
                        head.y,
                        tail.x,
                        tail.y,
                        line * 1.2,
                        WHITE,
                        gain,
                        gain * 0.3,
                    ),
                    None => frame.dot(head.x, head.y, WHITE, gain),
                }
            }
            for shell in &self.shells {
                let turn = shell.pace * view.time;
                for i in 0..shell.count {
                    let (sin, cos) = (TAU * i as f32 / shell.count as f32 + turn).sin_cos();
                    let point = (shell.e1 * cos + shell.e2 * sin) * (shell.radius * grow);
                    let Some(seen) = lens.see(point).and_then(|seen| bent(seen, hole)) else {
                        continue;
                    };
                    if (seen.depth > hole_depth) == far {
                        frame.dot(
                            seen.x,
                            seen.y,
                            WHITE,
                            shell.light * scenery * nearness(&seen),
                        );
                    }
                }
            }
            for link in &self.links {
                let (Some(a), Some(b)) = (self.seen[link.a], self.seen[link.b]) else {
                    continue;
                };
                let depth = (a.depth + b.depth) * 0.5;
                if (depth > hole_depth) != far {
                    continue;
                }
                let shown = mute(self.stars[link.a].cluster).min(mute(self.stars[link.b].cluster));
                let gain = birth * shown * shown * (stand / depth).clamp(0.5, 2.0);
                frame.line(
                    a.x,
                    a.y,
                    b.x,
                    b.y,
                    line,
                    WHITE,
                    link.light.0 * gain,
                    link.light.1 * gain,
                );
            }
            for (star, seen) in self.stars.iter().zip(&self.seen) {
                let Some(seen) = seen else { continue };
                if (seen.depth > hole_depth) != far {
                    continue;
                }
                let near = nearness(seen);
                let twinkle = 1.0 + 0.2 * (view.time * star.pace + star.beat).sin();
                let gain = star.light * twinkle * birth * mute(star.cluster) * near.powf(1.2);
                let size = star.size * unit * near.powf(0.75) * 0.6;
                // A hot middle, a soft light around it, and a wide faint haze.
                frame.glow(seen.x, seen.y, size, WHITE, gain * 1.3);
                frame.glow(seen.x, seen.y, size * 2.2, star.tint, gain * 0.36);
                frame.glow(seen.x, seen.y, size * 5.0, star.tint, gain * 0.06);
                if star.flare > 0.0 {
                    sparkle(
                        frame,
                        seen.x,
                        seen.y,
                        unit * near * (1.5 + 4.5 * star.flare),
                        gain * 0.2,
                        line,
                    );
                }
            }
            if far {
                if let Some(seen) = hole_seen {
                    self.hole.draw(
                        frame,
                        &HoleView {
                            x: seen.x,
                            y: seen.y,
                            radius: seen.scale * grow,
                            normal: lens.turn(disc_axis()),
                            time: view.time,
                            gain: birth,
                        },
                    );
                }
            }
        }

        let roaming = birth * (1.0 - view.focus_mix);
        if roaming > 0.01 {
            const PIECES: usize = 28;
            let (right, up) = lens.axes();
            for comet in &self.comets {
                let head = comet.start + comet.pace * view.time;
                let back = -comet.pace.signum() * comet.tail / PIECES as f32;
                let (lean_sin, lean_cos) = comet.lean.sin_cos();
                let at = |piece: usize| -> Option<Seen> {
                    let (sin, cos) = (head + back * piece as f32).sin_cos();
                    let (x, y) = (cos, sin * comet.squash);
                    let across =
                        right * (x * lean_cos - y * lean_sin) + up * (x * lean_sin + y * lean_cos);
                    lens.see(view.camera.target + across * comet.radius)
                };
                let strength = |piece: usize| -> f32 {
                    let along = 1.0 - piece as f32 / PIECES as f32;
                    0.6 * roaming * along * along
                };
                let mut from = at(0);
                if let Some(seen) = from {
                    let near = nearness(&seen);
                    frame.glow(seen.x, seen.y, 1.0 * unit * near, WHITE, roaming * 1.3);
                    frame.glow(seen.x, seen.y, 3.0 * unit * near, COOL, roaming * 0.22);
                }
                for piece in 1..=PIECES {
                    let to = at(piece);
                    if let (Some(a), Some(b)) = (from, to) {
                        frame.line(
                            a.x,
                            a.y,
                            b.x,
                            b.y,
                            line,
                            WHITE,
                            strength(piece - 1),
                            strength(piece),
                        );
                    }
                    from = to;
                }
            }
        }

        // The chosen star: lines to everything it is linked to, over the rest of the picture.
        if let Some((selected, seen)) = view.selected.and_then(|i| Some((i, self.seen(i)?))) {
            for &other in graph.neighbours(selected) {
                if let Some(to) = self.seen(other) {
                    frame.line(
                        seen.x,
                        seen.y,
                        to.x,
                        to.y,
                        line * 1.2,
                        WHITE,
                        0.95 * birth,
                        0.6 * birth,
                    );
                    frame.glow(to.x, to.y, 1.5 * unit, WHITE, birth);
                }
            }
            sparkle(frame, seen.x, seen.y, unit * 15.0, birth, line);
            frame.glow(seen.x, seen.y, 2.0 * unit, WHITE, 1.4 * birth);
        }
        if let Some(seen) = view
            .hover
            .filter(|i| view.selected != Some(*i))
            .and_then(|i| self.seen(i))
        {
            sparkle(frame, seen.x, seen.y, unit * 11.0, 0.9 * birth, line);
            frame.glow(seen.x, seen.y, 1.7 * unit, WHITE, birth);
        }
    }
}

/// Four long rays and four short ones between them.
fn sparkle(frame: &mut Frame, x: f32, y: f32, reach: f32, gain: f32, width: f32) {
    if gain <= 0.01 || reach < 1.5 {
        return;
    }
    for (dx, dy) in [(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0)] {
        frame.line(
            x,
            y,
            x + dx * reach,
            y + dy * reach,
            width,
            WHITE,
            gain,
            0.0,
        );
    }
    let short = reach * 0.33;
    for (dx, dy) in [(1.0, 1.0), (-1.0, 1.0), (1.0, -1.0), (-1.0, -1.0)] {
        frame.line(
            x,
            y,
            x + dx * short,
            y + dy * short,
            width,
            WHITE,
            gain * 0.6,
            0.0,
        );
    }
}

/// Faint grey smoke on black, in blotches, darker towards the corners; RGBA, opaque.
fn clouds(noise: &Noise, width: usize, height: usize, unit: f32) -> Vec<u8> {
    let columns = width / CLOUD_STEP + 2;
    let rows = height / CLOUD_STEP + 2;
    let mut coarse = Vec::with_capacity(columns * rows);
    for row in 0..rows {
        for column in 0..columns {
            // In logical pixels, so the smoke looks the same on any screen.
            let x = (column * CLOUD_STEP) as f32 / unit;
            let y = (row * CLOUD_STEP) as f32 / unit;
            let puffs = noise.fbm(x / 120.0, y / 120.0, 4);
            let patches = noise.fbm(x / 520.0 + 40.5, y / 520.0 + 9.5, 2);
            let edge_x = (x * unit / width.max(1) as f32 - 0.5) * 2.0;
            let edge_y = (y * unit / height.max(1) as f32 - 0.5) * 2.0;
            let corner = 1.0 - 0.3 * smoothstep(0.8, 1.5, edge_x.hypot(edge_y));
            coarse.push(
                smoothstep(0.3, 0.74, puffs) * smoothstep(0.36, 0.62, patches) * corner * 34.0,
            );
        }
    }
    let mut out = vec![0u8; width * height * 4];
    let step = CLOUD_STEP as f32;
    for y in 0..height {
        let fy = y as f32 / step;
        let row = fy as usize;
        let ty = fy - row as f32;
        for x in 0..width {
            let fx = x as f32 / step;
            let column = fx as usize;
            let tx = fx - column as f32;
            let i = row * columns + column;
            let grey = lerp(
                lerp(coarse[i], coarse[i + 1], tx),
                lerp(coarse[i + columns], coarse[i + columns + 1], tx),
                ty,
            );
            let o = (y * width + x) * 4;
            let level = (grey + 0.5) as u8;
            out[o] = level;
            out[o + 1] = level;
            out[o + 2] = level;
            out[o + 3] = 255;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tests::sample;

    fn view() -> SpaceView {
        SpaceView {
            camera: Camera {
                yaw: 0.5,
                pitch: 0.11,
                ..Camera::default()
            },
            time: 2.0,
            birth: 1.0,
            focus: None,
            focus_mix: 0.0,
            hover: None,
            selected: None,
            unit: 1.0,
            slide: 0.0,
        }
    }

    fn render(space: &mut Space, graph: &Graph, view: &SpaceView) -> Vec<u8> {
        let mut data = vec![0u8; 640 * 360 * 4];
        let mut frame = Frame::new(&mut data, 640, 360, Blend::Add);
        space.draw(&mut frame, graph, view);
        data
    }

    #[test]
    fn every_node_is_a_star_inside_the_galaxy_and_clear_of_the_hole() {
        let graph = Graph::parse(&sample(5, 60)).expect("graph");
        let space = Space::new(&graph);
        assert_eq!(space.stars.len(), graph.nodes.len());
        for star in &space.stars {
            let far = star.at.len();
            assert!((CLEARING - 0.01..=GALAXY + 0.01).contains(&far), "{far}");
        }
        assert!(!space.links.is_empty());
        // Communities stand apart: their middles are not all in one place.
        let apart = (space.anchor(0) - space.anchor(1)).len();
        assert!(apart > GALAXY * 0.3, "{apart}");
        // Linked stars stand closer together than stars picked at random.
        let mean = |pairs: &mut dyn Iterator<Item = (usize, usize)>| -> f32 {
            let (mut sum, mut n) = (0.0, 0);
            for (a, b) in pairs {
                sum += (space.stars[a].at - space.stars[b].at).len();
                n += 1;
            }
            sum / n.max(1) as f32
        };
        let linked = mean(&mut graph.links.iter().copied());
        let any = mean(&mut (0..graph.nodes.len() - 7).map(|i| (i, i + 7)));
        assert!(linked < any * 0.8, "{linked} against {any}");
    }

    #[test]
    fn the_same_graph_is_the_same_sky() {
        let graph = Graph::parse(&sample(4, 40)).expect("graph");
        let (mut a, mut b) = (Space::new(&graph), Space::new(&graph));
        assert_eq!(
            render(&mut a, &graph, &view()),
            render(&mut b, &graph, &view())
        );
    }

    #[test]
    fn a_frame_is_opaque_shows_stars_and_can_be_picked_from() {
        let graph = Graph::parse(&sample(4, 40)).expect("graph");
        let mut space = Space::new(&graph);
        let data = render(&mut space, &graph, &view());
        assert!(data.chunks_exact(4).all(|px| px[3] == 255));
        assert!(data.chunks_exact(4).filter(|px| px[0] > 200).count() > 200);
        let shown = (0..graph.nodes.len())
            .find(|&i| space.seen(i).is_some())
            .expect("a visible star");
        let seen = space.seen(shown).expect("seen");
        let picked = space.pick(seen.x + 1.0, seen.y, 8.0, None).expect("picked");
        let at = space.seen(picked).expect("seen");
        assert!((at.x - seen.x).hypot(at.y - seen.y) < 8.0);
        assert!(space.pick(-500.0, -500.0, 8.0, None).is_none());
        // In focus, only that community's stars answer.
        let other = (graph.nodes[shown].cluster + 1) % graph.clusters.len();
        assert!(space
            .pick(seen.x, seen.y, 2.0, Some(other))
            .map_or(true, |i| graph.nodes[i].cluster == other));
    }

    #[test]
    fn light_from_behind_the_hole_is_bent_around_it() {
        let hole = Seen {
            x: 100.0,
            y: 100.0,
            depth: 30.0,
            scale: 20.0,
        };
        let at = |x: f32, y: f32, depth: f32| Seen {
            x,
            y,
            depth,
            scale: 20.0,
        };
        // In front of the hole nothing changes.
        assert_eq!(
            bent(at(110.0, 100.0, 25.0), Some(&hole)),
            Some(at(110.0, 100.0, 25.0))
        );
        // Right behind it: hidden by the shadow when close, pushed out onto a ring when further back.
        assert_eq!(bent(at(104.0, 100.0, 30.5), Some(&hole)), None);
        let ring = bent(at(100.5, 100.0, 36.0), Some(&hole)).expect("seen");
        let away = (ring.x - 100.0) / 20.0;
        assert!((away - (BEND * 6.0).sqrt()).abs() < 0.05, "{away}");
        // Far to the side it hardly moves, and never towards the hole.
        let side = bent(at(260.0, 100.0, 36.0), Some(&hole)).expect("seen");
        assert!(side.x >= 260.0 && side.x < 266.0, "{side:?}");
        assert_eq!(
            bent(at(110.0, 100.0, 60.0), None),
            Some(at(110.0, 100.0, 60.0))
        );
    }

    #[test]
    fn focus_dims_the_other_communities_and_selection_draws_lines() {
        let graph = Graph::parse(&sample(4, 40)).expect("graph");
        let mut space = Space::new(&graph);
        let light = |data: &[u8]| -> u64 { data.chunks_exact(4).map(|px| u64::from(px[1])).sum() };
        let plain = light(&render(&mut space, &graph, &view()));
        let focused = light(&render(
            &mut space,
            &graph,
            &SpaceView {
                focus: Some(0),
                focus_mix: 1.0,
                ..view()
            },
        ));
        assert!(focused < plain, "{focused} < {plain}");
        let hub = graph.clusters[0].members[0];
        let chosen = light(&render(
            &mut space,
            &graph,
            &SpaceView {
                selected: Some(hub),
                ..view()
            },
        ));
        assert!(chosen > plain, "{chosen} > {plain}");
    }

    #[test]
    fn nothing_shows_before_the_picture_arrives() {
        let graph = Graph::parse(&sample(3, 30)).expect("graph");
        let mut space = Space::new(&graph);
        let data = render(
            &mut space,
            &graph,
            &SpaceView {
                birth: 0.0,
                ..view()
            },
        );
        assert!(data.chunks_exact(4).all(|px| px[0] < 60));
    }
}
