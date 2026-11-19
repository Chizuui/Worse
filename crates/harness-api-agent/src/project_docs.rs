//! Project instructions (`AGENTS.md`, or `CLAUDE.md` as a fallback) appended to the system prompt.
//!
//! The files are read once per prompt turn, so the system prompt stays byte-identical across the model
//! requests of that turn (prompt caching relies on a stable prefix).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Total budget of the project instructions of one turn.
pub(crate) const BUDGET: usize = 32 * 1024;
/// Note appended to content cut at [`BUDGET`].
const TRUNCATED: &str = "\n[truncated]";
/// A `CLAUDE.md` holding only this line just imports the `AGENTS.md` next to it.
const IMPORT_LINE: &str = "@AGENTS.md";
/// Closing line of the block: tells the model the pointers are current.
const CLOSING: &str = "These instructions and the code map above are current: use their file pointers and go straight to the relevant files instead of exploring the codebase.";

/// Prompt block with the project instructions that apply to `cwd`; empty when the project has none.
///
/// Files come outermost first (repository root → `cwd`), so the closest file wins where they disagree.
pub(crate) fn instructions(cwd: &str) -> String {
    let cwd = Path::new(cwd);
    if cwd.as_os_str().is_empty() {
        return String::new();
    }
    let mut text = String::new();
    let mut budget = BUDGET;
    for dir in search_path(cwd) {
        let Some(file) = doc_file(&dir) else { continue };
        let Ok(body) = std::fs::read_to_string(&file) else {
            continue;
        };
        let body = body.trim();
        if body.is_empty() {
            continue;
        }
        let cut = body.len() > budget;
        let body = if cut {
            &body[..boundary(body, budget)]
        } else {
            body
        };
        let _ = write!(
            text,
            "\n\n# Project instructions ({})\n{body}",
            display(cwd, &file)
        );
        if cut {
            text.push_str(TRUNCATED);
            break;
        }
        budget -= body.len();
        if budget == 0 {
            break;
        }
    }
    if text.is_empty() {
        return String::new();
    }
    text.push_str("\n\n");
    text.push_str(CLOSING);
    text
}

/// `cwd` and its parents, up to and including the first directory holding `.git` (file or dir) or the
/// filesystem root; outermost first.
fn search_path(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for dir in cwd.ancestors() {
        if dir.as_os_str().is_empty() {
            break;
        }
        dirs.push(dir.to_path_buf());
        if dir.join(".git").exists() {
            break;
        }
    }
    dirs.reverse();
    dirs
}

/// `AGENTS.md` in `dir`, else `CLAUDE.md` unless it only holds its `@AGENTS.md` import line.
fn doc_file(dir: &Path) -> Option<PathBuf> {
    let agents = dir.join("AGENTS.md");
    if agents.is_file() {
        return Some(agents);
    }
    let claude = dir.join("CLAUDE.md");
    if !claude.is_file() {
        return None;
    }
    let import_only = std::fs::read_to_string(&claude).is_ok_and(|text| text.trim() == IMPORT_LINE);
    if import_only {
        None
    } else {
        Some(claude)
    }
}

/// Path relative to `cwd` when inside it, absolute otherwise (forward slashes on every OS).
fn display(cwd: &Path, file: &Path) -> String {
    file.strip_prefix(cwd)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Largest char boundary `<= index`: the budget cut keeps the content valid UTF-8.
fn boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    /// Temporary project directory, removed on drop (the crate has no `tempfile` dependency).
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Option<Self> {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "harness-api-agent-docs-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).ok()?;
            Some(Self { path })
        }

        /// `name` relative to the project root, as an absolute path string.
        fn dir(&self, name: &str) -> String {
            if name.is_empty() {
                return self.path.to_string_lossy().into_owned();
            }
            self.path.join(name).to_string_lossy().into_owned()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Project with `.git` at its root plus the given `(relative path, content)` files.
    fn project(files: &[(&str, &str)]) -> Option<TempDir> {
        let dir = TempDir::new()?;
        for (name, body) in files {
            let path = dir.path.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok()?;
            }
            std::fs::write(&path, body).ok()?;
        }
        Some(dir)
    }

    #[test]
    fn no_project_docs_adds_nothing() {
        let Some(project) = project(&[(".git/config", "")]) else {
            return;
        };
        assert_eq!(instructions(&project.dir("")), "");
    }

    #[test]
    fn nested_agents_md_outermost_first() {
        let Some(project) = project(&[
            (".git/config", ""),
            ("AGENTS.md", "root rules"),
            ("sub/AGENTS.md", "sub rules"),
        ]) else {
            return;
        };
        let out = instructions(&project.dir("sub"));
        let root = out.find("root rules").unwrap_or(usize::MAX);
        let sub = out.find("sub rules").unwrap_or(usize::MAX);
        assert!(root < sub, "{out}");
        // The file in `cwd` gets a relative pointer, the one above it an absolute one.
        assert!(
            out.contains("# Project instructions (AGENTS.md)\nsub rules"),
            "{out}"
        );
        assert!(
            out.contains(&format!(
                "# Project instructions ({}",
                project.dir("AGENTS.md").replace('\\', "/")
            )),
            "{out}"
        );
        assert!(out.ends_with(CLOSING), "{out}");
    }

    #[test]
    fn claude_md_used_when_no_agents_md() {
        let Some(project) = project(&[(".git/config", ""), ("CLAUDE.md", "claude rules")]) else {
            return;
        };
        let out = instructions(&project.dir(""));
        assert!(
            out.contains("# Project instructions (CLAUDE.md)\nclaude rules"),
            "{out}"
        );
    }

    #[test]
    fn agents_md_wins_over_claude_md() {
        let Some(project) = project(&[
            (".git/config", ""),
            ("AGENTS.md", "agents rules"),
            ("CLAUDE.md", "claude rules"),
        ]) else {
            return;
        };
        let out = instructions(&project.dir(""));
        assert!(out.contains("agents rules"), "{out}");
        assert!(!out.contains("claude rules"), "{out}");
    }

    #[test]
    fn import_only_claude_md_is_skipped() {
        let Some(project) = project(&[(".git/config", ""), ("CLAUDE.md", "@AGENTS.md\n")]) else {
            return;
        };
        assert_eq!(instructions(&project.dir("")), "");
    }

    #[test]
    fn import_only_claude_md_does_not_shadow_nested_agents_md() {
        let Some(project) = project(&[
            (".git/config", ""),
            ("CLAUDE.md", "@AGENTS.md"),
            ("sub/AGENTS.md", "sub rules"),
        ]) else {
            return;
        };
        let out = instructions(&project.dir("sub"));
        assert!(!out.contains("CLAUDE.md"), "{out}");
        assert!(out.contains("sub rules"), "{out}");
    }

    #[test]
    fn walk_stops_at_git() {
        let Some(project) = project(&[
            ("AGENTS.md", "outside rules"),
            ("repo/.git/config", ""),
            ("repo/AGENTS.md", "inside rules"),
        ]) else {
            return;
        };
        let out = instructions(&project.dir("repo"));
        assert!(out.contains("inside rules"), "{out}");
        assert!(!out.contains("outside rules"), "{out}");
    }

    #[test]
    fn content_is_capped_at_32_kb() {
        let big = "z".repeat(BUDGET * 2);
        let Some(project) = project(&[(".git/config", ""), ("AGENTS.md", &big)]) else {
            return;
        };
        let out = instructions(&project.dir(""));
        assert_eq!(out.matches('z').count(), BUDGET);
        assert!(out.contains("[truncated]"), "missing truncation note");
        assert!(out.ends_with(CLOSING));
    }

    #[test]
    fn budget_is_shared_between_files() {
        let root = "z".repeat(BUDGET - 10);
        let sub = "q".repeat(100);
        let Some(project) = project(&[
            (".git/config", ""),
            ("AGENTS.md", &root),
            ("sub/AGENTS.md", &sub),
        ]) else {
            return;
        };
        let out = instructions(&project.dir("sub"));
        assert_eq!(out.matches('z').count(), BUDGET - 10);
        assert_eq!(out.matches('q').count(), 10);
        assert!(out.contains("[truncated]"), "{out}");
    }

    #[test]
    fn cut_keeps_utf8_boundaries() {
        let body = "é".repeat(BUDGET);
        let Some(project) = project(&[(".git/config", ""), ("AGENTS.md", &body)]) else {
            return;
        };
        let out = instructions(&project.dir(""));
        assert_eq!(out.matches('é').count(), BUDGET / 2);
        assert!(out.contains("[truncated]"), "{out}");
    }
}
