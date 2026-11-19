//! The maths the pictures share: 3D vectors, a repeatable random source, value noise and a few curves.

use std::ops::{Add, Mul, Sub};
use std::sync::OnceLock;

/// A full turn.
pub const TAU: f32 = std::f32::consts::TAU;
/// The golden angle: consecutive points of a sunflower spiral, and a spread that never lines up.
pub const GOLDEN: f32 = 2.399_963;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub const ZERO: Vec3 = Vec3::new(0.0, 0.0, 0.0);
    pub const UP: Vec3 = Vec3::new(0.0, 1.0, 0.0);

    #[must_use]
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    #[must_use]
    pub fn dot(self, other: Vec3) -> f32 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }

    #[must_use]
    pub fn cross(self, other: Vec3) -> Vec3 {
        Vec3::new(
            self.y * other.z - self.z * other.y,
            self.z * other.x - self.x * other.z,
            self.x * other.y - self.y * other.x,
        )
    }

    #[must_use]
    pub fn len(self) -> f32 {
        self.dot(self).sqrt()
    }

    /// The same direction with length one; a zero vector stays zero.
    #[must_use]
    pub fn unit(self) -> Vec3 {
        let len = self.len();
        if len > 1e-9 {
            self * (1.0 / len)
        } else {
            Vec3::ZERO
        }
    }

    #[must_use]
    pub fn lerp(self, other: Vec3, t: f32) -> Vec3 {
        self + (other - self) * t
    }
}

impl Add for Vec3 {
    type Output = Vec3;
    fn add(self, other: Vec3) -> Vec3 {
        Vec3::new(self.x + other.x, self.y + other.y, self.z + other.z)
    }
}

impl Sub for Vec3 {
    type Output = Vec3;
    fn sub(self, other: Vec3) -> Vec3 {
        Vec3::new(self.x - other.x, self.y - other.y, self.z - other.z)
    }
}

impl Mul<f32> for Vec3 {
    type Output = Vec3;
    fn mul(self, by: f32) -> Vec3 {
        Vec3::new(self.x * by, self.y * by, self.z * by)
    }
}

/// A small repeatable random source (SplitMix64): the same seed draws the same picture every time.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    pub fn bits(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// In `0.0..1.0`.
    pub fn unit(&mut self) -> f32 {
        (self.bits() >> 40) as f32 / 16_777_216.0
    }

    pub fn range(&mut self, low: f32, high: f32) -> f32 {
        low + (high - low) * self.unit()
    }

    /// A direction, every one equally likely.
    pub fn direction(&mut self) -> Vec3 {
        let y = self.range(-1.0, 1.0);
        let around = self.range(0.0, TAU);
        let flat = (1.0 - y * y).max(0.0).sqrt();
        Vec3::new(flat * around.cos(), y, flat * around.sin())
    }
}

#[must_use]
pub fn clamp01(x: f32) -> f32 {
    x.clamp(0.0, 1.0)
}

/// 0 below `low`, 1 above `high`, a smooth S in between. `low` may be above `high` (then it falls).
#[must_use]
pub fn smoothstep(low: f32, high: f32, x: f32) -> f32 {
    let t = clamp01((x - low) / (high - low));
    t * t * (3.0 - 2.0 * t)
}

#[must_use]
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

const FALLOFF_STEPS: usize = 2048;
const FALLOFF_REACH: f32 = 10.0;

fn falloff_table() -> &'static [f32] {
    static TABLE: OnceLock<Vec<f32>> = OnceLock::new();
    TABLE.get_or_init(|| {
        (0..=FALLOFF_STEPS)
            .map(|i| (-(i as f32) * FALLOFF_REACH / FALLOFF_STEPS as f32).exp())
            .collect()
    })
}

/// `exp(-x)` for `x >= 0` from a table: glows are drawn with it once per pixel.
#[must_use]
pub fn falloff(x: f32) -> f32 {
    if x >= FALLOFF_REACH {
        return 0.0;
    }
    let at = x.max(0.0) * (FALLOFF_STEPS as f32 / FALLOFF_REACH);
    let table = falloff_table();
    let i = at as usize;
    let t = at - i as f32;
    table[i] + (table[(i + 1).min(FALLOFF_STEPS)] - table[i]) * t
}

/// `atan2` good to about a thousandth of a radian, without the cost of the exact one.
#[must_use]
pub fn fast_atan2(y: f32, x: f32) -> f32 {
    let ax = x.abs();
    let ay = y.abs();
    let (low, high) = if ax > ay { (ay, ax) } else { (ax, ay) };
    if high < 1e-12 {
        return 0.0;
    }
    let a = low / high;
    let s = a * a;
    let mut r = ((-0.046_496_475 * s + 0.159_314_22) * s - 0.327_622_76) * s * a + a;
    if ay > ax {
        r = std::f32::consts::FRAC_PI_2 - r;
    }
    if x < 0.0 {
        r = std::f32::consts::PI - r;
    }
    if y < 0.0 {
        -r
    } else {
        r
    }
}

/// Value noise on a square grid that repeats, so it can be scrolled and wrapped around a circle.
#[derive(Debug, Clone)]
pub struct Noise {
    size: usize,
    cells: Vec<f32>,
}

impl Noise {
    /// `size` is rounded up to a power of two.
    #[must_use]
    pub fn new(size: usize, seed: u64) -> Self {
        let size = size.max(2).next_power_of_two();
        let mut rng = Rng::new(seed);
        let cells = (0..size * size).map(|_| rng.unit()).collect();
        Self { size, cells }
    }

    fn cell(&self, x: i64, y: i64) -> f32 {
        let mask = self.size as i64 - 1;
        self.cells[(y & mask) as usize * self.size + (x & mask) as usize]
    }

    /// In `0.0..1.0`, smooth between grid points.
    #[must_use]
    pub fn at(&self, x: f32, y: f32) -> f32 {
        self.at_wrapped(x, y, self.size as i64)
    }

    /// Like [`Noise::at`], with `y` repeating every `period` cells (a whole turn around a circle).
    #[must_use]
    pub fn at_wrapped(&self, x: f32, y: f32, period: i64) -> f32 {
        let fx = x.floor();
        let fy = y.floor();
        let tx = x - fx;
        let ty = y - fy;
        let tx = tx * tx * (3.0 - 2.0 * tx);
        let ty = ty * ty * (3.0 - 2.0 * ty);
        let ix = fx as i64;
        let iy0 = (fy as i64).rem_euclid(period);
        let iy1 = (iy0 + 1) % period;
        let a = self.cell(ix, iy0);
        let b = self.cell(ix + 1, iy0);
        let c = self.cell(ix, iy1);
        let d = self.cell(ix + 1, iy1);
        lerp(lerp(a, b, tx), lerp(c, d, tx), ty)
    }

    /// Several layers of the noise, each twice as fine and half as strong; still in `0.0..1.0`.
    #[must_use]
    pub fn fbm(&self, x: f32, y: f32, octaves: u32) -> f32 {
        let mut sum = 0.0;
        let mut weight = 0.5;
        let mut total = 0.0;
        let mut scale = 1.0;
        for octave in 0..octaves {
            let shift = octave as f32 * 17.3;
            sum += weight * self.at(x * scale + shift, y * scale - shift);
            total += weight;
            weight *= 0.5;
            scale *= 2.0;
        }
        sum / total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falloff_follows_the_exponential() {
        for x in [0.0_f32, 0.3, 1.0, 2.5, 7.9] {
            assert!((falloff(x) - (-x).exp()).abs() < 1e-3, "{x}");
        }
        assert!(falloff(40.0).abs() < f32::EPSILON);
    }

    #[test]
    fn fast_atan2_is_close_everywhere() {
        for i in 0..360 {
            let a = (i as f32).to_radians() - std::f32::consts::PI;
            let (y, x) = a.sin_cos();
            let got = fast_atan2(y, x);
            let diff = (got - a).abs();
            assert!(diff < 2e-3 || (diff - TAU).abs() < 2e-3, "{a} -> {got}");
        }
    }

    #[test]
    fn noise_wraps_around_a_circle() {
        let noise = Noise::new(64, 7);
        for i in 0..40 {
            let x = i as f32 * 0.37;
            assert!((noise.at_wrapped(x, 0.0, 8) - noise.at_wrapped(x, 8.0, 8)).abs() < 1e-6);
            let v = noise.at(x, x * 0.5);
            assert!((0.0..=1.0).contains(&v));
        }
    }

    #[test]
    fn the_random_source_repeats() {
        let mut a = Rng::new(5);
        let mut b = Rng::new(5);
        for _ in 0..10 {
            assert_eq!(a.bits(), b.bits());
        }
        let d = a.direction();
        assert!((d.len() - 1.0).abs() < 1e-4);
    }
}
