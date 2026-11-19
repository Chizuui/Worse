# Skill trigger health check

Modelled on [skill-reach](https://github.com/google/skill-reach) (Apache-2.0): it evaluates the
*discovery* phase of a skill catalog — does an incoming request route to the right skill, and does the
skill stay quiet when it should not.

Item 7 of `plans_harness_worse.md`. Depends on item 1 (skill marketplace, `skill-marketplace.md`).
This file is the standalone home of the section that item 7.1 asked for in `skill-marketplace.md`; the
marketplace page links here instead of copying it.

## Product rules

- "Healthy" means two things at once: the skill triggers when it should, and it stays quiet when it
  should not. Both are decided by the skill's `description` front matter, because that is the only text
  a CLI router sees before it loads the skill body.
- A health report never changes a skill. It is advisory: findings say what to fix, the user edits the
  `SKILL.md` in the existing Settings editor.
- Two levels of checking:
  - **Static check** (always available, offline, no quota): description length, overlap between
    installed descriptions, missing trigger phrase. Pure function over the installed skill list.
  - **Live check** (opt-in, uses the user's quota): `reach eval` drives a real CLI so the router is
    measured instead of estimated. Harness only *builds* the command; the user starts it, and Settings
    must carry the flag in `skillHealth.liveCheck` (default off, network/quota use is never implicit).
- Findings are per skill so the Skills page can show a badge per row, plus one report grade for the
  page header and the "Check skills" button.

## State owner

- `crates/harness-agent-config/src/skill_health/` owns the rules: `Thresholds`, `Severity`,
  `FindingCode`, `Finding`, `SkillHealth`, `Report`, `check`/`check_with` (`rules.rs`), the CLI readers
  `report_for_dir` / `report_for_scope` (`read.rs`) and the live-check command builder (`live.rs`).
- The installed skill list keeps its current owner: `md_store::list_skills` per CLI skill dir
  (`clis.rs`: Claude Code, Codex, Harness overlay). The health module only reads it.
- Nothing is cached and no skill file is written. The report is recomputed on demand.
- The only persisted state is the opt-in live-check flag, and it lives in the shared settings tree:
  `skillHealth.liveCheck` (default `false`) and `skillHealth.binary` (default `reach`) on
  `harness_settings::SkillHealthSettings`. The Skills page toggles it through `SettingsUi.set-bool`, so the
  settings worker sees the value in its snapshot and there is still exactly one writer of `settings.json`.

## Static rules

| Code | Severity | Rule |
| --- | --- | --- |
| `missingDescription` | error | no `description` front matter |
| `descriptionTooShort` | warn | fewer than 40 characters |
| `descriptionTooLong` | warn | more than 500 characters |
| `noTriggerPhrase` | warn | no phrase that names the situation, e.g. `use when`, `use this`, `when the user`, `whenever`, `before …` |
| `overlappingDescription` | warn | content-word Jaccard similarity at least 0.5 with another installed skill and at least 6 shared words |

- Content words are lowercased alphanumeric tokens of 3+ characters with router stop words removed
  (`use`, `when`, `the`, `skill`, …), so "Use when the user …" does not make two skills look alike.
- Similarity is computed between distinct skill *names* only: the same skill installed for Claude Code,
  Codex and Harness is one entry, and its clients are listed on that entry.
- Score: 100 minus 15 per warning and 40 per error, floored at 0. Grade: `poor` when a skill has an
  error or the score is under 50, `warning` when it has a warning, `good` otherwise. The report grade is
  the worst skill grade.

## Live check (opt-in)

- `LiveCheck { enabled, binary, command, out }` is the only entry point. `build` returns `None`
  while `enabled` is false, so no caller can start a probe by accident.
- Agent ids map to the CLIs skill-reach knows: Claude Code → `claude-code`, agy → `antigravity-cli`.
  Codex and the Harness overlay have no skill-reach agent, so `build` returns `None` for them.
- Command shape (`<binary>` defaults to `reach`, resolved on `PATH`):
  - one skill: `eval <name> --skills <skill-dir> --agent <cli> --yes --format json`
  - whole catalog: `eval <skill-dir> --auto --agent <cli> --yes --format json`
- `LiveCheck::build` takes the flag and binary from `Settings::skill_health`; the settings worker reads its
  snapshot, so turning the toggle off removes the command immediately. `to_command_line` renders the command
  the user can copy; `skills_dir(paths, agent, scope)` names the directory it would run against.
- Harness never runs it in the app, in tests or in gates. The user starts it from the Skills page once
  the flag is on; output parsing is out of scope for item 7.

## Settings UI

- Skills page (`ui/settings/skills.slint`, `src/skills/mod.rs`): a **Check skills** button sends
  `Req::SkillsHealth` to the settings worker, which calls `report_for_scope` on the worker thread (the UI
  thread never touches the skill dirs). The report is pushed back to the Skills global.
- Each catalog row that is installed gets a badge (`Healthy` / `Needs work` / `Broken`) plus one line of
  findings; installed skills the catalog does not list get a row of their own (no group, no update check)
  so their badge is reachable. The header badge shows the worst grade and the report score.
- The check is on demand only: opening the page, installing or removing a skill never starts it, and the
  live check is never started by the app.
- A `Switch` row under the toolbar turns `skillHealth.liveCheck` on and re-runs the check; when the command
  exists the page shows the copyable `reach eval …` line under it. The row says it uses the user's quota.

## Acceptance

- A skill without a description gets `missingDescription` (error) and a `poor` grade.
- A short description without a trigger phrase gets both `descriptionTooShort` and `noTriggerPhrase`.
- Two skills sharing the same wording report `overlappingDescription` on both, naming the other skill.
- The same skill installed for three CLIs yields one entry with three clients and no overlap finding
  against itself.
- A clean catalog grades `good` with score 100.
- `LiveCheck::build` returns `None` when `skillHealth.liveCheck` is false (the default) and a command whose
  first token is the binary and whose args contain `--agent claude-code` when it is true. Nothing is spawned.
- `Settings::default().skill_health.live_check` is false; `{"skillHealth":{"liveCheck":true}}` parses to an
  enabled live check with binary `reach`.
- `report_for_scope` merges a skill that two CLIs have into one entry listing both clients.
- The Skills page shows no badge until **Check skills** is pressed; afterwards every installed row has one
  and the header shows the worst grade with the score.