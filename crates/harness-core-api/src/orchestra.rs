//! Orchestra agent graph (API 0.10.0, spec `phase-9-parity.md` §9.11).

use serde::{Deserialize, Serialize};

use crate::types::AgentId;

/// Role of one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum OrchestraRole {
    Orchestrator,
    #[default]
    Subagent,
    Reviewer,
    Planner,
}

impl OrchestraRole {
    /// Name shown in the UI and given to the agent.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Orchestrator => "Orchestrator",
            Self::Subagent => "Subagent",
            Self::Reviewer => "QC / Reviewer",
            Self::Planner => "Planner / Researcher",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrchestraNode {
    pub id: String,
    pub agent: AgentId,
    #[serde(default)]
    pub role: OrchestraRole,
    #[serde(default)]
    pub x: f32,
    #[serde(default)]
    pub y: f32,
    #[serde(default)]
    pub instructions: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrchestraEdge {
    pub from: String,
    pub to: String,
}

/// The whole graph; empty = Orchestra off.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Orchestra {
    #[serde(default)]
    pub nodes: Vec<OrchestraNode>,
    #[serde(default)]
    pub edges: Vec<OrchestraEdge>,
}

/// Largest graph.
pub const MAX_ORCHESTRA_NODES: usize = 32;
/// Longest instructions of one node.
pub const MAX_ORCHESTRA_INSTRUCTIONS: usize = 4_000;

impl Orchestra {
    /// Checks the §9.11 rules; the error text is user-facing.
    ///
    /// # Errors
    /// The first broken rule.
    pub fn validate(&self) -> Result<(), String> {
        if self.nodes.len() > MAX_ORCHESTRA_NODES {
            return Err(format!("at most {MAX_ORCHESTRA_NODES} agents"));
        }
        let mut orchestrators = 0;
        for (i, node) in self.nodes.iter().enumerate() {
            if node.id.trim().is_empty() {
                return Err("a node has no id".into());
            }
            if self.nodes[..i].iter().any(|n| n.id == node.id) {
                return Err(format!("duplicate node id {}", node.id));
            }
            if self.nodes[..i].iter().any(|n| n.agent == node.agent) {
                return Err(format!("{} is on the canvas twice", node.agent));
            }
            if node.instructions.chars().count() > MAX_ORCHESTRA_INSTRUCTIONS {
                return Err(format!(
                    "instructions are limited to {MAX_ORCHESTRA_INSTRUCTIONS} characters"
                ));
            }
            if !node.x.is_finite() || !node.y.is_finite() {
                return Err("a node has an invalid position".into());
            }
            if node.role == OrchestraRole::Orchestrator {
                orchestrators += 1;
            }
        }
        if orchestrators > 1 {
            return Err("only one agent can be the orchestrator".into());
        }
        for (i, edge) in self.edges.iter().enumerate() {
            if edge.from == edge.to {
                return Err("an agent cannot connect to itself".into());
            }
            let known = |id: &str| self.nodes.iter().any(|n| n.id == id);
            if !known(&edge.from) || !known(&edge.to) {
                return Err("a connection points to a missing agent".into());
            }
            if self.edges[..i].iter().any(|e| {
                (e.from == edge.from && e.to == edge.to) || (e.from == edge.to && e.to == edge.from)
            }) {
                return Err("these agents are already connected".into());
            }
        }
        Ok(())
    }

    /// The orchestrator node, if any.
    #[must_use]
    pub fn orchestrator(&self) -> Option<&OrchestraNode> {
        self.nodes
            .iter()
            .find(|n| n.role == OrchestraRole::Orchestrator)
    }

    /// Nodes reachable from the orchestrator over edges in either direction (orchestrator excluded), in node order.
    #[must_use]
    pub fn team(&self) -> Vec<&OrchestraNode> {
        let Some(root) = self.orchestrator() else {
            return Vec::new();
        };
        let mut seen: Vec<&str> = vec![root.id.as_str()];
        let mut frontier: Vec<&str> = vec![root.id.as_str()];
        while let Some(current) = frontier.pop() {
            for edge in &self.edges {
                let next = if edge.from == current {
                    edge.to.as_str()
                } else if edge.to == current {
                    edge.from.as_str()
                } else {
                    continue;
                };
                if !seen.contains(&next) {
                    seen.push(next);
                    frontier.push(next);
                }
            }
        }
        self.nodes
            .iter()
            .filter(|n| n.id != root.id && seen.contains(&n.id.as_str()))
            .collect()
    }
}
