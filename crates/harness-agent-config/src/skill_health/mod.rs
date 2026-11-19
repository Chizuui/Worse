//! Skill trigger health check: does an installed skill trigger when it should, and stay quiet when it should not (spec `docs/specs/harness/skill-health.md`).
//!
//! [`check`] is a pure function over the installed skill list. [`LiveCheck`] only builds a `reach eval`
//! command: Harness never starts it.

mod live;
mod read;
mod rules;
#[cfg(test)]
mod tests;

pub use live::{reach_agent, LiveCheck};
pub use read::{report_for_dir, report_for_scope, skills_dir};
pub use rules::{
    check, check_with, Finding, FindingCode, Grade, InstalledSkill, Report, Severity, SkillHealth,
    Thresholds,
};
