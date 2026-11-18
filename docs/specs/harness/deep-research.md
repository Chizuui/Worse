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
  (`harness-delegate`), verifies claims across sources, writes `research/<slug>.md` in the workspace and
  replies with a summary, key findings with citations and the report path.
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
| `web_search` | `query`, `max_results` (≤20) | `TAVILY_API_KEY` → Tavily; else `BRAVE_SEARCH_API_KEY` → Brave Search API; else DuckDuckGo HTML (no key) |
| `web_fetch` | `url`, `offset` | GET http/https, 4 MB cap, HTML → text, 20k chars per call with a continue offset |

Keys are read from the environment only and sent only in request headers. DuckDuckGo is blocked on some
networks (Indonesia); the error then tells the model to use a key or another agent.

## Acceptance

- Toggle on → sent turn context contains `{"type":"research"}`; toggle off → it does not.
- The agent receives the `<deep-research>` block; search/fetch tool calls appear as `fetch` tool cards.
- `web_tests.rs`: result parsing and redirect decoding, HTML to text, URL encoding round trip.
- `research.rs` test: wire format matches the phone client.
