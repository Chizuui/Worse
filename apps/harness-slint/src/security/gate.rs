//! Commit gate (spec `docs/specs/harness/security-panel.md` §4): a pure function of the report, the gate setting
//! and the changed files. It never runs `git` and never mutates the working tree.

use crate::security::report::Vuln;

/// Whether `name` is one of the twelve lockfiles §3.2 watches (the file name only, not the directory).
#[must_use]
pub fn is_lockfile(name: &str) -> bool {
    crate::security::lockfiles::NAMES
        .iter()
        .any(|lockfile| name.eq_ignore_ascii_case(lockfile))
}

/// Whether any changed path is a watched lockfile.
#[must_use]
pub fn touches_lockfile<'a>(paths: impl IntoIterator<Item = &'a str>) -> bool {
    paths
        .into_iter()
        .any(|path| is_lockfile(path.rsplit(['/', '\\']).next().unwrap_or(path)))
}

/// What the gate decided for one commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to report: gate off, no lockfile changed, or a clean report.
    Allow,
    /// Gate `warn`: commit anyway, tell the user.
    Warn(String),
    /// Gate `block`: refuse the commit with this message.
    Block(String),
    /// The scan could not run; the commit proceeds and the result says why.
    Skipped(String),
}

impl Verdict {
    /// Short line the Git panel shows next to the commit result.
    #[must_use]
    pub fn notice(&self) -> Option<String> {
        match self {
            Verdict::Allow => None,
            Verdict::Warn(text) | Verdict::Block(text) | Verdict::Skipped(text) => {
                Some(text.clone())
            }
        }
    }
}

fn describe(rows: &[Vuln]) -> String {
    // Rows are sorted by severity, so the same package can appear far apart: dedupe on the whole set, then keep
    // the first three in report order.
    let mut packages: Vec<&str> = Vec::new();
    for row in rows {
        if !packages
            .iter()
            .any(|seen| seen.eq_ignore_ascii_case(&row.package))
        {
            packages.push(row.package.as_str());
        }
    }
    let listed: Vec<&str> = packages.iter().take(3).copied().collect();
    let more = packages.len().saturating_sub(listed.len());
    if more == 0 {
        listed.join(", ")
    } else {
        format!("{}, +{more} more", listed.join(", "))
    }
}

/// Decides the commit gate.
///
/// * `gate` — `security.commitGate`: `off`, `warn` or `block`.
/// * `changed` — every path of the commit (staged plus unstaged when `include_unstaged` is on).
/// * `report` — the latest scan, `Err` when it could not run.
pub fn decide(gate: &str, changed: &[String], report: Result<&[Vuln], &str>) -> Verdict {
    if gate != "warn" && gate != "block" {
        return Verdict::Allow;
    }
    if !touches_lockfile(changed.iter().map(String::as_str)) {
        return Verdict::Allow;
    }
    let rows = match report {
        Ok(rows) => rows,
        Err(reason) => {
            return Verdict::Skipped(format!("dependency scan skipped: {reason}"));
        }
    };
    if rows.is_empty() {
        return Verdict::Allow;
    }
    let count = rows.len();
    let advisories = if count == 1 {
        "1 advisory".to_owned()
    } else {
        format!("{count} advisories")
    };
    let text = format!("{advisories} in changed lockfiles ({})", describe(rows));
    if gate == "block" {
        Verdict::Block(format!(
            "commit blocked by the dependency gate: {text}; lower Settings › Security › Commit gate to warn to commit anyway"
        ))
    } else {
        Verdict::Warn(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(package: &str) -> Vuln {
        Vuln {
            package: package.to_owned(),
            version: "1.0.0".to_owned(),
            ecosystem: "crates.io".to_owned(),
            id: "RUSTSEC-0000-0001".to_owned(),
            severity: "high".to_owned(),
            summary: String::new(),
            fixed: "1.0.1".to_owned(),
            source: "Cargo.lock".to_owned(),
        }
    }

    #[test]
    fn lockfiles_are_recognized_by_file_name() {
        assert!(is_lockfile("Cargo.lock"));
        assert!(is_lockfile("package-lock.json"));
        assert!(!is_lockfile("src/main.rs"));
        assert!(touches_lockfile(["src/main.rs", "Cargo.lock"]));
        assert!(touches_lockfile(["crates\\x\\Cargo.lock"]));
        assert!(!touches_lockfile(["src/main.rs"]));
    }

    #[test]
    fn off_and_unrelated_commits_are_allowed() {
        let changed = vec!["src/main.rs".to_owned()];
        assert_eq!(decide("off", &changed, Ok(&[row("time")])), Verdict::Allow);
        assert_eq!(
            decide("block", &changed, Ok(&[row("time")])),
            Verdict::Allow
        );
    }

    #[test]
    fn warn_and_block_differ_only_in_the_verdict() {
        let changed = vec!["Cargo.lock".to_owned()];
        let rows = [row("time"), row("chrono")];
        let warn = decide("warn", &changed, Ok(&rows));
        let block = decide("block", &changed, Ok(&rows));
        assert!(matches!(warn, Verdict::Warn(_)));
        assert!(matches!(block, Verdict::Block(_)));
        let text = block.notice().unwrap_or_default();
        assert!(text.contains("2 advisories"));
        assert!(text.contains("time"));
        assert!(text.contains("Settings"));
        assert_eq!(decide("warn", &changed, Ok(&[])), Verdict::Allow);
    }

    #[test]
    fn a_package_with_two_advisories_is_named_once() {
        let changed = vec!["Cargo.lock".to_owned()];
        let rows = [row("time"), row("chrono"), row("time")];
        let text = decide("warn", &changed, Ok(&rows))
            .notice()
            .unwrap_or_default();
        assert!(text.contains("3 advisories"));
        assert_eq!(text.matches("time").count(), 1, "{text}");
        assert!(text.contains("chrono"));
    }

    #[test]
    fn a_failed_scan_never_blocks() {
        let changed = vec!["Cargo.lock".to_owned()];
        let verdict = decide("block", &changed, Err("the scanner is not installed"));
        assert!(matches!(verdict, Verdict::Skipped(_)));
        assert!(verdict
            .notice()
            .unwrap_or_default()
            .contains("not installed"));
    }
}
