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
- **Install.** When `graphify` is missing, the page offers "Set up Graphify": one button that installs it
  with what the computer has and then builds the graph. In order of preference: `uv tool install
  "graphifyy[mcp]"`; `pipx install "graphifyy[mcp]" --force`; on Windows without either, `winget install
  --id astral-sh.uv` and then the `uv` command (uv brings its own Python, so nothing else is needed); last,
  `python -m pip install --user "graphifyy[mcp]"`. The page says which of these it will run before the button
  is pressed, and shows the commands and their output while they run. The `mcp` extra brings what
  `graphify-mcp` needs. When nothing on the computer can install it, the page says so and shows the command
  to run by hand.
- **Finding the programs.** `PATH`, then the folders installers put launchers in, which are often not on the
  PATH of a desktop session or of an app that was already running: `~/.local/bin` (uv, pipx), `~/.cargo/bin`,
  and on Windows `%APPDATA%\Python\*\Scripts` (pip `--user`) and WinGet's links folder. `HARNESS_GRAPHIFY`,
  `HARNESS_GRAPHIFY_MCP` and `HARNESS_UV` name the programs explicitly; an override that names a missing file
  means "not installed".
- **Off switch.** "Give agents the project graph" on the same page (`graph` in `<home>/memory/config.json`,
  read each time). A server the user already configured under the name `graphify` in the MCP page is kept
  and not doubled.

- **Picture.** Below the card the page draws the graph itself, in one of two styles, and "Fill the window"
  shows it over the whole window. It is drawn on the CPU by `crates/harness-graph-view`, frame by frame on a
  thread of its own (`apps/harness-slint/src/settings/graph_view.rs`); names and controls are laid over the
  frames by `GraphCanvas`. The up to 700 best connected nodes are shown, with the links among them.
  - **Simple** (`orbit.rs`): a flat solar system in the theme's colours. The project is the sun, every
    community a planet (its best connected node) with its other nodes as moons on rings around it. Small
    communities circle close to the sun; when there are many, two or three share an orbit. Orbits are spaced
    by the sizes of what travels on them, so systems never touch. At most 260 bodies.
  - **Galaxy** (`space.rs`, `hole.rs`): every node a star, every community a patch of sky, around a black
    hole, seen through a camera that circles it. Stars are placed evenly through a slightly flattened ball;
    each community grows from its hub, a node taking the free place nearest the node it was reached from, so
    linked stars stand close. Links between near stars are drawn as faint lines, brightest at the better
    connected end. The hole is shaded pixel by pixel: a disc of gas drawn from a pattern of thousands of
    thin strands, the far side of the disc lifted into view by bending the eye's rays (`0.35 / distance`
    radians at the point nearest the hole), a band of thin rings around the shadow, and a bloom that spreads
    the light, veils the shadow and burns out to white where the gas is hottest. Stars, dust and the grains
    that circle the hole are pushed outwards around it by the same bend when they pass behind it. The disc's
    pattern is kept at several finenesses and drawn from the one whose steps are about a pixel wide.
  - Both: drag turns the galaxy (slides the solar system); Ctrl + wheel and the buttons zoom (the plain
    wheel scrolls the page; over the whole window it zooms). Click a community's name to look at it alone;
    click a node, or pick it from "Nodes" (a list with a search box), to light up its links and see its file
    and what it is linked to. The pause button stops the clock.
  - Cost: nothing is drawn while the picture is off screen, the window is hidden, or (once it has settled)
    while it is paused, the window is in the background or reduced motion is on. A frame is at most 1.5
    million pixels; a larger canvas is drawn smaller and stretched. One frame is on its way to the window
    at a time, so a slow computer shows fewer frames instead of falling behind. The hole's picture is reused
    for a frame (two when it is large) while it stays in place.
  - The style and the pause are kept in `<home>/memory/graph-view.json`. "Open graph" still opens Graphify's
    own `graph.html` with everything in it.

Not done, on purpose: `graphify install`. It writes a skill, a `CLAUDE.md` / `AGENTS.md` section and Claude
Code hooks into the user's agent settings. Harness's own prompt block carries the same guidance for every
agent without touching those files. Also not done: the semantic pass over documents, papers and images (it
needs a model key and is driven from an agent with `/graphify`), and refreshing the graph automatically
after a turn (the note tells the agent to run `graphify update .` after it changes code).

## Verification

- Unit tests: `graphify::tests` (graph statistics from node-link JSON with `links` or `edges`, the note for
  each combination of installed programs, the paths), `settings::graph::tests` (the summary line, what each
  installer runs), `settings::graph_view::tests` (the list rows, the kept preferences).
  `harness-graph-view` (42 tests): the graph model (best connected nodes kept, communities, search), the
  camera, the shapes (a dot keeps its light wherever it lands, lines, painting over), the hole (the shadow
  is veiled, the far side rings it, the bar, the patterns and their levels), the galaxy (every node a star
  clear of the hole, linked stars closer than random ones, the same graph is the same sky, light bent around
  the hole, focus and selection), the solar system (systems never touch for 1 to 10 communities at three
  moments, everything fits the frame and can be picked) and the engine (click, drag, pause and rest, focus
  follows a planet, a panel moves the middle over, a huge canvas is drawn smaller).
- In the app under Xvfb with **stand-ins** for `graphify`, `graphify-mcp` and `uv` (scripts that write a
  graph in Graphify's layout), with a 300-node graph of six communities:
  - without Graphify the page shows what "Set up Graphify" will run; pressing it ran the stand-in `uv`,
    found the new launchers in `~/.local/bin`, built the graph and showed the picture;
  - the galaxy: drawn with names, the tooltip of the node under the pointer, a click on a name (the others
    dim, the camera turns to it, "←" leads back), a click on a node (lines to what it is linked to, the panel
    with its file and its links), the list of nodes, Ctrl-free wheel zoom and a drag over the whole window;
  - the solar system: drawn in the theme's colours, in the page and over the whole window.
- The look of the galaxy was matched against frames of a reference video side by side at the same scale
  (the hole edge-on, tilted and from above; stars and lines; smoke). It is close, not identical: the far
  side of the disc is lifted by a simpler bend than a real black hole's, and the band of rings is drawn, not
  traced.
- Speed, measured in this environment (4 shared cores, no GPU): a 1114×690 galaxy frame with 384 stars takes
  about 7 ms on average in a release build (about 11 ms at 1671×1035); the first frame about 0.1 s more,
  once, to lay out the sky and weave the disc's pattern. In a debug build of the app under Xvfb the page
  showed 15 to 20 frames a second.

Not verified: the real Graphify. It could not be installed in the environment this was written in, so the
commands and file layout follow its source (version 0.9.112: `cli.py` `update`, `watch.py` `_rebuild_code`,
`serve.py` `_main`, `pyproject.toml` scripts) and were not run. In particular: the output of a real
`graphify update` on a large project, how long it takes, the real MCP server answering an agent, and the
`winget` and `pip` ways of installing. Also not verified: anything on Windows or macOS (frame rate there,
high-DPI screens), and graphs of several thousand nodes.
