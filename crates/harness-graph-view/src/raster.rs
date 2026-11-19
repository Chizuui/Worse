//! The pixels: a frame, and the few shapes the pictures are made of.
//!
//! A frame is RGBA with premultiplied alpha, four bytes a pixel, rows from the top. Positions are in pixels
//! with fractions: `(0.5, 0.5)` is the middle of the first pixel. Every shape is anti-aliased by coverage, so
//! something that moves a fraction of a pixel a frame glides instead of hopping.

use crate::math::falloff;

/// How a shape meets what is already in the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blend {
    /// Light on a dark, opaque frame: colours add up and clip at white.
    Add,
    /// Paint over whatever is there, see-through where the shape is; the frame may be transparent.
    Over,
}

/// A colour, each part in `0.0..=1.0`.
pub type Rgb = [f32; 3];

pub struct Frame<'a> {
    width: usize,
    height: usize,
    data: &'a mut [u8],
    blend: Blend,
}

fn add(to: u8, plus: f32) -> u8 {
    (u32::from(to) + (plus + 0.5) as u32).min(255) as u8
}

impl<'a> Frame<'a> {
    /// `data` holds `width * height * 4` bytes; a shorter slice draws nothing (the frame gets no height).
    pub fn new(data: &'a mut [u8], width: usize, height: usize, blend: Blend) -> Self {
        let height = if width == 0 {
            0
        } else {
            height.min(data.len() / (width * 4))
        };
        Self {
            width,
            height,
            data,
            blend,
        }
    }

    #[must_use]
    pub fn width(&self) -> usize {
        self.width
    }

    #[must_use]
    pub fn height(&self) -> usize {
        self.height
    }

    pub fn set_blend(&mut self, blend: Blend) {
        self.blend = blend;
    }

    /// The bytes, for a pass that shades pixel by pixel.
    pub fn bytes(&mut self) -> &mut [u8] {
        self.data
    }

    /// Fills the frame with one colour (premultiplied by `alpha`).
    pub fn clear(&mut self, rgb: Rgb, alpha: f32) {
        let px = [
            (rgb[0] * alpha * 255.0 + 0.5) as u8,
            (rgb[1] * alpha * 255.0 + 0.5) as u8,
            (rgb[2] * alpha * 255.0 + 0.5) as u8,
            (alpha * 255.0 + 0.5) as u8,
        ];
        for chunk in self.data.chunks_exact_mut(4) {
            chunk.copy_from_slice(&px);
        }
    }

    fn put(&mut self, x: usize, y: usize, rgb: Rgb, amount: f32) {
        if amount <= 0.003 {
            return;
        }
        let i = (y * self.width + x) * 4;
        let px = &mut self.data[i..i + 4];
        match self.blend {
            Blend::Add => {
                let k = amount * 255.0;
                px[0] = add(px[0], rgb[0] * k);
                px[1] = add(px[1], rgb[1] * k);
                px[2] = add(px[2], rgb[2] * k);
            }
            Blend::Over => {
                let a = amount.min(1.0);
                let keep = 1.0 - a;
                let k = a * 255.0;
                px[0] = (rgb[0] * k + f32::from(px[0]) * keep + 0.5) as u8;
                px[1] = (rgb[1] * k + f32::from(px[1]) * keep + 0.5) as u8;
                px[2] = (rgb[2] * k + f32::from(px[2]) * keep + 0.5) as u8;
                px[3] = (k + f32::from(px[3]) * keep + 0.5) as u8;
            }
        }
    }

    /// The pixel rows and columns a box touches, or `None` when it misses the frame.
    fn span(
        &self,
        left: f32,
        top: f32,
        right: f32,
        bottom: f32,
    ) -> Option<(usize, usize, usize, usize)> {
        if self.width == 0 || self.height == 0 {
            return None;
        }
        if !(left.is_finite() && top.is_finite() && right.is_finite() && bottom.is_finite()) {
            return None;
        }
        if right < 0.0 || bottom < 0.0 || left >= self.width as f32 || top >= self.height as f32 {
            return None;
        }
        let x0 = left.floor().max(0.0) as usize;
        let y0 = top.floor().max(0.0) as usize;
        let x1 = (right.floor().max(0.0) as usize).min(self.width - 1);
        let y1 = (bottom.floor().max(0.0) as usize).min(self.height - 1);
        Some((x0, y0, x1, y1))
    }

    /// A soft round light: `gain` at the middle, fading as a bell curve `sigma` pixels wide.
    pub fn glow(&mut self, x: f32, y: f32, sigma: f32, rgb: Rgb, gain: f32) {
        if gain <= 0.003 {
            return;
        }
        let sigma = sigma.max(0.45);
        let reach = sigma * 3.2 + 0.5;
        let Some((x0, y0, x1, y1)) = self.span(x - reach, y - reach, x + reach, y + reach) else {
            return;
        };
        let inv = 0.5 / (sigma * sigma);
        for py in y0..=y1 {
            let dy = py as f32 + 0.5 - y;
            for px in x0..=x1 {
                let dx = px as f32 + 0.5 - x;
                self.put(px, py, rgb, gain * falloff((dx * dx + dy * dy) * inv));
            }
        }
    }

    /// A point one pixel wide, shared between the four pixels it lies on.
    pub fn dot(&mut self, x: f32, y: f32, rgb: Rgb, gain: f32) {
        let fx = (x - 0.5).floor();
        let fy = (y - 0.5).floor();
        let tx = x - 0.5 - fx;
        let ty = y - 0.5 - fy;
        for (ox, oy, share) in [
            (0.0, 0.0, (1.0 - tx) * (1.0 - ty)),
            (1.0, 0.0, tx * (1.0 - ty)),
            (0.0, 1.0, (1.0 - tx) * ty),
            (1.0, 1.0, tx * ty),
        ] {
            let px = fx + ox;
            let py = fy + oy;
            if px >= 0.0 && py >= 0.0 && (px as usize) < self.width && (py as usize) < self.height {
                self.put(px as usize, py as usize, rgb, gain * share);
            }
        }
    }

    /// A straight line `width` pixels thick whose strength runs from `gain0` at the start to `gain1` at the end.
    #[allow(clippy::too_many_arguments)]
    pub fn line(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        width: f32,
        rgb: Rgb,
        gain0: f32,
        gain1: f32,
    ) {
        if gain0 <= 0.003 && gain1 <= 0.003 {
            return;
        }
        let dx = x1 - x0;
        let dy = y1 - y0;
        let len = dx.hypot(dy);
        if !len.is_finite() || len < 1e-3 {
            return;
        }
        let half = width.max(0.2) * 0.5;
        if self
            .span(
                x0.min(x1) - half - 1.0,
                y0.min(y1) - half - 1.0,
                x0.max(x1) + half + 1.0,
                y0.max(y1) + half + 1.0,
            )
            .is_none()
        {
            return;
        }
        // Walk the longer axis (`u`) and fill the few pixels across (`v`) at each step.
        let steep = dy.abs() > dx.abs();
        let (mut ua, mut va, mut ga, mut ub, mut vb, mut gb) = if steep {
            (y0, x0, gain0, y1, x1, gain1)
        } else {
            (x0, y0, gain0, x1, y1, gain1)
        };
        if ua > ub {
            std::mem::swap(&mut ua, &mut ub);
            std::mem::swap(&mut va, &mut vb);
            std::mem::swap(&mut ga, &mut gb);
        }
        let (u_limit, v_limit) = if steep {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        };
        let run = ub - ua;
        let slope = (vb - va) / run;
        // Across the line a pixel is `cos` times nearer than it is along `v`.
        let cos = run / len;
        let reach = (half + 0.5) / cos;
        let first = ua.floor().max(0.0) as usize;
        let last = (ub.floor().max(0.0) as usize).min(u_limit - 1);
        if ub < 0.0 || first > last {
            return;
        }
        for pu in first..=last {
            let low = (pu as f32).max(ua);
            let high = (pu as f32 + 1.0).min(ub);
            let end = (high - low).clamp(0.0, 1.0);
            if end <= 0.0 {
                continue;
            }
            let uc = (low + high) * 0.5;
            let t = (uc - ua) / run;
            let vc = va + (uc - ua) * slope;
            let gain = (ga + (gb - ga) * t) * end;
            let v_first = (vc - reach).floor();
            let v_last = (vc + reach).floor();
            if v_last < 0.0 || v_first >= v_limit as f32 {
                continue;
            }
            let v_first = v_first.max(0.0) as usize;
            let v_last = (v_last as usize).min(v_limit - 1);
            for pv in v_first..=v_last {
                let away = (pv as f32 + 0.5 - vc).abs() * cos;
                let cover = (half + 0.5 - away).clamp(0.0, 1.0);
                if cover > 0.0 {
                    if steep {
                        self.put(pv, pu, rgb, gain * cover);
                    } else {
                        self.put(pu, pv, rgb, gain * cover);
                    }
                }
            }
        }
    }

    /// A filled circle.
    pub fn disc(&mut self, x: f32, y: f32, radius: f32, rgb: Rgb, amount: f32) {
        let reach = radius + 1.0;
        let Some((x0, y0, x1, y1)) = self.span(x - reach, y - reach, x + reach, y + reach) else {
            return;
        };
        for py in y0..=y1 {
            let dy = py as f32 + 0.5 - y;
            for px in x0..=x1 {
                let dx = px as f32 + 0.5 - x;
                let cover = (radius + 0.5 - dx.hypot(dy)).clamp(0.0, 1.0);
                if cover > 0.0 {
                    self.put(px, py, rgb, amount * cover);
                }
            }
        }
    }

    /// The outline of a circle, `width` pixels thick.
    pub fn ring(&mut self, x: f32, y: f32, radius: f32, width: f32, rgb: Rgb, amount: f32) {
        let half = width * 0.5;
        let reach = radius + half + 1.0;
        let Some((x0, y0, x1, y1)) = self.span(x - reach, y - reach, x + reach, y + reach) else {
            return;
        };
        for py in y0..=y1 {
            let dy = py as f32 + 0.5 - y;
            for px in x0..=x1 {
                let dx = px as f32 + 0.5 - x;
                let cover = (half + 0.5 - (dx.hypot(dy) - radius).abs()).clamp(0.0, 1.0);
                if cover > 0.0 {
                    self.put(px, py, rgb, amount * cover);
                }
            }
        }
    }

    /// The outline of an ellipse with its axes along the frame's, drawn as short straight pieces.
    #[allow(clippy::too_many_arguments)]
    pub fn ellipse(&mut self, x: f32, y: f32, rx: f32, ry: f32, width: f32, rgb: Rgb, amount: f32) {
        let reach = width + 1.0;
        if self
            .span(
                x - rx - reach,
                y - ry - reach,
                x + rx + reach,
                y + ry + reach,
            )
            .is_none()
        {
            return;
        }
        let pieces = ((rx.max(ry) * 0.9) as usize).clamp(24, 360);
        let step = crate::math::TAU / pieces as f32;
        let mut from = (x + rx, y);
        for i in 1..=pieces {
            let (sin, cos) = (step * i as f32).sin_cos();
            let to = (x + rx * cos, y + ry * sin);
            self.line(from.0, from.1, to.0, to.1, width, rgb, amount, amount);
            from = to;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum(data: &[u8]) -> u64 {
        data.chunks_exact(4).map(|px| u64::from(px[0])).sum()
    }

    #[test]
    fn a_dot_keeps_its_light_wherever_it_lands() {
        for (x, y) in [(5.5, 5.5), (5.0, 5.0), (5.25, 5.8), (5.9, 5.1)] {
            let mut data = vec![0u8; 12 * 12 * 4];
            Frame::new(&mut data, 12, 12, Blend::Add).dot(x, y, [1.0, 1.0, 1.0], 0.8);
            let total = sum(&data);
            assert!((200..=208).contains(&total), "{x},{y}: {total}");
        }
    }

    #[test]
    fn a_line_is_about_as_bright_at_any_angle() {
        let mut totals = Vec::new();
        for (x1, y1) in [(30.0, 10.0), (10.0, 30.0), (24.1, 24.1), (28.0, 18.0)] {
            let mut data = vec![0u8; 40 * 40 * 4];
            Frame::new(&mut data, 40, 40, Blend::Add).line(
                10.0,
                10.0,
                x1,
                y1,
                1.0,
                [1.0, 1.0, 1.0],
                0.5,
                0.5,
            );
            let len = (x1 - 10.0_f32).hypot(y1 - 10.0);
            totals.push(sum(&data) as f32 / len);
        }
        for per_pixel in &totals {
            // A diagonal through the middles of the pixels is the worst case of the coverage sums.
            assert!(
                (per_pixel - totals[0]).abs() < totals[0] * 0.15,
                "{totals:?}"
            );
        }
    }

    #[test]
    fn shapes_off_the_frame_are_skipped_and_edges_are_safe() {
        let mut data = vec![0u8; 8 * 8 * 4];
        let mut frame = Frame::new(&mut data, 8, 8, Blend::Add);
        frame.glow(-40.0, 3.0, 2.0, [1.0; 3], 1.0);
        frame.line(-5.0, -5.0, 20.0, 30.0, 1.5, [1.0; 3], 1.0, 0.0);
        frame.line(f32::NAN, 0.0, 3.0, 3.0, 1.0, [1.0; 3], 1.0, 1.0);
        frame.disc(7.9, 7.9, 3.0, [1.0; 3], 1.0);
        frame.ring(0.0, 0.0, 4.0, 1.0, [1.0; 3], 1.0);
        frame.ellipse(4.0, 4.0, 30.0, 12.0, 1.0, [1.0; 3], 1.0);
        frame.dot(7.9, 7.9, [1.0; 3], 1.0);
        assert!(sum(&data) > 0);
    }

    #[test]
    fn painting_over_keeps_alpha_premultiplied() {
        let mut data = vec![0u8; 6 * 6 * 4];
        let mut frame = Frame::new(&mut data, 6, 6, Blend::Over);
        frame.disc(3.0, 3.0, 2.0, [1.0, 0.5, 0.0], 0.5);
        let px = &data[(3 * 6 + 3) * 4..(3 * 6 + 3) * 4 + 4];
        assert_eq!(px[3], 128);
        assert_eq!(px[0], 128);
        assert_eq!(px[1], 64);
        assert_eq!(px[2], 0);
    }

    #[test]
    fn a_short_buffer_draws_nothing() {
        let mut data = vec![0u8; 10];
        let mut frame = Frame::new(&mut data, 8, 8, Blend::Add);
        frame.disc(2.0, 2.0, 2.0, [1.0; 3], 1.0);
        assert_eq!(frame.height(), 0);
    }
}
