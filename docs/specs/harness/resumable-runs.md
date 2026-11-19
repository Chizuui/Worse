# Resumable runs

Status: implemented (plan item 3), except §5's `SubagentStatus::Resumable` (deferred, see §8).
Owner: `crates/harness-agent-hub` — `journal.rs` (tables + queries), `resume.rs` (tracker, prompt,
RPC entry points), `store.rs` (opens the connection, migrates, marks orphans).

## 1. Problem

When the hub stops in the middle of a long task (crash, update, user restart) `Store::open` rewrote
every `queued`/`running`/`paused` turn to `interrupted`, and `Registry::load` (`delegate.rs`) rewrote
every active subagent to `interrupted`. The work done so far was still in the transcript, but the
user had to retype the request and the agent had to redo every tool call.

## 2. What a step is

A **run** is one turn the hub started for a user message. It is identified by its `turn_id`
(`runs.turn_id`) and belongs to one `session_id`. A run holds an ordered list of **steps**:

| Step state | Written when | Meaning |
| --- | --- | --- |
| `tool_call` | a tool call appears in the event stream (`EventKind::ToolCall` / `ToolCallUpdated`, status not terminal) | the side effect is about to happen |
| `tool_result` | the same tool call reports `completed` / `failed` | the side effect happened, result recorded |

The run row itself carries the lifecycle: `running` (turn started) → `finished` (turn finished) /
`resumable` (hub died first) → `discarded` (user dismissed) / `resumed` (resume turn started).

Steps are appended in event order; `idx` is monotonic per run (`MAX(idx)+1` inside one statement on
the single store connection). A step row is written for both the call and its result, so the journal
keeps the input of a call whose result never came.

A run is **resumable** when it has at least one `tool_result` step and never finished. A run without
a finished tool call is closed as `finished` instead of offered: nothing was executed, so re-sending
the message costs nothing and a resume banner would be noise.

## 3. Journal schema (additive)

```sql
CREATE TABLE IF NOT EXISTS runs(
  turn_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, agent TEXT NOT NULL,
  user_text TEXT NOT NULL, state TEXT NOT NULL,
  started_at INTEGER NOT NULL, finished_at INTEGER);
CREATE INDEX IF NOT EXISTS runs_session ON runs(session_id, state);
CREATE TABLE IF NOT EXISTS run_steps(
  turn_id TEXT NOT NULL, idx INTEGER NOT NULL, state TEXT NOT NULL,
  tool_id TEXT, tool_title TEXT, tool_input TEXT, tool_output TEXT, at INTEGER NOT NULL,
  PRIMARY KEY(turn_id, idx));
```

Rules:

- Only `store.rs` opens a connection; `journal.rs` reads and writes these tables and is called from
  `store.run(...)` like every other write. No second writer, no second connection.
- `journal::migrate` runs in `Store::open` right after `store_chat::migrate`; migrations are additive
  (`CREATE TABLE IF NOT EXISTS`), so an older database upgrades in place and `SCHEMA_VERSION` stays 1.
- Tool text is capped at 4000 chars per field (`journal::TEXT_CAP`) so the journal cannot grow
  without bound; the resume prompt clips further (§4).
- `journal::mark_orphans` runs in `Store::open` after the turn-status sweep: a `running` run with a
  `tool_result` becomes `resumable`, one without becomes `finished`. The turn keeps `interrupted`:
  the transcript status stays the historical truth, the journal is what makes a run resumable.
- A journal write failure is logged (`tracing::warn!`) and never fails the turn.

## 4. Where the journal is fed from, and how resume works

The tracker (`resume::track`) subscribes to the hub event bus, exactly like `delegate::track`, and
mirrors `TurnStarted` (open the run; the user text is read from the `turns` row the actor persisted
just before emitting), `ToolCall` / `ToolCallUpdated` (append a step) and `TurnFinished` (close the
run). The session actor is therefore untouched: it stays the only writer of the transcript, queue and
events, and the journal is a projection of the events it already emits.

```
SessionActor ──emit──> EventBus ──broadcast──> resume::track ──store.run──> journal (runs, run_steps)
                              └──> UI event stream
```

`resume_run(session_id)` starts a **new turn** in the same session whose message is the original user
text plus a `<harness-resume>` block listing the finished steps:

```
fix the build

<harness-resume run="turn-abc" finished-steps="2">
Steps already executed before the interruption (do not repeat them, continue after step 7):
6. read Cargo.toml -> ok
7. execute cargo check -> 3 errors in ...
</harness-resume>
```

- **API agent** (`harness-api-agent`) resumes fully: the conversation the hub owns carries the
  recorded tool results, and its own tool loop is unchanged.
- **ACP CLIs** (Claude Code, Codex, agy, …) resume through the same new turn. The CLI's own session
  resume (`agent_handles`, already persisted) re-attaches the conversation; the `<harness-resume>`
  block tells the model which calls are done. A CLI without session resume starts cold but still gets
  the block, so it does not blindly repeat finished work.
- Only the last 40 finished steps are listed (the newest ones), each clipped to 300 chars, so a very
  long run cannot blow the prompt.
- The resume turn is a normal `SessionCmd::Send` with the agent of the interrupted run, so the actor
  stays the only writer. No replay path writes turns directly.

## 5. Subagent tree

Runs are keyed by session, so a child session has its own journal and `resume_run(child_id)` works
on it directly. The parent link lives in the `subagents` table and is never deleted, so resuming a
child keeps the parent/child relation intact and the resumed result still reaches the parent through
`delegate_server::push`.

Deferred: turning active subagents into a new `SubagentStatus::Resumable` on load instead of
`Interrupted`, and the corresponding UI label. That needs `delegate.rs` / `types.rs` and the Slint
turn mapping, which are owned by the scoped-subagent-permissions change (plan item 8) in the same
working tree. Until then an interrupted subagent is shown as `Interrupted` while its run is still
offered by `resumable_run(child_id)`, which is the part that makes the work recoverable.

## 6. API and UI

- `ResumableRun { sessionId, turnId, agent, userText, finishedSteps, lastStep, startedAt,
  finishedAt, resumed }` (API 0.23.0, `harness-core-api/src/types.rs`).
- Three additive RPCs on `HarnessCore` (all with `unsupported()` defaults, so a non-hub
  implementation keeps compiling):
  - `resumable_run(session_id) -> Option<ResumableRun>` — the offer, or `None`.
  - `resume_run(session_id) -> SendAccepted` — starts the resume turn (`NotFound` when there is no
    offer).
  - `discard_run(session_id) -> ()` — dismisses the offer (`resumable` → `discarded`); rows stay for
    diagnostics.
- `discard_run` and `resume_run` emit `EventKind::SessionsChanged` for the session, so the session
  list and open chat refresh without a new event kind.
- The offer is read on demand rather than carried in `SessionDetail`: the snapshot is built inside
  the session actor, and a second copy in `SessionDetail` would be a second source for the same fact.
- UI: a banner in the chat dock ("Run interrupted after N steps" + `Resume run` / `Discard`). It
  calls `resumable_run`/`resume_run`/`discard_run` through the existing command channel; no new
  write path.

## 7. Acceptance scenarios

1. **Forced kill during a tool** — a run with two finished tool calls and no finish: `Store::open`
   makes the run `resumable`, the turn stays `interrupted`, `resumable_run` reports 2 steps,
   `resume_run` sends one new turn whose message contains both steps, the model is told not to
   repeat them, and the run becomes `resumed` (still offered, marked `resumed`).
2. **Clean finish** — a turn that ends normally sets `finished`; no offer, `resumable_run` is `None`.
3. **Discard** — `discard_run` clears the offer (`discarded`); `resumable_run` is `None`; journal rows
   stay.
4. **Nothing executed** — a run killed before any tool result is closed as `finished`, not offered.
5. **Empty/old database** — a database without the tables upgrades on open; no offers exist and every
   existing behaviour (turn history, queue, subagents) is unchanged.
6. **Journal off-path failure** — a journal write error is logged and never fails the turn.

Covered by unit tests: `journal::tests::*` (5, SQLite in-memory) and `resume::tests::*` (3, prompt
shape and tail clipping).

## 8. Not done here

- `SubagentStatus::Resumable` and its UI mapping (§5).
- The chat-dock banner and the task-board action (§6): the RPCs are in place, the Slint views are not
  touched by this change.
- A release-build manual kill/restart check (plan 3.7).
