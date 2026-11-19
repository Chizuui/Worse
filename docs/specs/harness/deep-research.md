# Deep research mode

Pattern borrowed from [PaperOrchestra](https://github.com/google-research/paper-orchestra) (Apache-2.0,
ideas only, no code): plan, research each part, write, then verify.

## Product rules

- One toggle per session, "Deep research", in the composer `+` menu and the mode menu (desktop: `+` menu, Mode picker and the
  collapsed Options menu; phone: model/options sheet). The mode button shows `<mode> · Research` in the
  brand colour while it is on, and a removable "Deep research" pill sits next to `+`. Turned on in a new chat,
  it applies to the session created by the first message.
- While on, every message sent from the composer (Send, Steer, the first message of a new session) carries
  `ContextItem::Research`. The hub turns it into the research instructions for whichever agent runs the
  turn, so it works with Claude Code, Codex, agy and API providers, and survives failover.
- The agent plans sub-questions, searches and reads sources, may delegate sub-questions to subagents
  (`harness-delegate`), verifies claims across sources, writes the report as a LaTeX paper in `research/<slug>/`
  (`report.tex` + `references.bib`, natbib/plainnat; abstract, introduction with method, one section per
  sub-question, discussion, confidence and limitations, conclusion), compiles `report.pdf` when
  tectonic/latexmk/pdflatex is on PATH (never installs one), and replies with a summary, key findings with citations
  and the file paths. PaperOrchestra's output is a LaTeX paper, and the user asked for the same instead of Markdown
  (2026-11-18). Without file access the report goes in the reply as Markdown.
- No web access = the agent says so and stops; no answers from memory unless the user asks.

## State owner

- Toggle: UI-held. Desktop `DockState.research` (`apps/harness-slint/src/bridge/dock.rs`, lost on restart);
  phone `state.research` persisted in `localStorage` (`harness.research`). The hub keeps no mode; the item is
  stored with each turn's context like any attachment.
- Instructions: `crates/harness-agent-hub/src/research.rs` (`INSTRUCTIONS`, `LABEL`), rendered in
  `context.rs::blocks`.
- Wire format: `{"type":"research"}` (`harness-core-api` `ContextItem::Research`, API 0.21.0).

## API agent web tools (`crates/harness-api-agent/src/web.rs`, `web_text.rs`)

| Tool | Args | Backend |
| --- | --- | --- |
| `web_search` | `query`, `max_results` (≤20) | `TAVILY_API_KEY` → Tavily; else `BRAVE_SEARCH_API_KEY` → Brave Search API; else, keyless and in order: DuckDuckGo HTML, Brave Search web page, DuckDuckGo through the r.jina.ai reader. A backend with an error or no results falls through to the next; a refused DuckDuckGo connection is remembered for the rest of the agent process (`web_search.rs`) |
| `paper_search` | `query`, `max_results` (≤20), `year_from` | OpenAlex works search (no key); Crossref when OpenAlex fails or finds nothing. Title, year, authors, venue, citation count, DOI, open-access PDF (`papers.rs`) |
| `web_fetch` | `url`, `offset` | GET http/https, 4 MB cap, HTML → text, 20k chars per call with a continue offset. When the direct GET fails (blocked, HTTP error) or the body is not text (PDF), the page is read through `https://r.jina.ai/<url>` and the result says so |

Keys are read from the environment only and sent only in request headers. DuckDuckGo is blocked in Indonesia
(connection refused, checked 2026-11-18); Brave Search's page, OpenAlex, Crossref and r.jina.ai were reachable. The
r.jina.ai fallback sends the public URL being read (or the DuckDuckGo query) to Jina; it is keyless and rate limited.

## Figures

Pattern borrowed from [PaperVizAgent](https://github.com/google-research/papervizagent) (Apache-2.0, ideas only, no code):
plan → style → draw → critique, applied to a LaTeX report instead of an image model.

- **When a figure earns its place:** only when it carries information the sentences cannot — a pipeline or
  architecture diagram, a taxonomy of the compared approaches, a comparison across the sources read, a trend, a
  distribution. Two to six per full report, no decoration, every figure referenced from the body text as
  `Figure~\ref{fig:...}` and explained in one or two sentences. An unreferenced figure is removed.
- **File layout:** `research/<slug>/figures/`. Each figure keeps its generator next to its output:
  `fig-<name>.tex` (TikZ/PGFPlots), `fig-<name>.dot` (Graphviz) or `fig-<name>.py` (matplotlib), plus the
  rendered `fig-<name>.pdf`/`.svg`. `report.tex` embeds `\input{figures/fig-<name>}` for TikZ or
  `\includegraphics[width=\linewidth]{figures/fig-<name>}` for a rendered file, inside a `figure` block with
  `\caption` (content, takeaway, source citations, no "Figure 1:" prefix) and `\label{fig:<name>}`.
- **Allowed renderers, in order:** TikZ/PGFPlots written in the `.tex` (default, needs only the LaTeX engine);
  Graphviz `dot` for diagrams only when `dot` is already on PATH; Python matplotlib for data plots only when
  `python`/`python3` on PATH already imports matplotlib. Availability is checked once per run. Harness and the
  agent never install, upgrade or download a renderer, font or model. With no external tool the agent uses
  PGFPlots and pure TikZ, never ASCII art and never a figure it did not draw. `report.tex` adds only `graphicx`,
  `tikz` and `pgfplots` beyond the standard package list, and a drawing that would need an exotic package is
  simplified instead.
- **Loop:** plan (one line per figure: title, claim, type, data source, numbers and labels, caption) → style
  (aesthetic decisions only, content untouched) → draw (generator + `figure` block, render) → critique once
  (values match the sources, labels readable, nothing overlapping or clipped, legend and caption honest; fix and
  re-render once). A figure still wrong after that pass is deleted and reported, not shipped.
- **Style rules:** white background, no gradients, shadows, 3D bars/pies or clip art; one muted categorical
  palette plus viridis/magma sequential and coolwarm diverging scales, never rainbow/jet, at most six colours;
  colour always paired with marker shape, hatch or line style so the figure reads in greyscale; sans-serif
  labels at body size or one step smaller, never below 7 pt, axis labels with units; light grey dashed grids
  behind the data, markers on lines, dashed baselines, translucent bands for uncertainty, legends inside the
  plot or in one row above it, direct labels preferred; square heatmap cells with the value printed inside;
  diagrams with rounded boxes, one accent colour per role, at most about fifteen nodes, text as real LaTeX.
- **Honesty:** every number comes from a source the agent opened or work done in that run; disagreements are
  shown and labelled instead of averaged away. The reply names each figure, its renderer, any dropped figure and
  the file paths.
- Instructions live in `crates/harness-agent-hub/src/research_figures.rs` (`INSTRUCTIONS`, the
  `<deep-research-figures>` block), appended after `research.rs` `INSTRUCTIONS` by `context.rs::blocks` for the
  same `ContextItem::Research`, so failover and every agent CLI get the same text.

## Fact check

Pattern borrowed from [SAFE](https://github.com/google-deepmind/long-form-factuality) (code Apache-2.0,
dataset CC-BY-4.0; ideas only, no code copied): after the report is written, split it into atomic claims,
drop the ones that do not bear on the question, then search for each remaining claim and record a verdict.

- **When it runs:** only for research turns, after `report.tex` exists and before the final reply. It is on
  by default and can be switched off in Settings › General › Advanced › "Fact-check reports"
  (`research.factCheck`). The hub passes the flag to the agent as a prose line, so every agent CLI can skip the
  step; the API agent also gets a `fact_check` tool that does the mechanical part.
- **Inputs:** the research question and the report text (`report.tex`, or the Markdown report in the reply
  when the agent had no file access). The claim extractor (`extract` action) turns the prose into atomic,
  self-contained claims: one assertion per claim, no pronouns that point outside it, numbers and dates kept
  verbatim, each claim carrying the section and sentence index it came from.
- **Relevance filter:** claims that are definitions, method statements ("we searched OpenAlex"), opinions,
  or unrelated to the research question are dropped before any search; a claim that no search could settle
  is reported as `unclear`, never silently dropped.
- **Verification loop (per claim):** up to three `web_search`/`paper_search` queries (the claim text, then a
  keyword-reduced variant), snippets read first, `web_fetch` only for the sources that look decisive, then a
  verdict `supported` / `notSupported` / `unclear` with the source URL and a one-line reason. A claim is
  `supported` only when an opened source states it; a snippet that merely repeats the wording is not enough.
- **Cost limits:** at most 40 claims per report (the rest are reported as unchecked), at most 3 queries kept
  per claim, duplicates dropped by their normalised text, at most 120 searches for the whole step and at most
  4 claims verified at a time. The tool enforces the claim cap, the query cap and the deduplication; the search
  budget and the parallelism are instruction text, since the searches are the agent's own calls.
- **Outputs:** `research/<slug>/factcheck.json` (`question`, `report`, `checkedAt`, `summary` with
  `total`/`supported`/`notSupported`/`unclear`/`unchecked`, and one entry per claim with `text`, `location`,
  `verdict`, `reason`, `sources` (URL + title), `queries`), plus a `Fact-check` appendix table appended once
  to `report.tex` before `\end{document}` (claim, verdict, source).
- **Revision pass:** claims marked `notSupported` go back to the writer once (step 6 of the instructions) to
  be re-sourced, corrected or removed; a second failure leaves the claim flagged but shipped, and the reply
  names the flagged claims. The score never rises without a new source.
- **State owner:** the verdicts live in `factcheck.json` (written by the agent, one writer); the hub keeps
  only the instruction text (`crates/harness-agent-hub/src/factcheck.rs` `INSTRUCTIONS`, the
  `<deep-research-factcheck>` block) and the on/off flag in `HubConfig.fact_check`. The tool implementation
  (`crates/harness-api-agent/src/factcheck.rs`) is stateless: `extract` normalises and filters, `record`
  deduplicates, orders and scores, `write` renders the JSON and the LaTeX appendix from what it is given.
- **Failure:** no web access, or a tool error on every query, marks the affected claims `unclear` and the
  reply says the check was incomplete. A report that could not be read is not checked at all.

## Acceptance

- Toggle on → sent turn context contains `{"type":"research"}`; toggle off → it does not.
- The agent receives the `<deep-research>` block; search/fetch tool calls appear as `fetch` tool cards.
- `web_tests.rs`: DuckDuckGo and Brave result parsing, OpenAlex and Crossref parsing and rendering, HTML to text, URL
  encoding round trip.
- `research.rs` test: wire format matches the phone client.
- `research_figures.rs` tests: the `<deep-research-figures>` block names only local renderers, the `figures/`
  layout and the `figure`/`\label{fig:` block, and forbids installing a renderer (no `pip install`).
- `factcheck.rs` (hub) test: the `<deep-research-factcheck>` block names the JSON output, the verdicts and the
  claim cap, and is emitted only when the flag is on.
- `factcheck.rs` (API agent) tests: claim splitting keeps numbers, keeps decimals whole and drops method
  sentences, the relevance filter drops an unrelated claim, the claim cap holds, verdict dedupe requires a
  source for `supported`, the score counts each verdict, and the LaTeX appendix is escaped and appended once.
- Manual: one research turn with the option on writes `research/<slug>/factcheck.json` whose `summary.total`
  equals the number of claims and whose sources are URLs the run actually opened.
