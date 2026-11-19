//! The black hole in the middle of the galaxy picture.
//!
//! It is shaded pixel by pixel, in units of the shadow's radius, into a small picture of its own that holds
//! more light than a screen can show, and that picture is then given a camera's bloom: its light spreads,
//! veils the shadow in a warm haze and burns out to white where the gas is hottest.
//!
//! A flat disc of gas turns around the hole. It is drawn from a pattern of thousands of thin strands laid
//! along the flow, the trails of grains of gas, which is what makes it look like matter rather than a
//! gradient. The eye's rays are bent on their way past the hole: each one runs straight to the point where it
//! comes nearest and leaves that point turned towards the hole by `BEND / distance` radians. That one bend
//! is a rough stand-in for the real curved path, and it is enough to lift the far half of the disc into
//! view over the hole, whichever side the camera looks from. Light that went all the way around the hole
//! is not traced: it is drawn as what it adds up to, a band of thin rings that hugs the shadow.

use crate::math::{clamp01, falloff, fast_atan2, lerp, smoothstep, Noise, Rng, Vec3, TAU};
use crate::raster::{Frame, Rgb};

/// Where the gas begins and ends, in shadow radii.
const DISC_IN: f32 = 1.08;
const DISC_OUT: f32 = 4.15;
/// How far from the middle the hole's own picture reaches: the gas and the room its light spreads into.
const REACH: f32 = 6.2;
/// The bend of a ray that passes at distance `b` is `BEND / b`.
pub const BEND: f32 = 0.35;
/// Rays that pass further out than this are not bent.
const BEND_REACH: f32 = 6.0;
const BEND_STEPS: usize = 512;
const LANE_STEPS: usize = 160;
/// The gas flows faster near the hole, which would wind the strands ever tighter; two copies of the pattern
/// start over in turn, half a period apart, and are cross-faded so no restart shows.
const FLOW_PERIOD: f32 = 50.0;
/// How bright the gas is by distance from the hole (before the eye's bloom), measured off the look aimed for.
const HEAT: [(f32, f32); 10] = [
    (1.08, 0.74),
    (1.3, 0.68),
    (1.6, 0.54),
    (1.9, 0.44),
    (2.5, 0.3),
    (3.0, 0.18),
    (3.4, 0.115),
    (3.7, 0.07),
    (3.95, 0.035),
    (4.15, 0.0),
];
/// Light up to this strength is shown as it is; above it, it is eased towards the screen's full strength.
const SHOULDER: f32 = 0.6;
/// How wide the band of rings around the shadow is.
const BAND: f32 = 0.44;
/// How much of the light each width of the bloom adds, narrowest first.
const BLOOM: [f32; 5] = [0.16, 0.2, 0.22, 0.24, 0.24];

/// A pattern drawn once: across the disc (first index) by around it (second, wrapping).
struct Pattern {
    across: usize,
    around: usize,
    cells: Vec<f32>,
}

impl Pattern {
    /// `across` in `0..=1`; `turn` in turns, any number (it wraps).
    fn at(&self, across: f32, turn: f32) -> f32 {
        let x = across * (self.across - 1) as f32;
        let i = (x as usize).min(self.across - 2);
        let tx = x - i as f32;
        let y = (turn - turn.floor()) * self.around as f32;
        let j = (y as usize).min(self.around - 1);
        let ty = y - j as f32;
        let k = if j + 1 == self.around { 0 } else { j + 1 };
        let row = i * self.around;
        let next = row + self.around;
        lerp(
            lerp(self.cells[row + j], self.cells[next + j], tx),
            lerp(self.cells[row + k], self.cells[next + k], tx),
            ty,
        )
    }

    /// The same pattern with half as many steps across the disc: each the mean of two.
    fn halved(&self) -> Pattern {
        let across = self.across / 2;
        let mut cells = Vec::with_capacity(across * self.around);
        for i in 0..across {
            let (upper, lower) = (2 * i * self.around, (2 * i + 1) * self.around);
            for j in 0..self.around {
                cells.push(0.5 * (self.cells[upper + j] + self.cells[lower + j]));
            }
        }
        Pattern {
            across,
            around: self.around,
            cells,
        }
    }
}

/// A pattern at every fineness, the finest first: each has half as many steps across the disc as the one
/// before. A frame is drawn from the one whose steps are about a pixel wide, or strands thinner than a
/// pixel would flicker.
struct Mips {
    levels: Vec<Pattern>,
}

impl Mips {
    fn new(finest: Pattern) -> Self {
        let mut levels = vec![finest];
        while levels[levels.len() - 1].across >= 8 {
            let next = levels[levels.len() - 1].halved();
            levels.push(next);
        }
        Self { levels }
    }

    /// Writes the pattern as it is at one moment into `out`, at fineness `level` (0 the finest; in between
    /// two levels it is a mix of them). Each lane across the disc has turned by its own speed: `turned`
    /// gives the turns of a lane, by its place across the disc, for the two copies of the flow, and
    /// `weight` is the share of the first copy.
    fn bake(&self, level: f32, turned: impl Fn(f32) -> (f32, f32), weight: f32, out: &mut Pattern) {
        let last = self.levels.len() - 1;
        let level = level.clamp(0.0, last as f32);
        let fine = &self.levels[level as usize];
        let coarse = &self.levels[(level as usize + 1).min(last)];
        let mix = level - level.floor();
        let (rows, cols) = (fine.across, fine.around);
        out.across = rows;
        out.around = cols;
        out.cells.clear();
        out.cells.resize(rows * cols, 0.0);
        for i in 0..rows {
            let (first, second) = turned(i as f32 / (rows - 1) as f32);
            let fine_row = &fine.cells[i * cols..(i + 1) * cols];
            let coarse_at = (i / 2).min(coarse.across - 1) * cols;
            let coarse_row = &coarse.cells[coarse_at..coarse_at + cols];
            let into = &mut out.cells[i * cols..(i + 1) * cols];
            for (turn, share) in [(first, weight), (second, 1.0 - weight)] {
                if share <= 0.005 {
                    continue;
                }
                let shift = (turn - turn.floor()) * cols as f32;
                let whole = shift as usize;
                let part = shift - whole as f32;
                for (j, cell) in into.iter_mut().enumerate() {
                    let a = (j + whole) % cols;
                    let b = (a + 1) % cols;
                    let sharp = lerp(fine_row[a], fine_row[b], part);
                    let blurred = lerp(coarse_row[a], coarse_row[b], part);
                    *cell += share * lerp(sharp, blurred, mix);
                }
            }
        }
    }
}

/// How the strands of a pattern are laid.
struct Weave {
    across: usize,
    around: usize,
    strands: usize,
    /// Shortest and longest strand, in turns.
    long: (f32, f32),
    /// Above 1 crowds the strands towards the hole.
    crowd: f32,
    /// One strand in this many is a broad streak, several steps wide, that still shows when the disc is
    /// drawn small (0: none).
    broad: usize,
    /// How far a strand strays from its circle, in disc widths, and how many times a turn it wavers.
    stray: f32,
    waves: (f32, f32),
    /// How far the whole flow is pushed off its circles, in disc widths.
    warp: f32,
    /// How unevenly the light is spread in patches (0: evenly).
    patchy: f32,
    /// Light everywhere under the strands, so the gaps between them are dim rather than black.
    floor: f32,
    seed: u64,
}

/// Lays thin strands along the flow, each the trail of a grain of gas: an arc that drifts inwards a little,
/// wavers, and fades at both ends. Where they crowd they merge into a glow; where they are few, each one
/// shows. The result is evened out so that its mean is about one at every distance from the hole.
fn weave(plan: &Weave) -> Pattern {
    let (rows, cols) = (plan.across, plan.around);
    let mut rng = Rng::new(plan.seed);
    let noise = Noise::new(64, plan.seed ^ 0x5151);
    let mut cells = vec![0.0_f32; rows * cols];
    for strand in 0..plan.strands {
        let wide = if plan.broad > 0 && strand % plan.broad == 0 {
            rng.range(1.6, 4.2)
        } else {
            0.0
        };
        let across = rng.unit().powf(plan.crowd);
        let long = plan.long.0 + (plan.long.1 - plan.long.0) * rng.unit().powi(2);
        let start = rng.unit();
        // Short strands are faint hairs, long ones carry the light.
        let weight =
            (0.25 + 1.5 * rng.unit().powi(3)) * (0.35 + 0.65 * (long / plan.long.1).sqrt());
        let drift = (rng.unit() - 0.35) * 0.12 * long.sqrt();
        let stray = plan.stray * rng.unit();
        let waves = rng.range(plan.waves.0, plan.waves.1);
        let phase = rng.range(0.0, TAU);
        let steps = ((long * cols as f32) as usize).max(2);
        for k in 0..steps {
            let t = k as f32 / (steps - 1) as f32;
            let turn = start + t * long;
            let push = plan.warp * (noise.at_wrapped(across * 3.0 + 50.0, turn * 9.0, 9) - 0.5);
            let at = across + drift * t + stray * (TAU * waves * turn + phase).sin() + push;
            let row = at * (rows - 1) as f32;
            if row < 0.0 || row >= (rows - 1) as f32 {
                continue;
            }
            let col = ((turn - turn.floor()) * cols as f32) as usize % cols;
            let light = weight * (std::f32::consts::PI * t).sin().max(0.0).sqrt();
            if wide > 0.0 {
                // A streak with a soft edge, a few steps wide.
                let reach = (wide * 1.6) as usize + 1;
                let middle = row.round() as usize;
                for k in middle.saturating_sub(reach)..=(middle + reach).min(rows - 1) {
                    let away = (k as f32 - row) / wide;
                    cells[k * cols + col] += light * 0.75 * falloff(away * away);
                }
                continue;
            }
            let low = row as usize;
            let share = row - low as f32;
            cells[low * cols + col] += light * (1.0 - share);
            cells[(low + 1) * cols + col] += light * share;
        }
    }
    // Even out by distance: the mean of each row and its neighbours becomes one.
    let means: Vec<f32> = (0..rows)
        .map(|row| cells[row * cols..(row + 1) * cols].iter().sum::<f32>() / cols as f32)
        .collect();
    let span = (rows / 24).max(2);
    for row in 0..rows {
        let from = row.saturating_sub(span);
        let to = (row + span + 1).min(rows);
        let mean = means[from..to].iter().sum::<f32>() / (to - from) as f32;
        let scale = 1.0 / mean.max(0.02);
        let across = row as f32 / (rows - 1) as f32;
        for col in 0..cols {
            let turn = col as f32 / cols as f32;
            let patch = noise.at_wrapped(across * 5.0, turn * 8.0, 8) * 0.65
                + noise.at_wrapped(across * 11.0 + 7.0, turn * 20.0, 20) * 0.35;
            let patches = lerp(1.0, 0.1 + 1.8 * patch * patch * 2.0, plan.patchy);
            let cell = &mut cells[row * cols + col];
            *cell = ((plan.floor + (1.0 - plan.floor) * *cell * scale) * patches).min(4.0);
        }
    }
    Pattern {
        across: rows,
        around: cols,
        cells,
    }
}

/// One frame's view of the hole.
#[derive(Debug, Clone, Copy)]
pub struct HoleView {
    /// The middle, in pixels.
    pub x: f32,
    pub y: f32,
    /// The shadow's radius, in pixels.
    pub radius: f32,
    /// The disc's axis as the camera sees it: x to the right, y up, z away from the eye.
    pub normal: Vec3,
    pub time: f32,
    /// 0 to 1: fades the whole thing in.
    pub gain: f32,
}

/// What the shading reads; the pictures it writes are kept apart so several threads can share this.
struct Gas {
    /// Thin strands of gas, for the disc seen plainly.
    strands: Mips,
    /// A few long thin rings: for the band around the shadow and the far side seen around the hole, where
    /// the whole disc is squeezed into a narrow strip.
    rings: Mips,
    /// Sine and cosine of the bend, by how near the ray passes (`0..BEND_REACH`).
    bend: Vec<(f32, f32)>,
    /// By place across the disc (`0..1`): the twist of the spiral there, how fast the gas turns, its heat.
    lanes: Vec<(f32, f32, f32)>,
}

pub struct Hole {
    gas: Gas,
    /// The two patterns as they are in the frame being drawn.
    plain: Pattern,
    band: Pattern,
    /// The hole's own picture: light (red, green, blue) and how much of what is behind shows through.
    light: Vec<f32>,
    keep: Vec<f32>,
    /// That picture ready for the screen (red, green, blue, and how much of the frame stays, a byte each),
    /// where it lies on the frame, and the view it was made for. The hole changes slowly: while it stays
    /// where it is, every other frame lays the last picture down again instead of shading a new one.
    layer: Vec<u8>,
    layer_box: (usize, usize, usize, usize),
    layer_view: Option<(f32, f32, f32, usize, usize)>,
    rested: u32,
}

fn heat(radius: f32) -> f32 {
    for pair in HEAT.windows(2) {
        let ((from, a), (to, b)) = (pair[0], pair[1]);
        if radius <= to {
            return lerp(a, b, clamp01((radius - from) / (to - from)));
        }
    }
    0.0
}

fn ramp(across: f32) -> Rgb {
    const STOPS: [(f32, Rgb); 4] = [
        (0.0, [1.0, 0.94, 0.83]),
        (0.3, [1.0, 0.83, 0.64]),
        (0.62, [1.0, 0.74, 0.53]),
        (1.0, [1.0, 0.7, 0.5]),
    ];
    for pair in STOPS.windows(2) {
        let (from, a) = pair[0];
        let (to, b) = pair[1];
        if across <= to {
            let t = clamp01((across - from) / (to - from));
            return [
                lerp(a[0], b[0], t),
                lerp(a[1], b[1], t),
                lerp(a[2], b[2], t),
            ];
        }
    }
    STOPS[3].1
}

struct Shade<'a> {
    gas: &'a Gas,
    plain: &'a Pattern,
    band: &'a Pattern,
    normal: Vec3,
    e1: Vec3,
    e2: Vec3,
    /// Seen edge-on the eye looks through more of the gas: it is brighter.
    depth: f32,
    /// Seen edge-on, the far side lifted over the hole gives way to the band of rings around the shadow.
    lifted: f32,
}

impl Shade<'_> {
    /// The light of the disc at `point` (`radius` from the hole): its colour times its strength, and how much
    /// it hides what is behind it. `squeeze` is 0 for the disc seen plainly and 1 for the far side seen
    /// around the hole.
    fn emit(&self, point: Vec3, radius: f32, squeeze: f32) -> (Rgb, f32) {
        let across = (radius - DISC_IN) / (DISC_OUT - DISC_IN);
        if !(0.0..=1.0).contains(&across) {
            return ([0.0; 3], 0.0);
        }
        let at = across * (LANE_STEPS - 1) as f32;
        let i = (at as usize).min(LANE_STEPS - 2);
        let twist = lerp(self.gas.lanes[i].0, self.gas.lanes[i + 1].0, at - i as f32);
        let angle = fast_atan2(point.dot(self.e2), point.dot(self.e1));
        let turn = (angle + twist) / TAU;
        // The rim is ragged: how far out the gas reaches wavers around the disc, and turns with it.
        let reach = if across > 0.4 {
            let rag = self.band.at(0.3, turn) - 0.9;
            (across + 0.11 * rag * smoothstep(0.4, 1.0, across)).clamp(0.0, 1.0)
        } else {
            across
        };
        let at = reach * (LANE_STEPS - 1) as f32;
        let i = (at as usize).min(LANE_STEPS - 2);
        let body = lerp(self.gas.lanes[i].2, self.gas.lanes[i + 1].2, at - i as f32);
        let strands = if squeeze <= 0.01 {
            self.plain.at(across, turn)
        } else if squeeze >= 0.99 {
            self.band.at(across, turn)
        } else {
            lerp(
                self.plain.at(across, turn),
                self.band.at(across, turn),
                squeeze,
            )
        };
        // Gas coming towards the eye looks brighter than gas going away.
        let swing = self.normal.cross(point).z / radius;
        let beam = (1.0 + 0.2 * swing).powi(3);
        let strength = body * strands * beam * lerp(self.depth, self.lifted, squeeze);
        let colour = ramp(across);
        (
            [
                colour[0] * strength,
                colour[1] * strength,
                colour[2] * strength,
            ],
            clamp01(body * 2.2) * smoothstep(0.0, 0.02, across),
        )
    }
}

impl Default for Hole {
    fn default() -> Self {
        Self::new()
    }
}

impl Hole {
    #[must_use]
    pub fn new() -> Self {
        let fine = weave(&Weave {
            across: 256,
            around: 1024,
            strands: 26000,
            long: (0.008, 0.1),
            crowd: 1.25,
            broad: 9,
            stray: 0.02,
            waves: (12.0, 44.0),
            warp: 0.07,
            patchy: 0.9,
            floor: 0.15,
            seed: 0x00B1_ACC0,
        });
        let rings = weave(&Weave {
            across: 96,
            around: 1024,
            strands: 90,
            long: (0.2, 0.95),
            crowd: 1.0,
            broad: 0,
            stray: 0.004,
            waves: (3.0, 9.0),
            warp: 0.0,
            patchy: 0.3,
            floor: 0.4,
            seed: 0x0000_0D15,
        });
        let bend = (0..=BEND_STEPS)
            .map(|i| {
                let near = (i as f32 * BEND_REACH / BEND_STEPS as f32).max(0.5);
                // It fades to nothing at BEND_REACH, so the picture has no seam there.
                (BEND / near * (1.0 - smoothstep(4.4, BEND_REACH, near))).sin_cos()
            })
            .collect();
        let lanes = (0..LANE_STEPS)
            .map(|i| {
                let radius = DISC_IN + (DISC_OUT - DISC_IN) * i as f32 / (LANE_STEPS - 1) as f32;
                // Turns a second: Kepler's law, inner lanes faster.
                (
                    2.2 * radius.ln(),
                    -0.022 * (DISC_IN / radius).powf(1.5),
                    heat(radius),
                )
            })
            .collect();
        let empty = || Pattern {
            across: 2,
            around: 2,
            cells: vec![1.0; 4],
        };
        Self {
            gas: Gas {
                strands: Mips::new(fine),
                rings: Mips::new(rings),
                bend,
                lanes,
            },
            plain: empty(),
            band: empty(),
            light: Vec::new(),
            keep: Vec::new(),
            layer: Vec::new(),
            layer_box: (0, 0, 0, 0),
            layer_view: None,
            rested: 0,
        }
    }

    /// How far from the middle the hole's light reaches, in shadow radii.
    #[must_use]
    pub fn reach() -> f32 {
        REACH
    }

    /// Draws the hole over what is in the frame: the shadow and the gas hide what lies behind them.
    #[allow(clippy::too_many_lines)]
    pub fn draw(&mut self, frame: &mut Frame, view: &HoleView) {
        let scale = view.radius;
        let gain = clamp01(view.gain);
        if scale < 0.5 || gain <= 0.0 || !view.x.is_finite() || !view.y.is_finite() {
            return;
        }
        let mut normal = view.normal.unit();
        if normal == Vec3::ZERO {
            return;
        }
        // Exactly edge-on the flat disc would vanish; keep a sliver of tilt.
        if normal.z.abs() < 0.02 {
            normal.z = if normal.z < 0.0 { -0.02 } else { 0.02 };
            normal = normal.unit();
        }
        let width = frame.width();
        let height = frame.height();
        let reach = REACH * scale;
        if width == 0
            || height == 0
            || view.x + reach < 0.0
            || view.y + reach < 0.0
            || view.x - reach >= width as f32
            || view.y - reach >= height as f32
        {
            return;
        }
        let x0 = (view.x - reach).floor().max(0.0) as usize;
        let y0 = (view.y - reach).floor().max(0.0) as usize;
        let x1 = ((view.x + reach).ceil().max(0.0) as usize).min(width - 1);
        let y1 = ((view.y + reach).ceil().max(0.0) as usize).min(height - 1);
        let (box_w, box_h) = (x1 + 1 - x0, y1 + 1 - y0);
        let same = self.layer_view.is_some_and(|(x, y, radius, w, h)| {
            (x - view.x).abs() < 0.25
                && (y - view.y).abs() < 0.25
                && (radius / scale - 1.0).abs() < 0.003
                && (w, h) == (width, height)
        });
        // A large picture costs more to shade: it is kept a frame longer.
        let rests = if box_w * box_h > 220_000 { 2 } else { 1 };
        if same && gain >= 1.0 && self.rested < rests {
            self.rested += 1;
            self.lay(frame);
            return;
        }
        self.rested = 0;
        self.layer_view = (gain >= 1.0).then_some((view.x, view.y, scale, width, height));
        self.layer_box = (x0, y0, box_w, box_h);

        let helper = if normal.z.abs() < 0.9 {
            Vec3::new(0.0, 0.0, 1.0)
        } else {
            Vec3::new(1.0, 0.0, 0.0)
        };
        let e1 = normal.cross(helper).unit();
        let phase = (view.time / FLOW_PERIOD).rem_euclid(1.0);
        let flat = normal.x.hypot(normal.y).max(1e-4);
        let edge_on = (1.0 - normal.z.abs()).powi(2);
        // The patterns as they are now, at the fineness this frame can show: a step across the disc about a
        // pixel wide. Seen at a low angle the disc is squeezed, so its steps must be wider.
        let (time1, time2) = (
            phase * FLOW_PERIOD,
            (phase + 0.5).rem_euclid(1.0) * FLOW_PERIOD,
        );
        let weight = 1.0 - (2.0 * phase - 1.0).abs();
        let lanes = &self.gas.lanes;
        let turned = |across: f32| -> (f32, f32) {
            let at = across * (LANE_STEPS - 1) as f32;
            let i = (at as usize).min(LANE_STEPS - 2);
            let speed = lerp(lanes[i].1, lanes[i + 1].1, at - i as f32);
            (-speed * time1, -speed * time2 + 0.37)
        };
        let wide = (DISC_OUT - DISC_IN) * scale;
        let squeezed = normal.z.abs().max(0.02).sqrt();
        let steps = self.gas.strands.levels[0].across as f32;
        self.gas.strands.bake(
            (steps / (wide * squeezed)).max(1.0).log2() - 0.9,
            turned,
            weight,
            &mut self.plain,
        );
        let steps = self.gas.rings.levels[0].across as f32;
        self.gas.rings.bake(
            (steps / (BAND * scale)).max(1.0).log2() - 0.4,
            turned,
            weight,
            &mut self.band,
        );
        let light = &mut self.light;
        let keep = &mut self.keep;
        light.clear();
        light.resize(box_w * box_h * 3, 0.0);
        keep.clear();
        keep.resize(box_w * box_h, 1.0);
        let pass = Pass {
            shade: Shade {
                gas: &self.gas,
                plain: &self.plain,
                band: &self.band,
                normal,
                e1,
                e2: normal.cross(e1),
                depth: 1.0 + 0.3 * (1.0 - smoothstep(0.06, 0.36, normal.z.abs())),
                lifted: 0.12 + 0.88 * smoothstep(0.14, 0.4, normal.z.abs()),
            },
            x: view.x - x0 as f32,
            y: view.y - y0 as f32,
            scale,
            flat,
            bar: edge_on * smoothstep(0.34, 0.1, normal.z.abs()),
            lean: normal.z.abs(),
            near_side: if normal.z < 0.0 { -1.0 } else { 1.0 },
            turn: view.time * 0.05,
            width: box_w,
        };
        // The shading is the heavy part of a frame and every row stands alone: share the rows between a few
        // threads when there are enough of them to be worth it.
        let helpers = std::thread::available_parallelism()
            .map_or(1, |n| n.get().saturating_sub(1).clamp(1, 4));
        if helpers < 2 || box_w * box_h < 30_000 {
            pass.rows(light, keep, 0);
        } else {
            let band = box_h.div_ceil(helpers);
            std::thread::scope(|scope| {
                let lights = light.chunks_mut(band * box_w * 3);
                let keeps = keep.chunks_mut(band * box_w);
                for (i, (light, keep)) in lights.zip(keeps).enumerate() {
                    let pass = &pass;
                    scope.spawn(move || pass.rows(light, keep, i * band));
                }
            });
        }

        // The eye's bloom: the light spread at four widths, the widest about a shadow's radius.
        let step = ((scale * 0.085).round() as usize).clamp(1, 16);
        let (mut glow, glow_w, glow_h) = shrink(light, box_w, box_h, step);
        blur(&mut glow, glow_w, glow_h);
        let mut levels = vec![(glow, glow_w, glow_h)];
        while levels.len() < BLOOM.len() {
            let (last, w, h) = &levels[levels.len() - 1];
            if *w < 6 || *h < 6 {
                break;
            }
            let (mut next, next_w, next_h) = shrink(last, *w, *h, 2);
            blur(&mut next, next_w, next_h);
            levels.push((next, next_w, next_h));
        }
        while levels.len() > 1 {
            let Some((wide, wide_w, wide_h)) = levels.pop() else {
                break;
            };
            let weight = BLOOM[levels.len()] / BLOOM[levels.len() - 1];
            let last = levels.len() - 1;
            let (into, w, h) = &mut levels[last];
            grow_add(into, *w, *h, &wide, wide_w, wide_h, weight);
        }
        let (glow, glow_w, glow_h) = (&levels[0].0, levels[0].1, levels[0].2);

        // The picture for the screen: the light and its bloom, burnt towards white where it is too strong.
        let layer = &mut self.layer;
        layer.clear();
        layer.resize(box_w * box_h * 4, 0);
        let light = &self.light;
        let keep = &self.keep;
        let to_small = 1.0 / step as f32;
        let finish = |rows: &mut [u8], first: usize| {
            for (line, row) in rows.chunks_exact_mut(box_w * 4).enumerate() {
                let by = first + line;
                let gy = ((by as f32 + 0.5) * to_small - 0.5).clamp(0.0, (glow_h - 1) as f32);
                let gy0 = gy as usize;
                let gy1 = (gy0 + 1).min(glow_h - 1);
                let ty = gy - gy0 as f32;
                for bx in 0..box_w {
                    let gx = ((bx as f32 + 0.5) * to_small - 0.5).clamp(0.0, (glow_w - 1) as f32);
                    let gx0 = gx as usize;
                    let gx1 = (gx0 + 1).min(glow_w - 1);
                    let tx = gx - gx0 as f32;
                    let i = (by * box_w + bx) * 3;
                    let mut rgb = [0.0_f32; 3];
                    for (c, value) in rgb.iter_mut().enumerate() {
                        let a = lerp(
                            glow[(gy0 * glow_w + gx0) * 3 + c],
                            glow[(gy0 * glow_w + gx1) * 3 + c],
                            tx,
                        );
                        let b = lerp(
                            glow[(gy1 * glow_w + gx0) * 3 + c],
                            glow[(gy1 * glow_w + gx1) * 3 + c],
                            tx,
                        );
                        *value = light[i + c] + BLOOM[0] * lerp(a, b, ty);
                    }
                    // Light too strong for the screen: bright parts are eased towards full strength, as film
                    // does, so they keep their grain, and only the very hottest burn out to white.
                    let over = (rgb[0].max(rgb[1]).max(rgb[2]) - 1.5).max(0.0) * 0.5;
                    let px = &mut row[bx * 4..bx * 4 + 4];
                    for c in 0..3 {
                        let light = rgb[c] + over;
                        let shown = if light > SHOULDER {
                            SHOULDER + (1.0 - SHOULDER) * (1.0 - falloff((light - SHOULDER) / 0.6))
                        } else {
                            light
                        };
                        px[c] = (shown * gain * 255.0 + 0.5).min(255.0) as u8;
                    }
                    px[3] = ((1.0 - (1.0 - keep[by * box_w + bx]) * gain) * 255.0 + 0.5) as u8;
                }
            }
        };
        if helpers < 2 || box_w * box_h < 30_000 {
            finish(layer, 0);
        } else {
            let band = box_h.div_ceil(helpers);
            std::thread::scope(|scope| {
                for (i, rows) in layer.chunks_mut(band * box_w * 4).enumerate() {
                    let finish = &finish;
                    scope.spawn(move || finish(rows, i * band));
                }
            });
        }
        self.lay(frame);
    }

    /// Lays the hole's picture over the frame.
    fn lay(&self, frame: &mut Frame) {
        let (x0, y0, box_w, box_h) = self.layer_box;
        let width = frame.width();
        if x0 + box_w > width || y0 + box_h > frame.height() || self.layer.len() < box_w * box_h * 4
        {
            return;
        }
        let data = frame.bytes();
        for (by, row) in self.layer.chunks_exact(box_w * 4).enumerate().take(box_h) {
            let start = ((y0 + by) * width + x0) * 4;
            let into = &mut data[start..start + box_w * 4];
            for (px, layer) in into.chunks_exact_mut(4).zip(row.chunks_exact(4)) {
                if layer[3] == 255 && layer[0] == 0 {
                    continue;
                }
                let stay = u32::from(layer[3]);
                for c in 0..3 {
                    px[c] = ((u32::from(px[c]) * stay + 127) / 255 + u32::from(layer[c])).min(255)
                        as u8;
                }
            }
        }
    }
}

/// Everything one frame's shading needs, shared by the threads that do it.
struct Pass<'a> {
    shade: Shade<'a>,
    /// The hole's middle and the shadow's radius, in pixels of the hole's own picture.
    x: f32,
    y: f32,
    scale: f32,
    /// Length of the disc's axis as it lies on the picture.
    flat: f32,
    /// Strength of the bright bar of an edge-on disc, along the line where the disc cuts the picture.
    bar: f32,
    /// 0 seen edge-on, 1 seen from straight above.
    lean: f32,
    /// On which side of the picture the near half of the disc lies, along the disc's axis.
    near_side: f32,
    /// Turns the dashes of the thin rings.
    turn: f32,
    width: usize,
}

impl Pass<'_> {
    /// Shades whole rows of the hole's picture, starting at row `first`.
    #[allow(clippy::too_many_lines)]
    fn rows(&self, light: &mut [f32], keep: &mut [f32], first: usize) {
        let normal = self.shade.normal;
        let (scale, flat) = (self.scale, self.flat);
        let (axis_x, axis_y) = (normal.x / flat, normal.y / flat);
        let gas = (DISC_OUT + 0.35) * (DISC_OUT + 0.35);
        let far = REACH * REACH;
        let bend = &self.shade.gas.bend;
        let rows = light
            .chunks_exact_mut(self.width * 3)
            .zip(keep.chunks_exact_mut(self.width));
        for (line, (light, keep)) in rows.enumerate() {
            let v = (self.y - ((first + line) as f32 + 0.5)) / scale;
            for px in 0..self.width {
                let u = (px as f32 + 0.5 - self.x) / scale;
                let near2 = u * u + v * v;
                if near2 >= far {
                    continue;
                }
                // Height over the disc's plane at the point nearest the hole.
                let lift = normal.x * u + normal.y * v;
                let across = lift / flat;
                let along = (normal.y * u - normal.x * v) / flat;
                let mut flare = 0.0;
                if self.bar > 0.004 && across.abs() < 0.5 && along.abs() < 5.5 {
                    // An edge-on disc is thinner than a pixel: its light is put back as a bar, brighter on
                    // the side where the gas comes towards the eye.
                    let side = 1.0 - 0.45 * (along / (0.7 + along.abs()));
                    flare = self.bar
                        * (3.2
                            * falloff((across / 0.06).powi(2))
                            * falloff(((along + 0.7) / 0.8).powi(2))
                            + 1.0
                                * falloff((across / 0.035).powi(2))
                                * falloff((along / 3.0).powi(2)))
                        * side;
                }
                let out = &mut light[px * 3..px * 3 + 3];
                if near2 >= gas {
                    out[0] = flare;
                    out[1] = flare * 0.86;
                    out[2] = flare * 0.66;
                    continue;
                }
                let near = near2.sqrt();

                // In front of the hole the ray is still straight.
                let mut front = ([0.0_f32; 3], 0.0_f32);
                let before = -lift / normal.z;
                if before < 0.0 {
                    let radius = (near2 + before * before).sqrt();
                    if radius <= DISC_OUT {
                        front = self.shade.emit(Vec3::new(u, v, before), radius, 0.0);
                    }
                }
                // Behind it the ray has been bent towards the hole. Rays that pass inside the shadow never
                // come out, and what the near half of the disc covers need not be worked out.
                let mut back = ([0.0_f32; 3], 0.0_f32);
                if near > 0.98 && front.1 < 0.97 {
                    let at = near * (BEND_STEPS as f32 / BEND_REACH);
                    let i = (at as usize).min(BEND_STEPS - 1);
                    let t = at - i as f32;
                    let (sin, cos) = (
                        lerp(bend[i].0, bend[i + 1].0, t),
                        lerp(bend[i].1, bend[i + 1].1, t),
                    );
                    let slope = normal.z * cos - sin * lift / near;
                    if slope.abs() > 1e-5 {
                        let after = -lift / slope;
                        if after > 0.0 {
                            let pull = after * sin / near;
                            let point = Vec3::new(u - pull * u, v - pull * v, after * cos);
                            let radius = point.len();
                            if radius <= DISC_OUT {
                                // Where the bend has carried the ray far, the disc is squeezed into rings.
                                // At the seam with the near half (`pull` zero) it is the same pattern as
                                // there.
                                back = self.shade.emit(point, radius, smoothstep(0.05, 0.5, pull));
                            }
                        }
                    }
                }

                let shadow = clamp01((1.0 - near) * scale + 0.5);
                let open = 1.0 - shadow;
                // Thin rings of light that went all the way around: one on the rim of the shadow, a dotted
                // one inside it, and a wider one that slides towards the near side as the disc tips up.
                let thin = 0.012 + 0.55 / scale;
                let slide = 0.72 * self.lean * self.near_side;
                let (du, dv) = (u - axis_x * slide, v - axis_y * slide);
                let wide = (du * du + dv * dv).sqrt();
                let rim = (near - 1.01).abs() < thin * 3.0;
                let inner = (near - 0.85).abs() < thin * 3.0;
                let outer = (wide - 1.27).abs() < thin * 3.0;
                let place = (near - 1.0) / BAND;
                let hugs = (0.0..1.0).contains(&place);
                let angle = if rim || inner || outer || hugs {
                    fast_atan2(v, u) / TAU
                } else {
                    0.0
                };
                let dashes = |count: f32, turn: f32| -> f32 {
                    smoothstep(-0.2, 0.35, (TAU * (angle * count + turn)).sin())
                };
                let mut ring = 0.0;
                if rim {
                    ring += 0.75
                        * falloff(((near - 1.01) / thin).powi(2))
                        * (0.75 + 0.25 * dashes(31.0, self.turn));
                }
                if inner {
                    ring += 0.2
                        * falloff(((near - 0.85) / thin).powi(2))
                        * dashes(17.0, -self.turn * 1.4);
                }
                if outer {
                    ring += 0.26
                        * falloff(((wide - 1.27) / thin).powi(2))
                        * (0.6 + 0.4 * dashes(23.0, self.turn * 0.7));
                }
                // The band that hugs the shadow: light from all of the disc that went around the hole,
                // squeezed into thin rings, brighter where the gas comes towards the eye.
                let mut band = 0.0;
                if hugs {
                    let lines = self.shade.band.at(place, angle);
                    // About the power 0.8, without the cost of a power.
                    let fall = (1.0 - place).sqrt()
                        * (1.0 - place).sqrt().sqrt()
                        * smoothstep(0.0, 0.05, place);
                    band = 1.75 * fall * lines * (1.0 - 0.45 * along / near);
                }

                let through = 1.0 - front.1;
                let hugging = through * open * (1.0 - back.1);
                // The shadow is not black to an eye dazzled by the gas around it: a warm haze lies over it,
                // thicker towards the bright side.
                let veil = through * shadow * (0.13 - 0.05 * along);
                out[0] =
                    front.0[0] + through * back.0[0] * open + hugging * band + ring + flare + veil;
                out[1] = front.0[1]
                    + through * back.0[1] * open
                    + hugging * band * 0.92
                    + ring * 0.93
                    + flare * 0.86
                    + veil * 0.8;
                out[2] = front.0[2]
                    + through * back.0[2] * open
                    + hugging * band * 0.78
                    + ring * 0.82
                    + flare * 0.66
                    + veil * 0.58;
                keep[px] = through * (1.0 - back.1 * open) * open;
            }
        }
    }
}

/// A picture `by` times smaller, each pixel the mean of what it covers (red, green, blue per pixel).
fn shrink(src: &[f32], width: usize, height: usize, by: usize) -> (Vec<f32>, usize, usize) {
    let (w, h) = (width.div_ceil(by), height.div_ceil(by));
    let mut out = vec![0.0_f32; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let mut sum = [0.0_f32; 3];
            let mut count = 0.0;
            for sy in y * by..((y + 1) * by).min(height) {
                let row = sy * width;
                for sx in x * by..((x + 1) * by).min(width) {
                    let i = (row + sx) * 3;
                    sum[0] += src[i];
                    sum[1] += src[i + 1];
                    sum[2] += src[i + 2];
                    count += 1.0;
                }
            }
            let o = (y * w + x) * 3;
            out[o] = sum[0] / count;
            out[o + 1] = sum[1] / count;
            out[o + 2] = sum[2] / count;
        }
    }
    (out, w, h)
}

/// A soft blur, two pixels wide, along the rows and then down the columns.
fn blur(data: &mut [f32], width: usize, height: usize) {
    const TAPS: [f32; 5] = [0.0625, 0.25, 0.375, 0.25, 0.0625];
    let mut line = vec![0.0_f32; width.max(height) * 3];
    for y in 0..height {
        let row = y * width * 3;
        line[..width * 3].copy_from_slice(&data[row..row + width * 3]);
        for x in 0..width {
            for c in 0..3 {
                let mut sum = 0.0;
                for (k, tap) in TAPS.iter().enumerate() {
                    let sx = (x + k).saturating_sub(2).min(width - 1);
                    sum += tap * line[sx * 3 + c];
                }
                data[row + x * 3 + c] = sum;
            }
        }
    }
    for x in 0..width {
        for y in 0..height {
            for c in 0..3 {
                line[y * 3 + c] = data[(y * width + x) * 3 + c];
            }
        }
        for y in 0..height {
            for c in 0..3 {
                let mut sum = 0.0;
                for (k, tap) in TAPS.iter().enumerate() {
                    let sy = (y + k).saturating_sub(2).min(height - 1);
                    sum += tap * line[sy * 3 + c];
                }
                data[(y * width + x) * 3 + c] = sum;
            }
        }
    }
}

/// Adds a picture half this one's size, stretched smoothly over it, `weight` times.
#[allow(clippy::too_many_arguments)]
fn grow_add(
    into: &mut [f32],
    width: usize,
    height: usize,
    small: &[f32],
    small_w: usize,
    small_h: usize,
    weight: f32,
) {
    for y in 0..height {
        let sy = ((y as f32 + 0.5) * 0.5 - 0.5).clamp(0.0, (small_h - 1) as f32);
        let y0 = sy as usize;
        let y1 = (y0 + 1).min(small_h - 1);
        let ty = sy - y0 as f32;
        for x in 0..width {
            let sx = ((x as f32 + 0.5) * 0.5 - 0.5).clamp(0.0, (small_w - 1) as f32);
            let x0 = sx as usize;
            let x1 = (x0 + 1).min(small_w - 1);
            let tx = sx - x0 as f32;
            for c in 0..3 {
                let a = lerp(
                    small[(y0 * small_w + x0) * 3 + c],
                    small[(y0 * small_w + x1) * 3 + c],
                    tx,
                );
                let b = lerp(
                    small[(y1 * small_w + x0) * 3 + c],
                    small[(y1 * small_w + x1) * 3 + c],
                    tx,
                );
                into[(y * width + x) * 3 + c] += weight * lerp(a, b, ty);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::Blend;

    fn render(normal: Vec3) -> Vec<u8> {
        let mut data = vec![0u8; 320 * 320 * 4];
        for px in data.chunks_exact_mut(4) {
            px.copy_from_slice(&[40, 40, 40, 255]);
        }
        let mut hole = Hole::new();
        let mut frame = Frame::new(&mut data, 320, 320, Blend::Add);
        hole.draw(
            &mut frame,
            &HoleView {
                x: 160.0,
                y: 160.0,
                radius: 22.0,
                normal,
                time: 3.0,
                gain: 1.0,
            },
        );
        data
    }

    fn at(data: &[u8], x: usize, y: usize) -> [u8; 3] {
        let i = (y * 320 + x) * 4;
        [data[i], data[i + 1], data[i + 2]]
    }

    fn brightest(data: &[u8], x: usize, rows: std::ops::Range<usize>) -> u8 {
        rows.map(|y| at(data, x, y)[0]).max().unwrap_or(0)
    }

    #[test]
    fn the_shadow_is_veiled_and_the_far_side_rings_it() {
        // Seen from a little above the disc.
        let pitch = 0.11_f32;
        let data = render(Vec3::new(0.0, pitch.cos(), -pitch.sin()));
        // Inside the shadow the grey behind is gone; what is left is the warm haze of the light around it.
        let inside = at(&data, 160, 160 - 9);
        assert!(inside[0] < 130 && inside[0] > inside[2], "{inside:?}");
        // The far side, lifted over the shadow and under it: much brighter than the shadow.
        assert!(brightest(&data, 160, 160 - 34..160 - 23) > 170);
        assert!(brightest(&data, 160, 160 + 23..160 + 32) > 150);
        // The bar of the edge-on disc runs out to the sides, burnt out to white near the hole.
        assert!(brightest(&data, 160 - 40, 156..164) > 240);
        assert!(brightest(&data, 160 + 70, 150..170) > 70);
        // Far from the hole the background is left alone.
        assert_eq!(at(&data, 4, 4), [40, 40, 40]);
    }

    #[test]
    fn seen_from_above_the_disc_is_wide_and_brighter_where_the_gas_comes_closer() {
        let pitch = 0.6_f32;
        let data = render(Vec3::new(0.0, pitch.cos(), -pitch.sin()));
        // Gas far out to the sides, and below the hole at about half that.
        assert!(brightest(&data, 160 - 70, 150..170) > 60);
        assert!(brightest(&data, 160 + 70, 150..170) > 40);
        assert!(at(&data, 160, 160 + 40)[0] > 50);
        let left: u32 = (150..170)
            .map(|y| u32::from(at(&data, 160 - 50, y)[0]))
            .sum();
        let right: u32 = (150..170)
            .map(|y| u32::from(at(&data, 160 + 50, y)[0]))
            .sum();
        assert!(left > right + 200, "{left} against {right}");
    }

    #[test]
    fn the_patterns_are_even_and_made_of_strands() {
        let hole = Hole::new();
        let stats = |p: &Pattern| -> (f32, f32) {
            let mean = p.cells.iter().sum::<f32>() / p.cells.len() as f32;
            let spread = (p.cells.iter().map(|c| (c - mean).powi(2)).sum::<f32>()
                / p.cells.len() as f32)
                .sqrt();
            (mean, spread)
        };
        for mips in [&hole.gas.strands, &hole.gas.rings] {
            assert!(mips.levels.len() >= 4);
            for level in &mips.levels {
                let (mean, _) = stats(level);
                assert!((0.6..1.4).contains(&mean), "{mean}");
            }
            // The finest level has dark gaps and bright lines; the coarser ones even them out.
            assert!(stats(&mips.levels[0]).1 > stats(&mips.levels[3]).1 * 1.1);
        }
    }

    #[test]
    fn a_baked_pattern_turns_with_the_gas_and_mixes_two_levels() {
        let hole = Hole::new();
        let mips = &hole.gas.strands;
        let mut still = Pattern {
            across: 2,
            around: 2,
            cells: Vec::new(),
        };
        let mut turned = Pattern {
            across: 2,
            around: 2,
            cells: Vec::new(),
        };
        mips.bake(1.0, |_| (0.0, 0.0), 1.0, &mut still);
        assert_eq!(still.across, mips.levels[1].across);
        assert_eq!(still.cells, mips.levels[1].cells);
        // A quarter of a turn on: the same pattern, a quarter of the way round.
        mips.bake(1.0, |_| (0.25, 0.0), 1.0, &mut turned);
        let cols = still.around;
        assert!((turned.cells[5 * cols + 10] - still.cells[5 * cols + 10 + cols / 4]).abs() < 1e-5);
        // Half way between two levels the result is their mean, and its overall level is kept.
        mips.bake(0.5, |_| (0.0, 0.0), 1.0, &mut turned);
        assert_eq!(turned.across, mips.levels[0].across);
        let mean = |p: &Pattern| p.cells.iter().sum::<f32>() / p.cells.len() as f32;
        assert!((mean(&turned) - mean(&mips.levels[0])).abs() < 0.02);
        mips.bake(99.0, |_| (0.0, 0.0), 0.5, &mut turned);
        assert_eq!(turned.across, mips.levels[mips.levels.len() - 1].across);
    }

    #[test]
    fn nothing_is_drawn_while_faded_out_or_off_the_frame() {
        let mut hole = Hole::new();
        let mut data = vec![7u8; 64 * 64 * 4];
        let mut frame = Frame::new(&mut data, 64, 64, Blend::Add);
        let mut view = HoleView {
            x: 32.0,
            y: 32.0,
            radius: 6.0,
            normal: Vec3::UP,
            time: 0.0,
            gain: 0.0,
        };
        hole.draw(&mut frame, &view);
        view.gain = 1.0;
        view.x = -900.0;
        hole.draw(&mut frame, &view);
        view.x = f32::NAN;
        hole.draw(&mut frame, &view);
        assert!(data.iter().all(|b| *b == 7));
    }
}
