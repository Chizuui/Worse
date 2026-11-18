//! The picture of a Graphify graph on Settings › Memory › Graph: the most connected nodes, laid out by a small
//! force simulation, coloured by community.
//!
//! A project graph has thousands of nodes; a readable picture has a couple of hundred. The view keeps the
//! nodes with the most links and the links among them. Positions come from a Fruchterman–Reingold layout run
//! here (linked nodes pull together, all nodes push apart, a weak pull to the centre keeps loose parts in
//! view), started from one cluster per community so the result is the same every time.

use std::collections::HashMap;

use serde_json::Value;

/// Nodes drawn at most.
pub const MAX_NODES: usize = 160;
/// Links drawn at most (the strongest picture is not the densest one).
const MAX_LINKS: usize = 700;
/// Nodes that always show their name (the most connected).
const NAMED: usize = 14;
const ITERATIONS: usize = 160;
/// The box the link paths are written in. The canvas draws paths at their aspect ratio (it does not stretch
/// them), so the box has roughly the canvas's shape and the nodes are placed by the same fit (`GraphCanvas`).
pub const BOX_W: f32 = 1000.0;
pub const BOX_H: f32 = 532.0;

/// One drawn node.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// Position in the unit square.
    pub x: f32,
    pub y: f32,
    pub label: String,
    /// Where it comes from (`source_file`), for the detail line.
    pub source: String,
    pub community: i32,
    /// Links in the whole graph, not only the drawn ones.
    pub degree: u32,
    /// Always shows its name.
    pub named: bool,
}

/// The drawn part of a graph.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct View {
    pub nodes: Vec<Node>,
    /// Pairs of indexes into `nodes`.
    pub links: Vec<(usize, usize)>,
    pub total_nodes: usize,
    pub total_links: usize,
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// Deterministic pseudo-random number in `0..1` for `seed` (the layout must not change between runs).
fn jitter(seed: u64) -> f32 {
    let mut x = seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(0xD1B5_4A32_D192_ED03);
    x ^= x >> 29;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 32;
    #[allow(clippy::cast_precision_loss)]
    let unit = (x % 10_000) as f32 / 10_000.0;
    unit
}

/// Reads a `graph.json` (node-link data: `nodes`, and `links` or `edges`) into a picture; `None` when the
/// text is not such a graph or has no nodes.
#[must_use]
pub fn build(graph_json: &str) -> Option<View> {
    let graph: Value = serde_json::from_str(graph_json).ok()?;
    let nodes = graph.get("nodes")?.as_array()?;
    if nodes.is_empty() {
        return None;
    }
    let links = graph
        .get("links")
        .or_else(|| graph.get("edges"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let index: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (text(node, "id"), i))
        .collect();
    let mut degree = vec![0u32; nodes.len()];
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for link in &links {
        let (Some(&a), Some(&b)) = (
            index.get(text(link, "source")),
            index.get(text(link, "target")),
        ) else {
            continue;
        };
        if a == b {
            continue;
        }
        degree[a] += 1;
        degree[b] += 1;
        pairs.push((a.min(b), a.max(b)));
    }
    pairs.sort_unstable();
    pairs.dedup();
    // The most connected nodes; ties keep the file's order.
    let mut order: Vec<usize> = (0..nodes.len()).collect();
    order.sort_by_key(|&i| (std::cmp::Reverse(degree[i]), i));
    order.truncate(MAX_NODES);
    let slot: HashMap<usize, usize> = order.iter().enumerate().map(|(at, &i)| (i, at)).collect();
    let mut drawn: Vec<(usize, usize)> = pairs
        .iter()
        .filter_map(|(a, b)| Some((*slot.get(a)?, *slot.get(b)?)))
        .collect();
    // When there are too many links, keep those between the better connected nodes (lower slots).
    drawn.sort_by_key(|(a, b)| a + b);
    drawn.truncate(MAX_LINKS);
    let mut view = View {
        nodes: order
            .iter()
            .enumerate()
            .map(|(at, &i)| {
                let node = &nodes[i];
                let label = match text(node, "label") {
                    "" => text(node, "id"),
                    label => label,
                };
                Node {
                    x: 0.5,
                    y: 0.5,
                    label: label.to_owned(),
                    source: text(node, "source_file").to_owned(),
                    community: node
                        .get("community")
                        .and_then(Value::as_i64)
                        .and_then(|c| i32::try_from(c).ok())
                        .unwrap_or(-1),
                    degree: degree[i],
                    named: at < NAMED,
                }
            })
            .collect(),
        links: drawn,
        total_nodes: nodes.len(),
        total_links: pairs.len(),
    };
    layout(&mut view);
    Some(view)
}

/// Places the nodes (Fruchterman–Reingold in the unit square).
#[allow(clippy::cast_precision_loss)]
fn layout(view: &mut View) {
    let count = view.nodes.len();
    if count == 1 {
        return;
    }
    // Start: one cluster per community on a circle, members scattered around its centre.
    let mut communities: Vec<i32> = view.nodes.iter().map(|n| n.community).collect();
    communities.sort_unstable();
    communities.dedup();
    let mut at: Vec<(f32, f32)> = view
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let slot = communities
                .iter()
                .position(|c| *c == node.community)
                .unwrap_or(0);
            let angle = slot as f32 / communities.len().max(1) as f32 * std::f32::consts::TAU;
            let spread = if communities.len() > 1 { 0.3 } else { 0.0 };
            (
                0.5 + spread * angle.cos() + (jitter(i as u64 * 2 + 1) - 0.5) * 0.18,
                0.5 + spread * angle.sin() + (jitter(i as u64 * 2 + 2) - 0.5) * 0.18,
            )
        })
        .collect();
    let ideal = (1.0 / count as f32).sqrt() * 0.9;
    for step in 0..ITERATIONS {
        let heat = 0.08 * (1.0 - step as f32 / ITERATIONS as f32) + 0.002;
        let mut push = vec![(0.0f32, 0.0f32); count];
        for a in 0..count {
            for b in (a + 1)..count {
                let (dx, dy) = (at[a].0 - at[b].0, at[a].1 - at[b].1);
                let distance = (dx * dx + dy * dy).sqrt().max(0.001);
                let force = ideal * ideal / distance;
                let (fx, fy) = (dx / distance * force, dy / distance * force);
                push[a].0 += fx;
                push[a].1 += fy;
                push[b].0 -= fx;
                push[b].1 -= fy;
            }
        }
        for &(a, b) in &view.links {
            let (dx, dy) = (at[a].0 - at[b].0, at[a].1 - at[b].1);
            let distance = (dx * dx + dy * dy).sqrt().max(0.001);
            let force = distance * distance / ideal;
            let (fx, fy) = (dx / distance * force, dy / distance * force);
            push[a].0 -= fx;
            push[a].1 -= fy;
            push[b].0 += fx;
            push[b].1 += fy;
        }
        for (i, position) in at.iter_mut().enumerate() {
            // A weak pull to the centre, so parts with no link to the rest stay in view.
            let (gx, gy) = ((0.5 - position.0) * 0.6, (0.5 - position.1) * 0.6);
            let (fx, fy) = (push[i].0 + gx, push[i].1 + gy);
            let length = (fx * fx + fy * fy).sqrt().max(0.0001);
            let moved = length.min(heat);
            position.0 += fx / length * moved;
            position.1 += fy / length * moved;
        }
    }
    // Fit into the square with a margin.
    let (mut left, mut right, mut top, mut bottom) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    for &(x, y) in &at {
        left = left.min(x);
        right = right.max(x);
        top = top.min(y);
        bottom = bottom.max(y);
    }
    let (width, height) = ((right - left).max(0.001), (bottom - top).max(0.001));
    for (node, &(x, y)) in view.nodes.iter_mut().zip(&at) {
        node.x = 0.05 + (x - left) / width * 0.9;
        node.y = 0.06 + (y - top) / height * 0.88;
    }
}

fn segment(view: &View, a: usize, b: usize, out: &mut String) {
    use std::fmt::Write as _;
    let (from, to) = (&view.nodes[a], &view.nodes[b]);
    let _ = write!(
        out,
        "M {:.1} {:.1} L {:.1} {:.1} ",
        from.x * BOX_W,
        from.y * BOX_H,
        to.x * BOX_W,
        to.y * BOX_H
    );
}

/// SVG path of every drawn link, in a `BOX_W` × `BOX_H` box.
#[must_use]
pub fn links_path(view: &View) -> String {
    let mut path = String::new();
    for &(a, b) in &view.links {
        segment(view, a, b, &mut path);
    }
    path
}

/// SVG path of the links of node `selected` only.
#[must_use]
pub fn focus_path(view: &View, selected: usize) -> String {
    let mut path = String::new();
    for &(a, b) in &view.links {
        if a == selected || b == selected {
            segment(view, a, b, &mut path);
        }
    }
    path
}

/// Indexes of the drawn nodes linked to `selected`, best connected first.
#[must_use]
pub fn neighbours(view: &View, selected: usize) -> Vec<usize> {
    let mut out: Vec<usize> = view
        .links
        .iter()
        .filter_map(|&(a, b)| match (a == selected, b == selected) {
            (true, _) => Some(b),
            (_, true) => Some(a),
            _ => None,
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// "client.py · src/client.py · 23 links · connects to models.py, auth.py (+4 more)".
#[must_use]
pub fn detail(view: &View, selected: usize) -> String {
    let Some(node) = view.nodes.get(selected) else {
        return String::new();
    };
    let mut text = node.label.clone();
    if !node.source.is_empty() && node.source != node.label {
        text.push_str(" · ");
        text.push_str(&node.source);
    }
    text.push_str(&format!(
        " · {} link{}",
        node.degree,
        if node.degree == 1 { "" } else { "s" }
    ));
    let near = neighbours(view, selected);
    if !near.is_empty() {
        let names: Vec<&str> = near
            .iter()
            .take(6)
            .map(|&i| view.nodes[i].label.as_str())
            .collect();
        text.push_str(" · connects to ");
        text.push_str(&names.join(", "));
        if near.len() > 6 {
            text.push_str(&format!(" (+{} more)", near.len() - 6));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(nodes: usize) -> String {
        let list: Vec<String> = (0..nodes)
            .map(|i| format!(r#"{{"id":"n{i}","label":"file{i}.rs","source_file":"src/file{i}.rs","community":{}}}"#, i % 3))
            .collect();
        // n0 is a hub; the rest form a ring.
        let mut links: Vec<String> = (1..nodes)
            .map(|i| format!(r#"{{"source":"n0","target":"n{i}"}}"#))
            .collect();
        links.extend(
            (1..nodes)
                .map(|i| format!(r#"{{"source":"n{i}","target":"n{}"}}"#, i % (nodes - 1) + 1)),
        );
        links.push(r#"{"source":"n1","target":"missing"}"#.into());
        format!(
            r#"{{"nodes":[{}],"links":[{}]}}"#,
            list.join(","),
            links.join(",")
        )
    }

    #[test]
    fn keeps_the_most_connected_nodes_inside_the_square() {
        let view = build(&graph(400)).expect("graph");
        assert_eq!(view.total_nodes, 400);
        assert_eq!(view.nodes.len(), MAX_NODES);
        assert_eq!(view.nodes[0].label, "file0.rs", "the hub comes first");
        assert_eq!(view.nodes[0].degree, 399);
        assert!(view.nodes[0].named && !view.nodes[MAX_NODES - 1].named);
        assert!(view.links.len() <= 700 && !view.links.is_empty());
        for node in &view.nodes {
            assert!(
                (0.0..=1.0).contains(&node.x) && (0.0..=1.0).contains(&node.y),
                "{node:?}"
            );
            assert!(node.x.is_finite() && node.y.is_finite());
        }
        // Nodes do not pile up on one spot.
        let first = &view.nodes[1];
        let apart = view.nodes[2..]
            .iter()
            .filter(|n| (n.x - first.x).abs() + (n.y - first.y).abs() > 0.02)
            .count();
        assert!(apart > MAX_NODES - 10, "{apart}");
    }

    #[test]
    fn the_layout_is_the_same_every_time() {
        assert_eq!(build(&graph(60)), build(&graph(60)));
    }

    #[test]
    fn paths_and_detail_follow_the_links() {
        let view = build(&graph(6)).expect("graph");
        assert_eq!(links_path(&view).matches('M').count(), view.links.len());
        let hub = focus_path(&view, 0);
        assert_eq!(hub.matches('M').count(), 5);
        assert_eq!(neighbours(&view, 0).len(), 5);
        let text = detail(&view, 0);
        assert!(
            text.starts_with("file0.rs · src/file0.rs · 5 links · connects to "),
            "{text}"
        );
        assert!(
            build("{}").is_none() && build(r#"{"nodes":[]}"#).is_none() && build("x").is_none()
        );
        // One node, no links: still a picture.
        let lone = build(r#"{"nodes":[{"id":"a"}],"edges":[]}"#).expect("graph");
        assert_eq!((lone.nodes.len(), lone.nodes[0].label.as_str()), (1, "a"));
        assert_eq!(detail(&lone, 0), "a · 0 links");
    }
}
