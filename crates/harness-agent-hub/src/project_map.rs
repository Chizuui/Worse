use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

const WALK_DEADLINE: Duration = Duration::from_millis(1500);
const WALK_FILES_MAX: usize = 60_000;
const WALK_DEPTH: usize = 6;
const DIRS_SHOWN: usize = 12;
const SUBDIRS_SHOWN: usize = 6;
const ENTRIES_SHOWN: usize = 14;
const HOT_SHOWN: usize = 10;
const GOD_SHOWN: usize = 10;
const COMMUNITIES_SHOWN: usize = 8;
const GRAPH_BYTES_MAX: u64 = 64 * 1024 * 1024;
const SKIPPED_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    ".git",
    ".next",
    ".nuxt",
    ".turbo",
    ".gradle",
    "vendor",
    "__pycache__",
    ".venv",
    "venv",
    "coverage",
    "graphify-out",
];
const ENTRY_FILES: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "main.rs",
    "lib.rs",
    "main.go",
    "main.py",
    "__main__.py",
    "app.py",
    "manage.py",
    "index.ts",
    "index.tsx",
    "index.js",
    "main.ts",
    "main.tsx",
    "main.js",
    "server.ts",
    "server.js",
    "App.tsx",
    "Program.cs",
    "Main.java",
    "main.kt",
    "main.c",
    "main.cpp",
];

static BUILDING: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

#[must_use]
pub fn prompt_block(home: &Path, workspace: &str) -> Option<String> {
    let root = PathBuf::from(workspace);
    if !root.is_dir() || !is_project(&root) {
        return None;
    }
    let stamp = stamp(&root);
    let cache = cache_path(home, workspace);
    match read_cache(&cache) {
        Some((cached, body)) if cached == stamp => non_empty(body),
        Some((_, body)) => {
            rebuild_later(root, &cache, stamp);
            non_empty(body)
        }
        None => {
            let body = build(&root);
            write_cache(&cache, &stamp, &body);
            non_empty(body)
        }
    }
}

fn non_empty(body: String) -> Option<String> {
    (!body.trim().is_empty()).then_some(body)
}

fn is_project(root: &Path) -> bool {
    if root.parent().is_none() {
        return false;
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    home.map_or(true, |home| !same_path(&home, root))
}

fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| crate::recall::project_key(&p.to_string_lossy());
    norm(a) == norm(b)
}

fn cache_path(home: &Path, workspace: &str) -> PathBuf {
    let key = crate::recall::project_key(workspace);
    let digest = Sha256::digest(key.as_bytes());
    let name: String = digest.iter().take(8).fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    });
    home.join("memory").join("maps").join(format!("{name}.md"))
}

fn read_cache(path: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let (first, body) = text.split_once('\n')?;
    let stamp = first.strip_prefix("stamp: ")?;
    Some((stamp.to_owned(), body.to_owned()))
}

fn write_cache(path: &Path, stamp: &str, body: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, format!("stamp: {stamp}\n{body}")).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn rebuild_later(root: PathBuf, cache: &Path, stamp: String) {
    {
        let Ok(mut guard) = BUILDING.lock() else {
            return;
        };
        let set = guard.get_or_insert_with(HashSet::new);
        if !set.insert(cache.to_path_buf()) {
            return;
        }
    }
    let spawned = std::thread::Builder::new()
        .name("harness-project-map".into())
        .spawn({
            let cache = cache.to_path_buf();
            move || {
                let body = build(&root);
                write_cache(&cache, &stamp, &body);
                done(&cache);
            }
        });
    if spawned.is_err() {
        done(cache);
    }
}

fn done(cache: &Path) {
    if let Ok(mut guard) = BUILDING.lock() {
        if let Some(set) = guard.as_mut() {
            set.remove(cache);
        }
    }
}

fn stamp(root: &Path) -> String {
    let head = git_head(root).unwrap_or_else(|| "nogit".to_owned());
    let graph = std::fs::metadata(crate::graphify::paths(root).graph)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    format!("{head} {graph}")
}

fn git_dir(root: &Path) -> Option<PathBuf> {
    let dot = root.join(".git");
    if dot.is_dir() {
        return Some(dot);
    }
    let text = std::fs::read_to_string(&dot).ok()?;
    let target = text.trim().strip_prefix("gitdir:")?.trim();
    let path = PathBuf::from(target);
    Some(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
}

fn git_head(root: &Path) -> Option<String> {
    let dir = git_dir(root)?;
    let head = std::fs::read_to_string(dir.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(reference) = head.strip_prefix("ref:").map(str::trim) else {
        return Some(head.to_owned());
    };
    let common = std::fs::read_to_string(dir.join("commondir"))
        .ok()
        .map_or_else(|| dir.clone(), |c| dir.join(c.trim()));
    for base in [&dir, &common] {
        if let Ok(sha) = std::fs::read_to_string(base.join(reference)) {
            return Some(sha.trim().to_owned());
        }
    }
    let packed = std::fs::read_to_string(common.join("packed-refs")).ok();
    packed
        .as_deref()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_suffix(reference)?.trim().split(' ').next())
                .map(str::to_owned)
        })
        .or_else(|| Some(reference.to_owned()))
}

#[derive(Default)]
struct Tree {
    dirs: BTreeMap<String, usize>,
    subdirs: HashMap<String, BTreeMap<String, usize>>,
    root_files: usize,
    entries: Vec<String>,
    partial: bool,
}

fn walk(root: &Path) -> Tree {
    let started = Instant::now();
    let mut tree = Tree::default();
    let walker = ignore::WalkBuilder::new(root)
        .require_git(false)
        .max_depth(Some(WALK_DEPTH))
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry.file_type().is_some_and(|t| t.is_dir())
                || !SKIPPED_DIRS.contains(&entry.file_name().to_string_lossy().as_ref())
        })
        .build();
    let mut files = 0usize;
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        files += 1;
        if files > WALK_FILES_MAX || started.elapsed() > WALK_DEADLINE {
            tree.partial = true;
            break;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let parts: Vec<String> = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        match parts.as_slice() {
            [] => {}
            [_] => tree.root_files += 1,
            [top, rest @ ..] => {
                *tree.dirs.entry(top.clone()).or_default() += 1;
                if rest.len() > 1 {
                    *tree
                        .subdirs
                        .entry(top.clone())
                        .or_default()
                        .entry(rest[0].clone())
                        .or_default() += 1;
                }
            }
        }
        let name = parts.last().map(String::as_str).unwrap_or_default();
        if parts.len() <= 4 && ENTRY_FILES.contains(&name) {
            tree.entries.push(parts.join("/"));
        }
    }
    tree.entries.sort_by_key(|p| {
        let depth = p.matches('/').count();
        let name = p.rsplit('/').next().unwrap_or(p);
        let manifest = ENTRY_FILES[..7].contains(&name);
        let class = match (depth, manifest) {
            (0, _) => 0,
            (_, false) => 1,
            (_, true) => 2,
        };
        (class, depth, p.clone())
    });
    tree
}

fn hot_files(root: &Path) -> Vec<(String, usize)> {
    if git_dir(root).is_none() {
        return Vec::new();
    }
    let mut command = std::process::Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args([
            "log",
            "-n",
            "300",
            "--no-merges",
            "--name-only",
            "--format=",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000);
    }
    let Ok(output) = command.output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let mut counts: HashMap<String, usize> = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if line.is_empty() || is_noise(line) {
            continue;
        }
        *counts.entry(line.to_owned()).or_default() += 1;
    }
    let mut hot: Vec<(String, usize)> = counts
        .into_iter()
        .filter(|(path, n)| *n > 1 && root.join(path).is_file())
        .collect();
    hot.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    hot.truncate(HOT_SHOWN);
    hot
}

fn is_noise(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    Path::new(name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("lock"))
        || name.ends_with("-lock.json")
        || name.ends_with("-lock.yaml")
        || name == "CHANGELOG.md"
        || path.split('/').any(|part| SKIPPED_DIRS.contains(&part))
}

struct Node {
    label: String,
    file: String,
    community: Option<i64>,
    degree: usize,
}

struct GraphSummary {
    god: Vec<(String, String, usize)>,
    communities: Vec<(usize, Vec<String>, String)>,
}

fn graph_summary(root: &Path) -> Option<GraphSummary> {
    let path = crate::graphify::paths(root).graph;
    if std::fs::metadata(&path).ok()?.len() > GRAPH_BYTES_MAX {
        return None;
    }
    let graph: Value = serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
    summarize(&graph)
}

fn summarize(graph: &Value) -> Option<GraphSummary> {
    let nodes = graph.get("nodes")?.as_array()?;
    let links = graph
        .get("links")
        .or_else(|| graph.get("edges"))
        .and_then(Value::as_array);
    let id_of = |v: &Value| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    let mut degree: HashMap<String, usize> = HashMap::new();
    for link in links.into_iter().flatten() {
        for end in ["source", "target"] {
            if let Some(id) = link.get(end).and_then(id_of) {
                *degree.entry(id).or_default() += 1;
            }
        }
    }
    let mut all: Vec<Node> = nodes
        .iter()
        .filter_map(|node| {
            let id = node.get("id").and_then(id_of)?;
            let label = node
                .get("label")
                .and_then(Value::as_str)
                .map_or_else(|| id.clone(), str::to_owned);
            let file = node
                .get("source_file")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .replace('\\', "/");
            Some(Node {
                degree: degree.get(&id).copied().unwrap_or(0),
                label,
                file,
                community: node.get("community").and_then(Value::as_i64),
            })
        })
        .collect();
    all.sort_by(|a, b| b.degree.cmp(&a.degree).then_with(|| a.label.cmp(&b.label)));
    let god = all
        .iter()
        .filter(|n| n.degree > 0 && n.label != n.file)
        .take(GOD_SHOWN)
        .map(|n| (n.label.clone(), n.file.clone(), n.degree))
        .collect();
    let mut groups: BTreeMap<i64, Vec<&Node>> = BTreeMap::new();
    for node in &all {
        if let Some(community) = node.community {
            groups.entry(community).or_default().push(node);
        }
    }
    let mut communities: Vec<(usize, Vec<String>, String)> = groups
        .into_values()
        .map(|members| {
            let names = members.iter().take(3).map(|n| n.label.clone()).collect();
            let mut folders: HashMap<&str, usize> = HashMap::new();
            for member in &members {
                let folder = member.file.rsplit_once('/').map_or("", |(dir, _)| dir);
                *folders.entry(folder).or_default() += 1;
            }
            let folder = folders
                .into_iter()
                .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))
                .map(|(dir, _)| dir.to_owned())
                .unwrap_or_default();
            (members.len(), names, folder)
        })
        .collect();
    communities.sort_by_key(|c| std::cmp::Reverse(c.0));
    communities.truncate(COMMUNITIES_SHOWN);
    Some(GraphSummary { god, communities })
}

fn build(root: &Path) -> String {
    let tree = walk(root);
    let hot = hot_files(root);
    let graph = graph_summary(root);
    render(root, &tree, &hot, graph.as_ref())
}

fn render(
    root: &Path,
    tree: &Tree,
    hot: &[(String, usize)],
    graph: Option<&GraphSummary>,
) -> String {
    if tree.dirs.is_empty() && tree.entries.is_empty() && graph.is_none() {
        return String::new();
    }
    let name = root
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let mut text = format!(
        "<harness-map>\nMap of {name}{}: a starting point to search from, not a replacement for reading the \
files you change.\n",
        if graph.is_some() {
            " (file tree, git history and the Graphify graph)"
        } else {
            " (file tree and git history)"
        }
    );
    let mut dirs: Vec<(&String, &usize)> = tree.dirs.iter().collect();
    dirs.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    if !dirs.is_empty() {
        let _ = writeln!(
            text,
            "Folders (files{}):",
            if tree.partial { ", counted partly" } else { "" }
        );
        for (dir, count) in dirs.iter().take(DIRS_SHOWN) {
            let _ = write!(text, "- {dir}/ {count}");
            if let Some(subdirs) = tree.subdirs.get(*dir) {
                let mut subdirs: Vec<(&String, &usize)> = subdirs.iter().collect();
                subdirs.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
                let names: Vec<&str> = subdirs
                    .iter()
                    .take(SUBDIRS_SHOWN)
                    .map(|(n, _)| n.as_str())
                    .collect();
                if !names.is_empty() {
                    let more = if subdirs.len() > SUBDIRS_SHOWN {
                        ", …"
                    } else {
                        ""
                    };
                    let _ = write!(text, ": {}{more}", names.join(", "));
                }
            }
            text.push('\n');
        }
        if dirs.len() > DIRS_SHOWN {
            let _ = writeln!(text, "- … {} more folders", dirs.len() - DIRS_SHOWN);
        }
    }
    if !tree.entries.is_empty() {
        let shown: Vec<&str> = tree
            .entries
            .iter()
            .take(ENTRIES_SHOWN)
            .map(String::as_str)
            .collect();
        let _ = writeln!(text, "Entry points and manifests: {}", shown.join(", "));
    }
    if !hot.is_empty() {
        let shown: Vec<String> = hot.iter().map(|(p, n)| format!("{p} ({n})")).collect();
        let _ = writeln!(
            text,
            "Changed most in the last 300 commits: {}",
            shown.join(", ")
        );
    }
    if let Some(graph) = graph {
        if !graph.god.is_empty() {
            let shown: Vec<String> = graph
                .god
                .iter()
                .map(|(label, file, degree)| {
                    if file.is_empty() {
                        format!("{label} ({degree})")
                    } else {
                        format!("{label} [{file}] ({degree})")
                    }
                })
                .collect();
            let _ = writeln!(text, "Most connected symbols: {}", shown.join(", "));
        }
        if !graph.communities.is_empty() {
            text.push_str("Communities (size: main members, folder):\n");
            for (size, names, folder) in &graph.communities {
                let _ = write!(text, "- {size}: {}", names.join(", "));
                if !folder.is_empty() {
                    let _ = write!(text, " ({folder})");
                }
                text.push('\n');
            }
        }
    }
    text.push_str("</harness-map>");
    text
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("harness-map-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn maps_folders_entries_and_skips_dependencies() {
        let root = temp().join("demo");
        for file in [
            "Cargo.toml",
            "crates/core/Cargo.toml",
            "crates/core/src/lib.rs",
            "crates/core/src/a.rs",
            "crates/app/src/main.rs",
            "docs/guide.md",
            "node_modules/pkg/index.js",
            "target/debug/x.rs",
        ] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        }
        let tree = walk(&root);
        assert_eq!(tree.dirs.get("crates"), Some(&4));
        assert!(!tree.dirs.contains_key("node_modules") && !tree.dirs.contains_key("target"));
        assert_eq!(tree.entries.first().map(String::as_str), Some("Cargo.toml"));
        assert!(tree.entries.iter().any(|e| e == "crates/app/src/main.rs"));
        let text = render(&root, &tree, &[("crates/core/src/a.rs".into(), 4)], None);
        assert!(text.starts_with("<harness-map>\nMap of demo (file tree and git history)"));
        assert!(text.contains("- crates/ 4: core, app") || text.contains("- crates/ 4: app, core"));
        assert!(text.contains("crates/core/src/a.rs (4)") && text.ends_with("</harness-map>"));
        assert!(text.lines().count() <= 40);

        let home = temp();
        let workspace = root.to_string_lossy().into_owned();
        let block = prompt_block(&home, &workspace).unwrap();
        assert!(block.contains("crates/"));
        let cache = cache_path(&home, &workspace);
        let (stamp_now, _) = read_cache(&cache).unwrap();
        assert_eq!(stamp_now, stamp(&root));
        write_cache(&cache, &stamp_now, "<harness-map>\ncached\n</harness-map>");
        assert!(prompt_block(&home, &workspace).unwrap().contains("cached"));
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn summarizes_a_graph() {
        let graph: Value = serde_json::from_str(
            r#"{"nodes":[
                {"id":"hub","label":"Hub","source_file":"crates/hub/src/hub.rs","community":1},
                {"id":"session","label":"Session","source_file":"crates/hub/src/session.rs","community":1},
                {"id":"pool","label":"Pool","source_file":"crates/hub/src/pool.rs","community":1},
                {"id":"ui","label":"App","source_file":"apps/ui/main.rs","community":2}],
              "links":[{"source":"hub","target":"session"},{"source":"hub","target":"pool"},
                {"source":"ui","target":"hub"}]}"#,
        )
        .unwrap();
        let summary = summarize(&graph).unwrap();
        assert_eq!(
            summary.god[0],
            ("Hub".into(), "crates/hub/src/hub.rs".into(), 3)
        );
        assert_eq!(summary.communities[0].0, 3);
        assert_eq!(summary.communities[0].1[0], "Hub");
        assert_eq!(summary.communities[0].2, "crates/hub/src");
    }

    #[test]
    fn reads_heads_from_refs_and_packed_refs() {
        let root = temp();
        let git = root.join(".git");
        std::fs::create_dir_all(git.join("refs/heads")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(git.join("packed-refs"), "# pack\nabc123 refs/heads/main\n").unwrap();
        assert_eq!(git_head(&root).as_deref(), Some("abc123"));
        std::fs::write(git.join("refs/heads/main"), "def456\n").unwrap();
        assert_eq!(git_head(&root).as_deref(), Some("def456"));
        let _ = std::fs::remove_dir_all(root);
    }
}
