//! Draws one frame of the graph picture and writes it as a PPM: a quick look at a change without the app.
//!
//! `frame <out.ppm> [style] [graph.json] [width] [height] [unit] [seconds] [drag-x] [drag-y] [focus]`
//! `style` is `space` (the galaxy) or `orbit` (the solar system); without a graph file a made-up one of six
//! communities is drawn. The drag turns the galaxy (or slides the solar system) by that many pixels first.
//! The solar system is see-through: it is laid over a dark page here.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::expect_used
)]

use std::sync::Arc;

use harness_graph_view::math::Rng;
use harness_graph_view::{Engine, Graph, Style};

fn sample() -> String {
    let sizes = [150_usize, 110, 44, 36, 28, 16];
    let mut rng = Rng::new(11);
    let mut nodes = Vec::new();
    let mut links = Vec::new();
    for (g, &size) in sizes.iter().enumerate() {
        for i in 0..size {
            nodes.push(format!(
                r#"{{"id":"n{g}_{i}","label":"node_{g}_{i}.rs","source_file":"src/g{g}/f{i}.rs","community":{g}}}"#
            ));
            if i > 0 {
                // A few hubs, then chains hanging off them.
                let parent = if i < 6 {
                    0
                } else if rng.unit() < 0.35 {
                    (rng.unit() * 6.0) as usize
                } else {
                    (rng.unit() * i as f32) as usize
                };
                links.push(format!(
                    r#"{{"source":"n{g}_{parent}","target":"n{g}_{i}"}}"#
                ));
                if rng.unit() < 0.25 {
                    let other = (rng.unit() * i as f32) as usize;
                    links.push(format!(
                        r#"{{"source":"n{g}_{other}","target":"n{g}_{i}"}}"#
                    ));
                }
            }
        }
        if g > 0 {
            for k in 0..4 {
                links.push(format!(
                    r#"{{"source":"n{g}_{k}","target":"n{}_{}"}}"#,
                    g - 1,
                    k + 1
                ));
            }
        }
    }
    format!(
        r#"{{"nodes":[{}],"links":[{}]}}"#,
        nodes.join(","),
        links.join(",")
    )
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let text = |i: usize| {
        args.get(i)
            .map(String::as_str)
            .filter(|s| !s.is_empty() && *s != "-")
    };
    let num = |i: usize, default: f32| text(i).and_then(|s| s.parse().ok()).unwrap_or(default);
    let out = text(1).unwrap_or("frame.ppm");
    let style = Style::named(text(2).unwrap_or("space"));
    let json = text(3).map_or_else(sample, |path| {
        std::fs::read_to_string(path).expect("the graph file")
    });
    let graph = Arc::new(Graph::parse(&json).expect("a graph"));
    let mut engine = Engine::new(Arc::clone(&graph), style);
    engine.resize(num(4, 1114.0) as usize, num(5, 690.0) as usize, num(6, 1.0));
    let focus = num(10, -1.0);
    if focus >= 0.0 {
        engine.set_focus(Some(focus as usize));
    }
    let mut left = num(7, 4.0);
    while left > 0.0 {
        engine.step(0.05);
        left -= 0.05;
    }
    let (drag_x, drag_y) = (num(8, 0.0), num(9, 0.0));
    if drag_x != 0.0 || drag_y != 0.0 {
        engine.pointer_down(300.0, 300.0);
        engine.pointer_drag(300.0 + drag_x, 300.0 + drag_y);
        engine.pointer_up(300.0 + drag_x, 300.0 + drag_y);
        engine.step(0.016);
    }
    let (width, height) = engine.size();
    let mut data = vec![0_u8; width * height * 4];
    let mut longest = std::time::Duration::ZERO;
    let mut total = std::time::Duration::ZERO;
    let runs = 12_u32;
    for _ in 0..runs {
        engine.step(0.016);
        let started = std::time::Instant::now();
        engine.render(&mut data);
        let took = started.elapsed();
        longest = longest.max(took);
        total += took;
    }
    eprintln!(
        "{} nodes, {width}x{height}: a frame takes {:?} on average, {longest:?} at most",
        graph.nodes.len(),
        total / runs
    );
    let page = [18.0_f32, 19.0, 22.0];
    let mut file = format!("P6\n{width} {height}\n255\n").into_bytes();
    for px in data.chunks_exact(4) {
        let through = 1.0 - f32::from(px[3]) / 255.0;
        for c in 0..3 {
            file.push((f32::from(px[c]) + page[c] * through).min(255.0) as u8);
        }
    }
    std::fs::write(out, file).expect("writing the picture");
}
