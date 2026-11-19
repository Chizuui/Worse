//! Draws a project's knowledge graph as a picture, on the CPU.
//!
//! Pixel maths: casts between float and integer are everywhere and each one is bounded by the frame's size.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::many_single_char_names,
    clippy::similar_names
)]

pub mod camera;
pub mod engine;
pub mod hole;
pub mod math;
pub mod model;
pub mod orbit;
pub mod raster;
pub mod space;

pub use engine::{Engine, Marker, Overlay, Style};
pub use model::Graph;
pub use orbit::Palette;
