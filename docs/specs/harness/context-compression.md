# Context compression (API agent) and cross-agent memory

Modelled on [billion-context](https://github.com/ranxianglei/billion-context) (MIT).

## Product rules

- The Harness API agent never loses a message to compaction. Old ranges are *folded*: the model sees a
  summary, the originals stay in the session file and can be searched or restored.
- The model decides what to fold (it knows what is finished). Harness only nudges, and folds mechanically
  as a fallback when the context would overflow.
- When another agent (Claude Code, Codex, agy, another API provider) takes the session over, it gets
  enough context to continue: session notes plus a tiered catch-up of earlier turns.

## State owner

- `crates/harness-api-agent` session file `<HARNESS_PROVIDER_STATE>/sessions/<id>.json`:
  `messages` (all originals) and `context` (`base`, `nextId`, `nudgedAt`, `folds[]`).
- `compress.rs`: `Folds` (fold state, view, compress/decompress, auto-fit). `compress_tools.rs`: the four
  tools, search, status, nudge.
- Hub transcript (SQLite) stays the source for catch-up; `catchup.rs` renders it.

## Tools (model-facing)

| Tool | Args | Effect |
| --- | --- | --- |
| `context_compress` | `from`, `to` (`m12`), `summary` | fold `from..=to` into the summary; folds inside are merged |
| `context_decompress` | `id` (`f3`) | restore a fold, refused if it would overflow the budget |
| `context_search` | `query` | all-terms search over every message (folded too) and fold summaries |
| `context_status` | none | size, folds, compressible ranges with refs |

## Invariants

- Message refs `mN` are absolute (`base + index`) and never reused; fold ids `fN` are never reused.
- A fold never starts on a `tool` message and the message after it is never a `tool` message, so an
  assistant tool call always folds together with its results.
- The assistant message that issued the call (the newest message) can never be folded.
- A range that partially overlaps an existing fold is rejected.
- The request view replaces each fold by one `user` message `[Harness folded context fN: messages …]`.

## Budgets and nudges

- View budget 400k chars (~100k tokens). Before every model request, `fit` folds the oldest ranges with a
  mechanical digest (adjacent automatic folds merge, capped at 6k chars) while the view is over budget,
  keeping the newest 8 messages; only then are whole oldest turns dropped (also when originals exceed 8M
  chars).
- Nudge from ~40k tokens visible, then every ~30k tokens of growth: a note appended to the newest user or
  tool message listing the oldest compressible ranges, also asking to refresh session notes.
- `/compact` sent to the API agent becomes an instruction to fold every finished range.

## Cross-agent handoff

- Session notes (`harness-memory` MCP) are injected on fresh / catch-up / failover; the nudge and
  `/compact` prompt ask the model to refresh them.
- `catchup.rs::render_history` uses three tiers within its 120k-char budget: newest turns verbatim (up to
  60%, or all of it for the newest turn alone), then condensed turns (user request 600 chars, head and
  tail of the answer 1.5k chars, up to 12 tool titles) up to 85%, then one-line turns, then a count of
  omitted turns.

## Acceptance

- Fold → view shows one summary message; decompress → view equals the originals (`compress_tests.rs`).
- Ranges that split a call from its result, or cut a fold, are rejected with a hint.
- Overflowing history is folded, not dropped; search still finds the folded text.
- A nudge fires once per growth step.
- Catch-up keeps the newest turn verbatim and condensed older turns with tool titles within budget.
