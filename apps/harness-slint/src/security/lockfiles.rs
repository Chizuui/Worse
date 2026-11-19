//! Lockfile discovery for the dependency audit (spec `docs/specs/harness/security-panel.md` §3.2): the twelve watched
//! file names, found two directories deep, skipping the trees that never hold the project's own lockfile.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The twelve lockfiles §3.2 watches.
pub const NAMES: [&str; 12] = [
    "Cargo.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "go.mod",
    "poetry.lock",
    "Pipfile.lock",
    "requirements.txt",
    "Gemfile.lock",
    "composer.lock",
    "pubspec.lock",
    "packages.lock.json",
];

const SKIP: [&str; 6] = ["node_modules", "target", "vendor", "dist", "build", "out"];
const DEPTH: usize = 2;

/// Absolute paths of every watched lockfile under `root`, parents first.
#[must_use]
pub fn in_workspace(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        for name in NAMES {
            let path = dir.join(name);
            if path.is_file() {
                out.push(path);
            }
        }
        if depth == 0 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut dirs: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.path())
            .filter(|path| {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                !name.starts_with('.') && !SKIP.contains(&name.as_ref())
            })
            .collect();
        dirs.sort();
        for dir in dirs {
            walk(&dir, depth - 1, out);
        }
    }
    let mut out = Vec::new();
    walk(root, DEPTH, &mut out);
    out
}

/// Newest modification time of the watched lockfiles, the debounce key.
#[must_use]
pub fn newest(root: &Path) -> Option<SystemTime> {
    in_workspace(root)
        .iter()
        .filter_map(|path| std::fs::metadata(path).ok()?.modified().ok())
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_lockfiles_two_levels_deep_and_skips_noise() {
        let base = std::env::temp_dir().join(format!("harness-lockfiles-{}", std::process::id()));
        let nested = base.join("crates").join("app");
        let skipped = base.join("node_modules").join("pkg");
        std::fs::create_dir_all(&nested).ok();
        std::fs::create_dir_all(&skipped).ok();
        std::fs::write(base.join("Cargo.lock"), "").ok();
        std::fs::write(nested.join("package-lock.json"), "").ok();
        std::fs::write(skipped.join("yarn.lock"), "").ok();
        let found = in_workspace(&base);
        let names: Vec<String> = found
            .iter()
            .filter_map(|path| Some(path.file_name()?.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(names, ["Cargo.lock", "package-lock.json"]);
        assert!(newest(&base).is_some());
        std::fs::remove_dir_all(&base).ok();
    }
}
