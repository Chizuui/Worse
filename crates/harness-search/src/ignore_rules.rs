//! Port of `packages/services/src/file/workspaceFileIgnore.ts` (read side).
//!
//! `.zcodeignore` at the workspace root (gitignore syntax) is the single source
//! of truth for the search index. When it is absent, the in-memory template
//! (root `.gitignore` + sync marker + deduplicated built-in defaults + defaults
//! marker) is used; the file is never written by the Rust Host. Matching is
//! case-insensitive, like the `ignore` npm package default (`ignorecase: true`).

use std::fs;
use std::io;
use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Name of the per-workspace search ignore file.
pub const WORKSPACE_FILE_SEARCH_IGNORE_FILE_NAME: &str = ".zcodeignore";
const GITIGNORE_FILE_NAME: &str = ".gitignore";

const BUILTIN_IGNORE_LINES: [&str; 28] = [
    ".git/",
    ".hg/",
    ".svn/",
    "node_modules/",
    "bower_components/",
    "jspm_packages/",
    "__pycache__/",
    "site-packages/",
    "venv/",
    "coverage/",
    "htmlcov/",
    "lcov-report/",
    "cmakefiles/",
    "cmake-build-*/",
    "bazel-*/",
    "pods/",
    "deriveddata/",
    "storybook-static/",
    "playwright-report/",
    "test-results/",
    "allure-results/",
    "allure-report/",
    "cdk.out/",
    "*.egg-info/",
    "*.dist-info/",
    "eggs/",
    "pip-wheel-metadata/",
    "wheels/",
];

const TEMPLATE_HEADER: [&str; 5] = [
    "# ZCode 工作区文件搜索忽略规则（.zcodeignore）",
    "# 语法与 .gitignore 一致，只影响 ZCode 的 @ 文件候选 / Command Center / 文件树搜索，",
    "# 不影响文件树浏览、上传或 Agent 文件访问。",
    "# 修改 .gitignore 不会自动同步到本文件；可在设置页「从 .gitignore 同步」。",
    "",
];

/// Sync marker line (exact TS text).
pub const SYNC_MARKER: &str =
    "# ===== ↑ 以上同步自 .gitignore（「从 .gitignore 同步」只重写以上部分）=====";
/// Defaults marker line (exact TS text).
pub const DEFAULTS_MARKER: &str =
    "# ----- ↑ 以上为 ZCode 默认排除规则（自定义规则请写在本行下方，不会被同步/恢复改动）-----";
const CUSTOM_SECTION_HINT: &str = "# 自定义规则写在下方（本行提示可删除）";

/// Where the active rules came from (TS `WorkspaceFileSearchIgnoreRulesSource`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RulesSource {
    /// `.zcodeignore` exists and was read.
    File,
    /// In-memory template (file absent).
    Template,
    /// `.zcodeignore` unreadable; `.gitignore` used.
    FallbackGitignore,
    /// Nothing readable; built-in defaults used.
    FallbackBuiltin,
}

/// Compiled ignore rules for one workspace root.
#[derive(Debug, Clone)]
pub struct IgnoreRules {
    matcher: Gitignore,
    /// Origin of the rules.
    pub source: RulesSource,
}

impl IgnoreRules {
    /// Compiles gitignore-syntax `content`; invalid lines are skipped like the
    /// `ignore` npm package does.
    #[must_use]
    pub fn from_content(content: &str) -> Self {
        Self {
            matcher: build_matcher(content),
            source: RulesSource::File,
        }
    }

    /// TS `isWorkspaceFileSearchPathIgnored`: `relative_path` uses `/`;
    /// directories match directory-only rules.
    #[must_use]
    pub fn is_ignored(&self, relative_path: &str, is_dir: bool) -> bool {
        self.matcher
            .matched(Path::new(relative_path), is_dir)
            .is_ignore()
    }
}

fn build_matcher(content: &str) -> Gitignore {
    let mut builder = GitignoreBuilder::new("");
    let _ = builder.case_insensitive(true);
    for line in content.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Err(error) = builder.add_line(None, line) {
            tracing::debug!(%error, line, "skipping invalid ignore rule");
        }
    }
    builder.build().unwrap_or_else(|error| {
        tracing::warn!(%error, "ignore rules failed to compile; using none");
        Gitignore::empty()
    })
}

fn split_lines(content: &str) -> impl Iterator<Item = &str> {
    content
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
}

/// TS `buildBuiltinDefaultsSection`: built-in lines minus those already
/// declared by the gitignore section (trim-equal, ignoring one trailing `/`).
fn builtin_defaults_section(gitignore: Option<&str>) -> String {
    let Some(gitignore) = gitignore else {
        return BUILTIN_IGNORE_LINES.join("\n");
    };
    let mut declared = std::collections::HashSet::new();
    for raw in split_lines(gitignore) {
        let line = crate::utf16::js_trim(raw);
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        declared.insert(line.to_owned());
        declared.insert(line.strip_suffix('/').unwrap_or(line).to_owned());
    }
    BUILTIN_IGNORE_LINES
        .iter()
        .filter(|line| {
            let bare = line.strip_suffix('/').unwrap_or(line);
            !declared.contains(**line) && !declared.contains(bare)
        })
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
}

/// TS `buildWorkspaceFileSearchIgnoreTemplate`.
#[must_use]
pub fn build_template(gitignore: Option<&str>) -> String {
    let gitignore_section = match gitignore {
        Some(content) if !crate::utf16::js_trim(content).is_empty() => {
            if content.ends_with('\n') {
                content.to_owned()
            } else {
                format!("{content}\n")
            }
        }
        _ => format!("{}\n", TEMPLATE_HEADER.join("\n")),
    };
    [
        gitignore_section.as_str(),
        SYNC_MARKER,
        &builtin_defaults_section(gitignore),
        DEFAULTS_MARKER,
        CUSTOM_SECTION_HINT,
        "",
    ]
    .join("\n")
}

fn read_optional(path: &Path) -> io::Result<Option<String>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// TS `loadWorkspaceFileSearchIgnoreRules` without the (disabled) auto-create.
#[must_use]
pub fn load_rules(root: &Path) -> IgnoreRules {
    let gitignore = || {
        read_optional(&root.join(GITIGNORE_FILE_NAME))
            .ok()
            .flatten()
    };
    match read_optional(&root.join(WORKSPACE_FILE_SEARCH_IGNORE_FILE_NAME)) {
        Ok(Some(content)) => IgnoreRules {
            matcher: build_matcher(&content),
            source: RulesSource::File,
        },
        Ok(None) => IgnoreRules {
            matcher: build_matcher(&build_template(gitignore().as_deref())),
            source: RulesSource::Template,
        },
        Err(error) => {
            if let Some(content) = gitignore() {
                tracing::warn!(%error, "[workspace-file-ignore] .zcodeignore unreadable, using .gitignore");
                IgnoreRules {
                    matcher: build_matcher(&content),
                    source: RulesSource::FallbackGitignore,
                }
            } else {
                tracing::warn!(%error, "[workspace-file-ignore] .zcodeignore unreadable, using built-in defaults");
                IgnoreRules {
                    matcher: build_matcher(&build_template(None)),
                    source: RulesSource::FallbackBuiltin,
                }
            }
        }
    }
}

/// Cache-signature fingerprint of `.zcodeignore` (`mtime:size` or `none`).
#[must_use]
pub fn ignore_file_fingerprint(root: &Path) -> String {
    match fs::metadata(root.join(WORKSPACE_FILE_SEARCH_IGNORE_FILE_NAME)) {
        Ok(meta) => {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            format!("{mtime}:{}", meta.len())
        }
        Err(_) => "none".to_owned(),
    }
}
