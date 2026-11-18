# Decision model

Status: delegation routing implemented on `liquid-glass-ui`; the other uses below are a proposal. Code:
`crates/harness-agent-hub/src/decision.rs`, the hook in `delegate_server.rs` (`delegate`), the settings card in
`apps/harness-slint/src/settings/decision.rs`.

## 1. What it is

A **decision model** is a small, cheap, fast model that makes Harness's own choices, so the choices are no
longer a fixed rule and do not cost a turn of the expensive agent that is doing the work. It is one of the
user's API providers: any OpenAI-compatible Chat Completions endpoint. OpenRouter and Command Code's Provider
API both serve many models behind one key, which makes them a natural pick, with a small model.

One request per decision: a system line, the question, no tools, a JSON answer. `<home>/decision.json` holds
the choice (`provider`, `model`, `delegation`) and is read each time, so a change applies at once.

Whatever the model answers is checked against what Harness offered. No answer in 20 seconds, an HTTP error,
an answer that is not JSON, or an agent that was not offered: Harness decides by its rule, as before. A
decision model can make a choice worse; it cannot make one that was not allowed.

## 2. Implemented: who gets a delegated task

`delegate` (the `harness-delegate` tool an orchestrating agent calls) has an optional `agent`. Without it the
rule was: the first available Subagent of the Orchestra team, or Harness's default agent when no team
applies. With a decision model:

1. Harness lists the candidates: the available members of the Orchestra team with their role and the first
   line of their instructions, or every available agent when no team applies (the delegating agent is marked
   as such). Each with the models it offers, when it offers several.
2. It asks the decision model: the task (first 3,000 characters) and the candidates. The answer names an
   agent, optionally one of that agent's models, and a one-sentence reason.
3. The agent is used exactly as if the orchestrator had named it (so the Orchestra team check still applies).
   A named model is set on the new subagent session before its first turn.
4. The orchestrator's tool result ends with `[Picked by the decision model, model <m>: <reason>]`.

An orchestrator that names an agent itself is not overruled. A follow-up to an existing subagent
(`session_id`) is not routed.

Settings › Orchestra › **Decision model**: the provider, an optional model id (empty = the provider's own
model), and the switch for delegation.

## 3. What else a decision model can do in Harness

Not built. Listed by how much each would change, smallest first; each would get its own switch.

| Use | Today | With a decision model |
| --- | --- | --- |
| Pick the agent and model of a **new session** ("Auto" in the agent menu) | the user picks, or the last agent | the first request decides: a cheap model for a small question, a strong one for a refactor |
| **Failover target** when an agent hits its limit | next in the fallback order | the agent that suits the unfinished task among those available |
| **What to recall** from the local memory | word search, top 6 | rerank the 20 word-search hits by the request; fixes "expiry" not finding "expire" for the hits it sees |
| **Write memories** from a finished session | a digest cut from the text, no facts extracted | propose facts, decisions and preferences worth keeping (the user or the agent confirms) |
| **Session title and digest** | first request as title; digest cut from first/last text | a real title and a three-line summary |
| **Inbox triage** | a router agent session (`route_trigger`) | a single cheap request for the routine cases, the router agent only for the rest |
| **Permission triage** | ask, accept edits, or Machine mode | classify a command as routine or risky; ask only for the risky ones |
| **Model per turn** inside one API provider | fixed model | a fast model for short follow-ups, the strong one when the turn needs it |

Costs to keep in mind: every use adds a network round trip before something happens (bounded here by the
20-second limit), and sends the task text to the decision provider.

## 4. Verification

- Unit tests (`decision::tests`): the question lists agents, roles, notes and models; answers are checked
  against the candidates (prose around the JSON, a code fence, an agent or model named by display name, an
  unknown agent, an unknown model); the config; and the whole call against a local Chat Completions server
  (the key and question arrive, the answer becomes a choice, and the off / missing provider / unusable answer
  cases return no decision).
- In the app under Xvfb with scripted agents and a **stand-in** decision server: an orchestrating agent calls
  `delegate` without an agent. Without a decision model the task goes where the rule sends it. With one, the
  server is asked with the four candidates, its pick ("Reviewer") gets the task, and on the second delegation
  the question lists that agent's models and the picked model is set on the subagent session
  (`session/set_model` arrives at the agent).

Not verified: a real model as decision model (how well it picks, how long it takes), and Orchestra teams with
roles in the app run (the role and notes lines are covered by the unit test of the question).
