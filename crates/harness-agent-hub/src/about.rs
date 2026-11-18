//! What a fresh agent session is told about Harness itself.
//!
//! An agent gets Harness's tools through MCP servers, but nothing said what "Harness" is: asked "what can you
//! do in this harness?", a model went looking through the application's files instead of naming its tools.
//! This block names the servers attached to the session and what each is for.

/// Which optional servers this session has.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Attached {
    /// `harness-delegate` with the `delegate` tool (top-level sessions).
    pub delegate: bool,
    /// `harness-delegate` as a subagent: it can only report to its parent.
    pub child: bool,
    /// `harness-computer` (the shipped app).
    pub computer: bool,
    /// The project's Graphify graph server.
    pub graph: bool,
}

/// The block for the first prompt of a fresh agent session.
#[must_use]
pub fn block(attached: Attached) -> String {
    let mut text = String::from(
        "<harness>\nYou are running inside Worse Harness, a desktop app that runs coding agents side by \
side; another agent may continue this session later. Besides your own tools, Harness gives you these \
through MCP servers:\n\
- harness-memory: `memory_search`, `memory_add`, `memory_forget`, `memory_recent`, `memory_session` (a \
local memory shared by every agent and session), `memory_read` / `memory_write` (the MEMORY.md documents), \
`session_notes_read` / `session_notes_write` (working notes of this session, handed to whoever continues \
it).\n",
    );
    if attached.delegate {
        text.push_str(
            "- harness-delegate: `list_agents`, `delegate`, `subagent_wait`, `subagent_message`: give a \
sub-task to another agent or model as a subagent and collect its result; `claim_paths` / `release_paths` \
keep parallel subagents off the same files.\n",
        );
    } else if attached.child {
        text.push_str(
            "- harness-delegate: you are a subagent here. `notify_parent` reports to the session that \
started you; `claim_paths` / `release_paths` reserve the files you edit.\n",
        );
    }
    if attached.computer {
        text.push_str(
            "- harness-computer: `screenshot`, `click`, `type`, `key`, `scroll`, `drag`, `windows`: see \
and control this computer. It works only while the session's mode is Machine.\n",
        );
    }
    if attached.graph {
        text.push_str(
            "- graphify: `query_graph`, `get_node`, `get_neighbors`, `shortest_path`, `god_nodes`: the \
knowledge graph of this project.\n",
        );
    }
    text.push_str(
        "When the user asks what you can do in Harness or which tools you have, answer from this list \
and your own tool list; do not inspect the application's files to find out.\n</harness>",
    );
    text
}

#[cfg(test)]
mod tests {
    use super::{block, Attached};

    #[test]
    fn names_only_the_servers_the_session_has() {
        let all = block(Attached {
            delegate: true,
            child: false,
            computer: true,
            graph: true,
        });
        assert!(all.starts_with("<harness>") && all.ends_with("</harness>"));
        for name in [
            "harness-memory",
            "`delegate`",
            "harness-computer",
            "graphify: `query_graph`",
        ] {
            assert!(all.contains(name), "{name}");
        }
        let bare = block(Attached::default());
        assert!(bare.contains("harness-memory"));
        for name in ["harness-delegate", "harness-computer", "graphify"] {
            assert!(!bare.contains(name), "{name}");
        }
        let child = block(Attached {
            child: true,
            ..Attached::default()
        });
        assert!(child.contains("you are a subagent") && !child.contains("`delegate`,"));
    }
}
