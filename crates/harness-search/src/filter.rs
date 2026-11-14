//! Port of `defaultWorkspaceFileSearchFilter`
//! (`packages/services/src/file/workspaceFileMentionFilter.ts`).

const SKIPPED_DIRECTORY_NAMES: [&str; 24] = [
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "bower_components",
    "jspm_packages",
    "__pycache__",
    "site-packages",
    "venv",
    "coverage",
    "htmlcov",
    "lcov-report",
    "cmakefiles",
    "pods",
    "deriveddata",
    "storybook-static",
    "playwright-report",
    "test-results",
    "allure-results",
    "allure-report",
    "cdk.out",
    "eggs",
    "pip-wheel-metadata",
    "wheels",
];
const SKIPPED_DIRECTORY_PREFIXES: [&str; 2] = ["cmake-build-", "bazel-"];
const SKIPPED_DIRECTORY_SUFFIXES: [&str; 2] = [".egg-info", ".dist-info"];
const SKIPPED_FILE_NAMES: [&str; 2] = ["coverage.out", "lcov.info"];
const SKIPPED_FILE_EXTENSIONS: [&str; 29] = [
    ".a",
    ".aar",
    ".beam",
    ".class",
    ".dll",
    ".dylib",
    ".ear",
    ".exe",
    ".gcda",
    ".gcno",
    ".gem",
    ".hi",
    ".idb",
    ".ilk",
    ".jar",
    ".lib",
    ".node",
    ".nupkg",
    ".o",
    ".obj",
    ".pdb",
    ".profdata",
    ".profraw",
    ".pyc",
    ".pyo",
    ".rlib",
    ".so",
    ".tsbuildinfo",
    ".war",
];

/// Filter verdict for one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    /// Whether the entry enters the index.
    pub include: bool,
    /// Whether a directory is descended into.
    pub traverse: bool,
}

/// Node `path.extname` of a bare file name.
fn extname(name: &str) -> &str {
    match name.rfind('.') {
        Some(index) if index > 0 && !name[..index].chars().all(|c| c == '.') => &name[index..],
        _ => "",
    }
}

fn should_skip_directory(name: &str) -> bool {
    let n = name.to_lowercase();
    SKIPPED_DIRECTORY_NAMES.contains(&n.as_str())
        || SKIPPED_DIRECTORY_PREFIXES.iter().any(|p| n.starts_with(p))
        || SKIPPED_DIRECTORY_SUFFIXES.iter().any(|s| n.ends_with(s))
}

fn should_skip_file(name: &str) -> bool {
    let n = name.to_lowercase();
    n == ".env"
        || n.starts_with(".env.")
        || SKIPPED_FILE_NAMES.contains(&n.as_str())
        || SKIPPED_FILE_EXTENSIONS.contains(&extname(&n))
}

fn is_inside_hidden_directory(relative_path: &str) -> bool {
    let mut segments: Vec<&str> = relative_path.split('/').collect();
    segments.pop();
    segments.iter().any(|segment| segment.starts_with('.'))
}

/// `defaultWorkspaceFileSearchFilter.evaluate(entry, { ignoreRulesActive })`.
#[must_use]
pub fn evaluate(
    name: &str,
    relative_path: &str,
    is_dir: bool,
    ignore_rules_active: bool,
) -> Decision {
    if is_dir {
        if !ignore_rules_active && should_skip_directory(name) {
            return Decision {
                include: false,
                traverse: false,
            };
        }
        let hidden = name.starts_with('.') || is_inside_hidden_directory(relative_path);
        return Decision {
            include: !hidden,
            traverse: true,
        };
    }
    Decision {
        include: !should_skip_file(name),
        traverse: false,
    }
}
