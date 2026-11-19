use std::path::Path;

use super::live::LiveCheck;
use super::read::{report_for_dir, report_for_scope};
use super::rules::{check, FindingCode, Grade, InstalledSkill, Report, Severity, SkillHealth};
use crate::types::AgentKind;

fn skill(name: &str, description: Option<&str>) -> InstalledSkill {
    InstalledSkill::new(name, "claude-code", description.map(str::to_owned))
}

fn entry<'a>(report: &'a Report, name: &str) -> &'a SkillHealth {
    match report.skill(name) {
        Some(found) => found,
        None => panic!("no health entry for {name}"),
    }
}

fn codes(health: &SkillHealth) -> Vec<FindingCode> {
    health.findings.iter().map(|f| f.code).collect()
}

fn build(enabled: bool, agent: AgentKind, dir: &Path, skill: Option<&str>) -> LiveCheck {
    match LiveCheck::build(enabled, "reach", agent, dir, skill) {
        Some(check) => check,
        None => panic!("expected a command"),
    }
}

#[test]
fn missing_description_is_an_error() {
    let report = check(&[skill("graph", None)]);
    let found = entry(&report, "graph");
    assert_eq!(found.findings.len(), 1);
    assert_eq!(found.findings[0].code, FindingCode::MissingDescription);
    assert_eq!(found.findings[0].severity, Severity::Error);
    assert_eq!(found.grade(), Grade::Poor);
    assert_eq!(report.grade(), Grade::Poor);
}

#[test]
fn short_description_without_trigger_phrase_reports_both() {
    let report = check(&[skill("graph", Some("Plots graphs."))]);
    let found = codes(entry(&report, "graph"));
    assert!(found.contains(&FindingCode::DescriptionTooShort));
    assert!(found.contains(&FindingCode::NoTriggerPhrase));
    assert_eq!(found.len(), 2);
}

#[test]
fn clean_catalog_is_good() {
    let report = check(&[skill(
        "graph",
        Some("Use when the user asks for a chart of numeric data in any language or format."),
    )]);
    let found = entry(&report, "graph");
    assert!(found.findings.is_empty(), "{:?}", found.findings);
    assert_eq!(found.score(), 100);
    assert_eq!(report.grade(), Grade::Good);
    assert_eq!(report.score(), 100);
}

#[test]
fn long_description_warns() {
    let text = format!("Use when {}.", "x".repeat(520));
    let report = check(&[skill("graph", Some(&text))]);
    assert_eq!(
        codes(entry(&report, "graph")),
        vec![FindingCode::DescriptionTooLong]
    );
}

#[test]
fn overlapping_descriptions_name_each_other() {
    let text =
        "Use when the user needs deployment of containers to kubernetes clusters with rollback";
    let report = check(&[skill("deploy", Some(text)), skill("release", Some(text))]);
    for (name, other) in [("deploy", "release"), ("release", "deploy")] {
        let found = entry(&report, name);
        let overlap = found
            .findings
            .iter()
            .find(|f| f.code == FindingCode::OverlappingDescription);
        match overlap {
            Some(finding) => assert!(finding.message.contains(other)),
            None => panic!("{name} has no overlap finding"),
        }
        assert_eq!(found.grade(), Grade::Warning);
    }
}

#[test]
fn same_skill_in_three_clis_is_one_entry() {
    let description = Some("Use when the user asks for a chart of numeric data.");
    let listed = [
        InstalledSkill::new("graph", "claude-code", description.map(str::to_owned)),
        InstalledSkill::new("graph", "codex", description.map(str::to_owned)),
        InstalledSkill::new("graph", "harness", description.map(str::to_owned)),
    ];
    let report = check(&listed);
    assert_eq!(report.skills.len(), 1);
    let found = entry(&report, "graph");
    assert_eq!(found.clients, vec!["claude-code", "codex", "harness"]);
    assert!(!codes(found).contains(&FindingCode::OverlappingDescription));
}

#[test]
fn live_check_needs_the_flag() {
    let dir = Path::new("/tmp/skills");
    assert!(LiveCheck::build(false, "reach", AgentKind::ClaudeCode, dir, Some("graph")).is_none());

    let single = build(true, AgentKind::ClaudeCode, dir, Some("graph"));
    assert_eq!(single.binary, "reach");
    assert_eq!(single.command[0], "eval");
    assert_eq!(single.command[1], "graph");
    assert!(single
        .command
        .windows(2)
        .any(|w| w == ["--agent", "claude-code"]));
    assert!(single.to_command_line().starts_with("reach eval graph"));

    let agy = build(true, AgentKind::Agy, dir, None);
    assert!(agy
        .command
        .windows(2)
        .any(|w| w == ["--agent", "antigravity-cli"]));
    assert!(agy.command.contains(&"--auto".to_owned()));
    assert!(LiveCheck::build(true, "reach", AgentKind::Codex, dir, None).is_none());
}

#[tokio::test]
async fn reads_a_skill_dir_from_disk() -> crate::Result<()> {
    let home = tempfile::tempdir()?;
    let skills = home.path().join("skills");
    std::fs::create_dir_all(skills.join("graph"))?;
    std::fs::write(
        skills.join("graph").join("SKILL.md"),
        "---\nname: graph\ndescription: Plots graphs.\n---\nBody\n",
    )?;
    std::fs::create_dir_all(skills.join("empty"))?;
    std::fs::write(skills.join("empty").join("SKILL.md"), "---\n---\nBody\n")?;
    let report = report_for_dir(&skills, "harness").await?;
    assert_eq!(report.skills.len(), 2);
    let graph = entry(&report, "graph");
    assert_eq!(graph.clients, vec!["harness"]);
    assert_eq!(graph.grade(), Grade::Warning);
    assert_eq!(entry(&report, "empty").grade(), Grade::Poor);
    Ok(())
}

#[tokio::test]
async fn live_check_stays_disabled_by_default() {
    let settings = harness_settings::Settings::default().skill_health;
    assert!(!settings.live_check);
    assert_eq!(settings.binary, "reach");
    assert!(LiveCheck::build(
        settings.live_check,
        &settings.binary,
        AgentKind::ClaudeCode,
        Path::new("/skills"),
        Some("graph"),
    )
    .is_none());
}

#[tokio::test]
async fn live_check_reads_the_settings_file_flag() -> crate::Result<()> {
    // 用户在设置里打开 skillHealth.liveCheck 后，命令才出现；关闭时 build 返回 None。
    let on: harness_settings::Settings =
        serde_json::from_str(r#"{"skillHealth":{"liveCheck":true,"binary":"reach"}}"#)
            .map_err(|e| crate::ConfigError::invalid(e.to_string()))?;
    assert!(on.skill_health.live_check);
    let Some(command) = LiveCheck::build(
        on.skill_health.live_check,
        &on.skill_health.binary,
        AgentKind::ClaudeCode,
        Path::new("/skills"),
        None,
    ) else {
        panic!("the enabled flag must build a command");
    };
    assert_eq!(command.binary, "reach");
    assert!(command
        .to_command_line()
        .starts_with("reach eval /skills --auto"));
    let off: harness_settings::Settings = serde_json::from_str(r#"{"skillHealth":{}}"#)
        .map_err(|e| crate::ConfigError::invalid(e.to_string()))?;
    assert!(LiveCheck::build(
        off.skill_health.live_check,
        &off.skill_health.binary,
        AgentKind::ClaudeCode,
        Path::new("/skills"),
        None,
    )
    .is_none());
    Ok(())
}

#[tokio::test]
async fn scope_report_merges_the_clis_that_have_a_skill() -> crate::Result<()> {
    let home = tempfile::tempdir()?;
    let data = tempfile::tempdir()?;
    let body = "---\nname: graph\ndescription: Use when the user asks for a chart of numeric data.\n---\nBody\n";
    for dir in [
        home.path().join(".claude").join("skills"),
        data.path().join("skills"),
    ] {
        std::fs::create_dir_all(dir.join("graph"))?;
        std::fs::write(dir.join("graph").join("SKILL.md"), body)?;
    }
    let paths = crate::ConfigPaths::for_home(home.path(), data.path(), None);
    let agents = [
        AgentKind::ClaudeCode,
        AgentKind::Codex,
        AgentKind::Agy,
        AgentKind::Harness,
    ];
    let report = report_for_scope(&paths, &agents, crate::Scope::User).await?;
    assert_eq!(report.skills.len(), 1);
    assert_eq!(
        entry(&report, "graph").clients,
        vec!["claude-code", "harness"]
    );
    assert_eq!(report.grade(), Grade::Good);
    Ok(())
}
