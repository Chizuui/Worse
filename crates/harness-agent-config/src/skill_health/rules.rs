//! Static rules and report types.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Tunables of the static check.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    pub min_description_chars: usize,
    pub max_description_chars: usize,
    pub overlap_jaccard: f64,
    pub overlap_shared_words: usize,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            min_description_chars: 40,
            max_description_chars: 500,
            overlap_jaccard: 0.5,
            overlap_shared_words: 6,
        }
    }
}

/// How much a finding matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Severity {
    Warn,
    Error,
}

impl Severity {
    fn penalty(self) -> u32 {
        match self {
            Self::Warn => 15,
            Self::Error => 40,
        }
    }
}

/// What is wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FindingCode {
    MissingDescription,
    DescriptionTooShort,
    DescriptionTooLong,
    NoTriggerPhrase,
    OverlappingDescription,
}

/// One finding about one skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub code: FindingCode,
    pub severity: Severity,
    /// English UI text.
    pub message: String,
}

/// Health of one skill, merged over the CLIs that have it installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillHealth {
    pub name: String,
    /// CLIs that have this skill (`claude-code`, `harness`, ...).
    pub clients: Vec<String>,
    pub description: Option<String>,
    pub findings: Vec<Finding>,
}

/// Grade of one skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Grade {
    Good,
    Warning,
    Poor,
}

impl Grade {
    fn of(findings: &[Finding], score: u32) -> Self {
        if findings.iter().any(|f| f.severity == Severity::Error) || score < 50 {
            Self::Poor
        } else if findings.is_empty() {
            Self::Good
        } else {
            Self::Warning
        }
    }
}

impl SkillHealth {
    /// 100 minus the finding penalties, floored at 0.
    #[must_use]
    pub fn score(&self) -> u32 {
        self.findings
            .iter()
            .fold(100_u32, |acc, f| acc.saturating_sub(f.severity.penalty()))
    }

    #[must_use]
    pub fn grade(&self) -> Grade {
        Grade::of(&self.findings, self.score())
    }
}

/// Whole-catalog result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub skills: Vec<SkillHealth>,
}

impl Report {
    /// Worst skill grade, `good` for an empty catalog.
    #[must_use]
    pub fn grade(&self) -> Grade {
        self.skills
            .iter()
            .map(SkillHealth::grade)
            .max_by_key(|g| match g {
                Grade::Good => 0,
                Grade::Warning => 1,
                Grade::Poor => 2,
            })
            .unwrap_or(Grade::Good)
    }

    /// Mean skill score, 100 for an empty catalog.
    #[must_use]
    pub fn score(&self) -> u32 {
        if self.skills.is_empty() {
            return 100;
        }
        let total: u32 = self.skills.iter().map(SkillHealth::score).sum();
        total / u32::try_from(self.skills.len()).unwrap_or(1)
    }

    /// Health of one skill.
    #[must_use]
    pub fn skill(&self, name: &str) -> Option<&SkillHealth> {
        self.skills.iter().find(|s| s.name == name)
    }
}

/// One installed skill as the check sees it: name, the CLI that has it, its description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledSkill {
    pub name: String,
    pub client: String,
    pub description: Option<String>,
}

impl InstalledSkill {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        client: impl Into<String>,
        description: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            client: client.into(),
            description,
        }
    }
}

const TRIGGER_PHRASES: [&str; 6] = [
    "use when",
    "use this",
    "use for",
    "when the user",
    "whenever",
    "before ",
];

const STOP_WORDS: [&str; 24] = [
    "the", "and", "for", "use", "when", "this", "that", "with", "from", "your", "you", "any",
    "all", "not", "but", "are", "was", "will", "can", "its", "has", "have", "into", "skill",
];

fn content_words(text: &str) -> Vec<String> {
    let mut words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .map(str::to_lowercase)
        .filter(|w| !STOP_WORDS.contains(&w.as_str()))
        .collect();
    words.sort();
    words.dedup();
    words
}

fn jaccard(a: &[String], b: &[String]) -> (f64, usize) {
    let shared = a.iter().filter(|w| b.binary_search(w).is_ok()).count();
    let union = a.len() + b.len() - shared;
    if union == 0 {
        return (0.0, 0);
    }
    #[allow(clippy::cast_precision_loss)]
    let ratio = shared as f64 / union as f64;
    (ratio, shared)
}

/// Static check over installed skills. Same-name entries merge into one result.
#[must_use]
pub fn check(skills: &[InstalledSkill]) -> Report {
    check_with(skills, &Thresholds::default())
}

/// [`check`] with explicit thresholds.
#[must_use]
pub fn check_with(skills: &[InstalledSkill], t: &Thresholds) -> Report {
    let mut merged: BTreeMap<String, SkillHealth> = BTreeMap::new();
    for skill in skills {
        let entry = merged
            .entry(skill.name.clone())
            .or_insert_with(|| SkillHealth {
                name: skill.name.clone(),
                clients: Vec::new(),
                description: None,
                findings: Vec::new(),
            });
        if !entry.clients.contains(&skill.client) {
            entry.clients.push(skill.client.clone());
        }
        if entry.description.is_none() {
            entry.description.clone_from(&skill.description);
        }
    }

    let mut report = Report {
        skills: merged.into_values().collect(),
    };
    let words: Vec<Vec<String>> = report
        .skills
        .iter()
        .map(|s| content_words(s.description.as_deref().unwrap_or_default()))
        .collect();
    for i in 0..report.skills.len() {
        let mut findings = description_findings(report.skills[i].description.as_deref(), t);
        let own = &words[i];
        for (j, other) in report.skills.iter().enumerate() {
            if i == j {
                continue;
            }
            let theirs = &words[j];
            if own.len().min(theirs.len()) < t.overlap_shared_words {
                continue;
            }
            let (ratio, shared) = jaccard(own, theirs);
            if shared >= t.overlap_shared_words && ratio >= t.overlap_jaccard {
                findings.push(Finding {
                    code: FindingCode::OverlappingDescription,
                    severity: Severity::Warn,
                    message: format!(
                        "Description overlaps with \u{201c}{}\u{201d}; the agent may pick either one",
                        other.name
                    ),
                });
            }
        }
        report.skills[i].findings = findings;
    }
    report
}

fn description_findings(description: Option<&str>, t: &Thresholds) -> Vec<Finding> {
    let Some(text) = description.map(str::trim).filter(|d| !d.is_empty()) else {
        return vec![Finding {
            code: FindingCode::MissingDescription,
            severity: Severity::Error,
            message: "No description, so the agent never knows when to use this skill".into(),
        }];
    };
    let mut out = Vec::new();
    let chars = text.chars().count();
    if chars < t.min_description_chars {
        out.push(Finding {
            code: FindingCode::DescriptionTooShort,
            severity: Severity::Warn,
            message: format!(
                "Description is {chars} characters; say what it does and when to use it (at least {} characters)",
                t.min_description_chars
            ),
        });
    }
    if chars > t.max_description_chars {
        out.push(Finding {
            code: FindingCode::DescriptionTooLong,
            severity: Severity::Warn,
            message: format!(
                "Description is {chars} characters; keep it under {} so routing stays accurate",
                t.max_description_chars
            ),
        });
    }
    let lower = text.to_lowercase();
    if !TRIGGER_PHRASES.iter().any(|p| lower.contains(p)) {
        out.push(Finding {
            code: FindingCode::NoTriggerPhrase,
            severity: Severity::Warn,
            message:
                "No trigger phrase; start with something like \u{201c}Use when\u{2026}\u{201d}"
                    .into(),
        });
    }
    out
}
