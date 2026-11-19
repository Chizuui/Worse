//! The graph as the pictures use it: the best connected nodes of a Graphify `graph.json`, grouped by
//! community.
//!
//! A project graph has thousands of nodes; a picture stays readable (and quick to draw) with some hundreds.
//! The nodes with the most links are kept, with the links among them. Nodes are in order of their links, the
//! best connected first, so a picture that wants fewer takes the front of the list.

use std::collections::HashMap;

use serde_json::Value;

/// Nodes kept at most.
pub const MAX_NODES: usize = 700;
/// Links kept at most.
pub const MAX_LINKS: usize = 2600;
/// Communities kept by name at most; smaller ones beyond that, and nodes without a community, share one
/// group called "other".
pub const MAX_CLUSTERS: usize = 9;

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub label: String,
    /// Where it comes from (`source_file`).
    pub source: String,
    /// Index into [`Graph::clusters`].
    pub cluster: usize,
    /// Links in the whole graph, not only the kept ones.
    pub degree: u32,
    /// The best connected node of its cluster.
    pub hub: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cluster {
    /// Its hub's label, or "other".
    pub name: String,
    /// Nodes of this community in the whole graph.
    pub size: usize,
    /// Colour slot: 0 for the largest community, counting up; -1 for "other".
    pub tone: i32,
    /// Indexes into [`Graph::nodes`], the best connected first; never empty.
    pub members: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub clusters: Vec<Cluster>,
    /// Pairs of indexes into `nodes`, the smaller first, each pair once.
    pub links: Vec<(usize, usize)>,
    pub total_nodes: usize,
    pub total_links: usize,
    near: Vec<Vec<usize>>,
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn label(node: &Value) -> &str {
    match text(node, "label") {
        "" => text(node, "id"),
        label => label,
    }
}

impl Graph {
    /// Reads a `graph.json` (node-link data: `nodes`, and `links` or `edges`); `None` when the text is not
    /// such a graph or has no nodes.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn parse(graph_json: &str) -> Option<Graph> {
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
        let community = |i: usize| -> i64 {
            nodes[i]
                .get("community")
                .and_then(Value::as_i64)
                .unwrap_or(-1)
        };
        // Sizes of the communities in the whole graph, for the captions.
        let mut sizes: HashMap<i64, usize> = HashMap::new();
        for i in 0..nodes.len() {
            *sizes.entry(community(i)).or_default() += 1;
        }
        // The best connected nodes; ties keep the file's order.
        let mut order: Vec<usize> = (0..nodes.len()).collect();
        order.sort_by_key(|&i| (std::cmp::Reverse(degree[i]), i));
        order.truncate(MAX_NODES);
        // Their communities, largest (among the kept nodes) first.
        let mut kept_sizes: Vec<(i64, usize)> = Vec::new();
        for &i in &order {
            let c = community(i);
            match kept_sizes.iter_mut().find(|(id, _)| *id == c) {
                Some((_, n)) => *n += 1,
                None => kept_sizes.push((c, 1)),
            }
        }
        kept_sizes.sort_by_key(|&(id, n)| (id < 0, std::cmp::Reverse(n), id));
        let named: Vec<i64> = kept_sizes
            .iter()
            .filter(|(id, _)| *id >= 0)
            .take(MAX_CLUSTERS)
            .map(|(id, _)| *id)
            .collect();
        let has_other = kept_sizes.iter().any(|(id, _)| !named.contains(id));
        let cluster_of = |i: usize| -> usize {
            named
                .iter()
                .position(|id| *id == community(i))
                .unwrap_or(named.len())
        };
        let mut clusters: Vec<Cluster> = (0..named.len() + usize::from(has_other))
            .map(|c| Cluster {
                name: String::new(),
                size: 0,
                tone: if c < named.len() {
                    i32::try_from(c).unwrap_or(-1)
                } else {
                    -1
                },
                members: Vec::new(),
            })
            .collect();
        let mut slot: HashMap<usize, usize> = HashMap::new();
        let mut kept = Vec::with_capacity(order.len());
        for (at, &i) in order.iter().enumerate() {
            let c = cluster_of(i);
            let hub = clusters[c].members.is_empty();
            clusters[c].members.push(at);
            slot.insert(i, at);
            kept.push(Node {
                label: label(&nodes[i]).to_owned(),
                source: text(&nodes[i], "source_file").to_owned(),
                cluster: c,
                degree: degree[i],
                hub,
            });
        }
        for (c, cluster) in clusters.iter_mut().enumerate() {
            if c < named.len() {
                cluster.name = cluster
                    .members
                    .first()
                    .map(|&i| kept[i].label.clone())
                    .unwrap_or_default();
                cluster.size = sizes
                    .get(&named[c])
                    .copied()
                    .unwrap_or(cluster.members.len());
            } else {
                "other".clone_into(&mut cluster.name);
                cluster.size = cluster.members.len();
            }
        }
        let mut drawn: Vec<(usize, usize)> = pairs
            .iter()
            .filter_map(|(a, b)| {
                let (a, b) = (*slot.get(a)?, *slot.get(b)?);
                Some((a.min(b), a.max(b)))
            })
            .collect();
        // When there are too many links, keep those of the better connected nodes.
        drawn.sort_by_key(|&(a, b)| (std::cmp::Reverse(kept[a].degree + kept[b].degree), a, b));
        drawn.truncate(MAX_LINKS);
        drawn.sort_unstable();
        let mut near = vec![Vec::new(); kept.len()];
        for &(a, b) in &drawn {
            near[a].push(b);
            near[b].push(a);
        }
        Some(Graph {
            nodes: kept,
            clusters,
            links: drawn,
            total_nodes: nodes.len(),
            total_links: pairs.len(),
            near,
        })
    }

    /// The kept nodes linked to `node`, in order.
    #[must_use]
    pub fn neighbours(&self, node: usize) -> &[usize] {
        self.near.get(node).map_or(&[], Vec::as_slice)
    }

    /// Nodes whose name or file contains `query` (letter case ignored), the best connected first.
    #[must_use]
    pub fn search(&self, query: &str, limit: usize) -> Vec<usize> {
        let query = query.trim().to_lowercase();
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| {
                query.is_empty()
                    || node.label.to_lowercase().contains(&query)
                    || node.source.to_lowercase().contains(&query)
            })
            .map(|(i, _)| i)
            .take(limit)
            .collect()
    }

    /// "23 links · src/client.py".
    #[must_use]
    pub fn note(&self, node: usize) -> String {
        let Some(node) = self.nodes.get(node) else {
            return String::new();
        };
        let mut text = format!(
            "{} link{}",
            node.degree,
            if node.degree == 1 { "" } else { "s" }
        );
        if !node.source.is_empty() && node.source != node.label {
            text.push_str(" · ");
            text.push_str(&node.source);
        }
        text
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A graph of `groups` communities of `per` nodes: each a hub with spokes and a few links across.
    pub(crate) fn sample(groups: usize, per: usize) -> String {
        let mut nodes = Vec::new();
        let mut links = Vec::new();
        for g in 0..groups {
            for i in 0..per {
                nodes.push(format!(
                    r#"{{"id":"n{g}_{i}","label":"node {g}.{i}","source_file":"src/g{g}/f{i}.rs","community":{g}}}"#
                ));
                if i > 0 {
                    links.push(format!(r#"{{"source":"n{g}_0","target":"n{g}_{i}"}}"#));
                }
                if i > 2 && i % 3 == 0 {
                    links.push(format!(
                        r#"{{"source":"n{g}_{}","target":"n{g}_{i}"}}"#,
                        i - 2
                    ));
                }
            }
            if g > 0 {
                links.push(format!(r#"{{"source":"n{g}_1","target":"n{}_1"}}"#, g - 1));
            }
        }
        format!(
            r#"{{"nodes":[{}],"links":[{}]}}"#,
            nodes.join(","),
            links.join(",")
        )
    }

    #[test]
    fn nodes_come_best_connected_first_and_grouped() {
        let graph = Graph::parse(&sample(3, 12)).expect("graph");
        assert_eq!(graph.total_nodes, 36);
        assert_eq!(graph.nodes.len(), 36);
        assert_eq!(graph.clusters.len(), 3);
        assert!(graph
            .nodes
            .windows(2)
            .all(|pair| pair[0].degree >= pair[1].degree));
        for (c, cluster) in graph.clusters.iter().enumerate() {
            assert_eq!(cluster.size, 12);
            assert_eq!(cluster.tone, i32::try_from(c).expect("small"));
            let hub = cluster.members[0];
            assert!(graph.nodes[hub].hub);
            assert_eq!(cluster.name, graph.nodes[hub].label);
            assert!(cluster.members.iter().all(|&i| graph.nodes[i].cluster == c));
            assert_eq!(
                cluster
                    .members
                    .iter()
                    .filter(|&&i| graph.nodes[i].hub)
                    .count(),
                1
            );
        }
    }

    #[test]
    fn links_join_kept_nodes_once_and_are_listed_both_ways() {
        let graph = Graph::parse(&sample(2, 8)).expect("graph");
        assert_eq!(graph.links.len(), graph.total_links);
        for &(a, b) in &graph.links {
            assert!(a < b && b < graph.nodes.len());
            assert!(graph.neighbours(a).contains(&b));
            assert!(graph.neighbours(b).contains(&a));
        }
        assert!(graph.neighbours(999).is_empty());
    }

    #[test]
    fn many_communities_share_an_other_group() {
        let graph = Graph::parse(&sample(13, 4)).expect("graph");
        assert_eq!(graph.clusters.len(), MAX_CLUSTERS + 1);
        let other = graph.clusters.last().expect("other");
        assert_eq!((other.name.as_str(), other.tone), ("other", -1));
        assert_eq!(other.members.len(), 16);
    }

    #[test]
    fn a_big_graph_is_cut_to_its_best_connected_nodes() {
        let graph = Graph::parse(&sample(4, 300)).expect("graph");
        assert_eq!(graph.total_nodes, 1200);
        assert_eq!(graph.nodes.len(), MAX_NODES);
        assert!(graph.links.len() <= MAX_LINKS);
        assert_eq!(graph.clusters[0].size, 300);
    }

    #[test]
    fn search_and_notes() {
        let graph = Graph::parse(&sample(2, 5)).expect("graph");
        let found = graph.search("NODE 1.", 10);
        assert_eq!(found.len(), 5);
        assert!(found
            .iter()
            .all(|&i| graph.nodes[i].label.starts_with("node 1.")));
        assert_eq!(graph.search("g0/f3", 10).len(), 1);
        assert_eq!(graph.search("", 3).len(), 3);
        assert!(graph.search("nothing like it", 10).is_empty());
        let hub = graph.clusters[0].members[0];
        assert_eq!(graph.note(hub), "4 links · src/g0/f0.rs");
        assert_eq!(graph.note(999), "");
    }

    #[test]
    fn what_is_not_a_graph_is_none() {
        assert!(Graph::parse("not json").is_none());
        assert!(Graph::parse(r#"{"nodes":[]}"#).is_none());
        assert!(Graph::parse(r#"{"links":[]}"#).is_none());
        let lone = Graph::parse(r#"{"nodes":[{"id":"a"}]}"#).expect("one node");
        assert_eq!(
            (lone.nodes[0].label.as_str(), lone.clusters[0].name.as_str()),
            ("a", "other")
        );
    }
}
