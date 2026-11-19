//! Where the eye is, and where a point of the world lands on the frame.

use crate::math::Vec3;

/// An eye that circles a point it looks at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    /// Around the vertical axis, in radians.
    pub yaw: f32,
    /// Above the horizon (below, when negative), in radians.
    pub pitch: f32,
    /// 1 shows the whole picture; larger moves the eye in.
    pub zoom: f32,
    /// The point looked at.
    pub target: Vec3,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            yaw: 0.0,
            pitch: 0.0,
            zoom: 1.0,
            target: Vec3::ZERO,
        }
    }
}

/// A camera worked out for one frame.
#[derive(Debug, Clone, Copy)]
pub struct Lens {
    eye: Vec3,
    right: Vec3,
    up: Vec3,
    forward: Vec3,
    /// Pixels per world unit at one unit of depth; 0 for a flat (parallel) view.
    focal: f32,
    /// Pixels per world unit of a flat view.
    flat: f32,
    distance: f32,
    cx: f32,
    cy: f32,
}

/// A point of the world on the frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Seen {
    pub x: f32,
    pub y: f32,
    /// How far in front of the eye.
    pub depth: f32,
    /// Pixels per world unit at that depth.
    pub scale: f32,
}

impl Lens {
    /// A view with depth: things nearer the eye are larger. At zoom 1 a ball of radius `reach` around the
    /// target is `spread` pixels in radius; the eye stands `stand` reaches away and comes closer as the zoom
    /// grows.
    #[must_use]
    pub fn deep(
        camera: &Camera,
        width: f32,
        height: f32,
        reach: f32,
        spread: f32,
        stand: f32,
    ) -> Self {
        let mut lens = Self::facing(camera, width, height);
        let far = reach * stand;
        lens.distance = far / camera.zoom.max(0.05);
        lens.focal = spread * far / reach;
        lens.eye = camera.target - lens.forward * lens.distance;
        lens
    }

    /// A view without depth: everything keeps its size, `scale` pixels per world unit at zoom 1.
    #[must_use]
    pub fn level(camera: &Camera, width: f32, height: f32, scale: f32) -> Self {
        let mut lens = Self::facing(camera, width, height);
        lens.flat = scale * camera.zoom.max(0.05);
        lens.distance = 1000.0;
        lens.eye = camera.target - lens.forward * lens.distance;
        lens
    }

    /// The same view with its middle moved over the frame, in pixels: to make room for a panel.
    #[must_use]
    pub fn slid(mut self, right: f32, down: f32) -> Self {
        self.cx += right;
        self.cy += down;
        self
    }

    fn facing(camera: &Camera, width: f32, height: f32) -> Self {
        let (sin_yaw, cos_yaw) = camera.yaw.sin_cos();
        let (sin_pitch, cos_pitch) = camera.pitch.sin_cos();
        let forward = Vec3::new(-sin_yaw * cos_pitch, -sin_pitch, -cos_yaw * cos_pitch);
        let right = Vec3::new(cos_yaw, 0.0, -sin_yaw);
        Self {
            eye: camera.target,
            right,
            up: right.cross(forward),
            forward,
            focal: 0.0,
            flat: 0.0,
            distance: 1.0,
            cx: width * 0.5,
            cy: height * 0.5,
        }
    }

    /// Where `point` lands, or `None` when it is behind the eye or too near to it.
    #[must_use]
    pub fn see(&self, point: Vec3) -> Option<Seen> {
        let from_eye = point - self.eye;
        let depth = from_eye.dot(self.forward);
        let scale = if self.focal > 0.0 {
            if depth < self.distance * 0.04 {
                return None;
            }
            self.focal / depth
        } else {
            self.flat
        };
        Some(Seen {
            x: self.cx + from_eye.dot(self.right) * scale,
            y: self.cy - from_eye.dot(self.up) * scale,
            depth,
            scale,
        })
    }

    /// A direction of the world in the eye's own frame: x to the right, y up, z away from the eye.
    #[must_use]
    pub fn turn(&self, direction: Vec3) -> Vec3 {
        Vec3::new(
            direction.dot(self.right),
            direction.dot(self.up),
            direction.dot(self.forward),
        )
    }

    /// How far the eye stands from the point it looks at.
    #[must_use]
    pub fn distance(&self) -> f32 {
        self.distance
    }

    /// The world direction of one pixel to the right and of one pixel up, each a world unit long.
    #[must_use]
    pub fn axes(&self) -> (Vec3, Vec3) {
        (self.right, self.up)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_is_in_the_middle_and_right_is_right() {
        let camera = Camera::default();
        let lens = Lens::deep(&camera, 800.0, 400.0, 10.0, 150.0, 3.5);
        let middle = lens.see(Vec3::ZERO).expect("seen");
        assert!((middle.x - 400.0).abs() < 1e-3 && (middle.y - 200.0).abs() < 1e-3);
        assert!((middle.depth - 35.0).abs() < 1e-3);
        // A point one reach to the right is `spread` pixels to the right.
        let right = lens.see(Vec3::new(10.0, 0.0, 0.0)).expect("seen");
        assert!((right.x - 550.0).abs() < 0.5, "{right:?}");
        let up = lens.see(Vec3::new(0.0, 10.0, 0.0)).expect("seen");
        assert!(up.y < 60.0, "{up:?}");
        // Nearer is larger; behind the eye is not seen.
        let near = lens.see(Vec3::new(0.0, 0.0, 9.0)).expect("seen");
        assert!(near.scale > middle.scale);
        assert!(lens.see(Vec3::new(0.0, 0.0, 60.0)).is_none());
    }

    #[test]
    fn zoom_moves_the_eye_in_and_turning_keeps_the_frame_square() {
        let mut camera = Camera {
            yaw: 0.7,
            pitch: 0.4,
            ..Camera::default()
        };
        let far = Lens::deep(&camera, 600.0, 600.0, 10.0, 200.0, 3.5);
        camera.zoom = 2.0;
        let near = Lens::deep(&camera, 600.0, 600.0, 10.0, 200.0, 3.5);
        assert!((far.distance() - 2.0 * near.distance()).abs() < 1e-3);
        let (right, up) = far.axes();
        assert!(right.dot(up).abs() < 1e-5);
        assert!((far.turn(Vec3::UP).len() - 1.0).abs() < 1e-5);
        // Looking down from above, "up" in the world leans towards the eye.
        assert!(far.turn(Vec3::UP).z < 0.0);
    }

    #[test]
    fn a_level_view_has_no_depth() {
        let camera = Camera {
            pitch: 0.5,
            zoom: 2.0,
            ..Camera::default()
        };
        let lens = Lens::level(&camera, 400.0, 300.0, 3.0);
        let a = lens.see(Vec3::new(10.0, 0.0, 0.0)).expect("seen");
        let b = lens.see(Vec3::new(10.0, 0.0, 50.0)).expect("seen");
        assert!((a.scale - 6.0).abs() < 1e-5 && (b.scale - 6.0).abs() < 1e-5);
        assert!((a.x - 260.0).abs() < 1e-3);
        assert!(
            b.y > a.y,
            "nearer points of the ground lie lower on the frame"
        );
    }
}
