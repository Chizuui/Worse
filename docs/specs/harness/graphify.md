# Graphify support

Status: implemented on `liquid-glass-ui`. Code: `crates/harness-agent-hub/src/graphify.rs`, the hooks in
`hub.rs` (`mcp_servers`) and `session.rs` (`graph_block`), the page in `apps/harness-slint/src/settings/graph.rs`.

[Graphify](https://github.com/Graphify-Labs/graphify) (Apache-2.0, Python, PyPI package `graphifyy`) turns a
project into a knowledge graph: files and symbols as nodes, relations as links, grouped into communities, in
`<project>/graphify-out/` (`graph.json`, `GRAPH_REPORT.md`, `graph.html`). An agent can ask the graph how the
code fits together instead of grepping.

## What "built in" means here

Graphify is not bundled into Harness: it is a Python package with tree-sitter grammars, and Harness is one
Rust executable. Harness uses the tool when it is installed, and offers to install it.

| Found | What Harness does |
| --- | --- |
| `<project>/graphify-out/graph.json` | a fresh agent session is told the graph exists and how to ask it (`<harness-graph>` block) |
| also `graphify-mcp` | its MCP server is attached to every session of the project: `graphify-mcp <graph.json>` over stdio, named `graphify` (tools `query_graph`, `get_node`, `get_neighbors`, `shortest_path`, `god_nodes`, `graph_stats`, …) |
| `graphify` | Settings › Memory › Graph can build and refresh the graph |

- **Build.** `graphify update <project>` with the project as working directory. This is Graphify's code-only
  pass: tree-sitter on this computer, no model, no API key. The page shows the command's last lines while it
  runs, then the graph's size (nodes, links, communities) and age. "Open graph" opens `graph.html` in the
  browser, "Open report" opens `GRAPH_REPORT.md`.
- **Install.** When `graphify` is missing, the page shows `uv tool install "graphifyy[mcp]"` and, when `uv` or
  `pipx` is installed, a button that runs it (`pipx install "graphifyy[mcp]" --force` for pipx). The `mcp`
  extra brings what `graphify-mcp` needs.
- **Finding the programs.** `PATH`, then `~/.local/bin` (where `uv tool` and `pipx` put launchers, often not
  on the PATH of a desktop session). `HARNESS_GRAPHIFY` and `HARNESS_GRAPHIFY_MCP` name the programs
  explicitly; an override that names a missing file means "not installed".
- **Off switch.** "Give agents the project graph" on the same page (`graph` in `<home>/memory/config.json`,
  read each time). A server the user already configured under the name `graphify` in the MCP page is kept
  and not doubled.

Not done, on purpose: `graphify install`. It writes a skill, a `CLAUDE.md` / `AGENTS.md` section and Claude
Code hooks into the user's agent settings. Harness's own prompt block carries the same guidance for every
agent without touching those files. Also not done: the semantic pass over documents, papers and images (it
needs a model key and is driven from an agent with `/graphify`), and refreshing the graph automatically
after a turn (the note tells the agent to run `graphify update .` after it changes code).

## Verification

- Unit tests: `graphify::tests` (graph statistics from node-link JSON with `links` or `edges`, the note for
  each combination of installed programs, the paths), `settings::graph::tests` (the summary line).
- In the app under Xvfb with **stand-ins** for `graphify` and `graphify-mcp` (scripts that write a graph in
  Graphify's layout): the Graph tab finds the tool, "Build graph" runs it and shows its output and
  "42 nodes · 96 links · 3 communities", and a new session is started with the `graphify` server attached
  and both notes in its first prompt.

Not verified: the real Graphify. It could not be installed in the environment this was written in, so the
commands and file layout follow its source (version 0.9.112: `cli.py` `update`, `watch.py` `_rebuild_code`,
`serve.py` `_main`, `pyproject.toml` scripts) and were not run. In particular: the output of a real
`graphify update` on a large project, how long it takes, and the real MCP server answering an agent.
