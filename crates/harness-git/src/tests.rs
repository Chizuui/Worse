//! Unit tests: parsers on canned output and the full read path on temp
//! repositories built with the git CLI.

use std::path::Path;
use std::process::Command;

use crate::parse::{parse_graph_records, parse_numstat, parse_status_porcelain};
use crate::runner::never_cancelled;
use crate::types::{
    ChangeKind, ChangeSection, DiffAvailability, GraphRefKind, HeadRefType, WorkspaceRepositoryKind,
};
use crate::{ChangeSource, DiffQuery, GitError, GitService};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn write(dir: &Path, rel: &str, content: &[u8]) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "Test User"]);
    git(dir, &["config", "user.email", "test@example.invalid"]);
    git(dir, &["config", "core.autocrlf", "false"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
}

fn s(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn same_path(a: &str, b: &Path) -> bool {
    let canon = |p: &Path| {
        harness_fs::realpath(&p.to_string_lossy())
            .unwrap()
            .replace('\\', "/")
            .to_lowercase()
    };
    canon(Path::new(a)) == canon(b)
}

/// Builds: committed `a.txt`, `pkg/b.ts`, `ren_old.txt`; then modifies `a.txt`
/// (unstaged), stages `pkg/b.ts`, renames `ren_old.txt` (staged), adds an
/// untracked text file, an untracked empty file and an untracked binary.
fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_repo(dir);
    write(dir, "a.txt", b"one\ntwo\n");
    write(dir, "pkg/b.ts", b"export {}\n");
    write(dir, "ren_old.txt", b"rename me\nline2\nline3\n");
    write(dir, ".gitignore", b"*.log\n");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "initial"]);
    write(dir, "a.txt", b"one\nTWO\nthree\n");
    write(dir, "pkg/b.ts", b"export const x = 1;\nexport {}\n");
    git(dir, &["add", "pkg/b.ts"]);
    git(dir, &["mv", "ren_old.txt", "ren new.txt"]);
    write(dir, "pkg/new file ü.md", b"hello\nworld");
    write(dir, "empty.txt", b"");
    write(dir, "blob.bin", b"\0\x01\x02");
    write(dir, "debug.log", b"ignored\n");
    tmp
}

#[test]
fn parses_porcelain_v2() {
    let out =
        "# branch.oid abc\0# branch.head feat\0# branch.upstream origin/feat\0# branch.ab +2 -3\0\
1 .M N... 100644 100644 100644 h1 h2 a b.txt\0\
2 R. N... 100644 100644 100644 h1 h2 R100 new.txt\0old.txt\0\
u UU N... 100644 100644 100644 100644 h1 h2 h3 c.txt\0? d dir/\0";
    let parsed = parse_status_porcelain(out);
    assert_eq!(parsed.branch_name.as_deref(), Some("feat"));
    assert_eq!(parsed.tracking_branch_name.as_deref(), Some("origin/feat"));
    assert_eq!((parsed.ahead, parsed.behind), (2, 3));
    assert_eq!(parsed.entries.len(), 4);
    assert_eq!(parsed.entries[0].path, "a b.txt");
    assert_eq!(parsed.entries[0].kind, ChangeKind::Modified);
    assert_eq!(parsed.entries[1].original_path.as_deref(), Some("old.txt"));
    assert_eq!(parsed.entries[1].kind, ChangeKind::Renamed);
    assert!(parsed.entries[2].is_conflicted);
    assert_eq!(parsed.entries[3].path, "d dir/");
    let detached = parse_status_porcelain("# branch.head (detached)\0");
    assert_eq!(detached.head_ref_type, HeadRefType::Detached);
    assert_eq!(detached.branch_name, None);
}

#[test]
fn parses_numstat_and_graph() {
    let stats = parse_numstat("3\t1\ta.txt\0-\t-\tbin.dat\0\x30\t0\t\0old.txt\0new.txt\0");
    let rows: Vec<_> = stats
        .iter()
        .map(|(k, v)| (k.clone(), v.added, v.removed, v.kind))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("a.txt".to_owned(), 3, 1, None),
            ("bin.dat".to_owned(), 0, 0, None),
            ("new.txt".to_owned(), 0, 0, Some(ChangeKind::Renamed)),
        ]
    );
    let graph = parse_graph_records(
        "h1\0p1 p2\0Ann\x001700000000\0subj\0HEAD -> main, tag: v1, origin/main, refs/heads/x\x1e\nh2\0\0\0\0s2\0\x1e\n",
    );
    assert_eq!(graph.len(), 2);
    assert_eq!(graph[0].parents, vec!["p1", "p2"]);
    assert_eq!(graph[0].authored_at_ms, Some(1_700_000_000_000));
    let refs: Vec<_> = graph[0]
        .refs
        .iter()
        .map(|r| (r.name.as_str(), r.kind))
        .collect();
    assert_eq!(
        refs,
        vec![
            ("HEAD", GraphRefKind::Head),
            ("main", GraphRefKind::Branch),
            ("v1", GraphRefKind::Tag),
            ("origin/main", GraphRefKind::Remote),
            ("x", GraphRefKind::Branch),
        ]
    );
    assert_eq!(graph[1].author_name, None);
    assert_eq!(graph[1].authored_at_ms, None);
}

#[test]
fn status_changes_and_summary() {
    let tmp = fixture();
    let dir = tmp.path();
    let service = GitService::new();
    let summary = service
        .repository_summary(&s(dir), &never_cancelled)
        .unwrap();
    assert!(summary.is_repository && summary.is_git_available && summary.is_dirty);
    assert_eq!(summary.branch_name.as_deref(), Some("main"));
    assert_eq!(summary.workspace_in_repo_path, ".");
    assert!(same_path(&summary.repo_root, dir));
    assert_ne!(summary.auto_refresh_watch_paths.len(), 0);

    let unstaged = service
        .changes(&s(dir), ChangeSource::Unstaged, &never_cancelled)
        .unwrap();
    let rows: Vec<_> = unstaged
        .iter()
        .map(|c| (c.repo_relative_path.as_str(), c.section, c.added, c.removed))
        .collect();
    // empty.txt (+0/-0) and blob.bin (binary, 0 lines) are filtered; debug.log is ignored.
    assert_eq!(
        rows,
        vec![
            ("a.txt", ChangeSection::Unstaged, 2, 1),
            ("pkg/new file ü.md", ChangeSection::Untracked, 2, 0),
        ]
    );
    let a = &unstaged[0];
    assert!(a
        .path
        .ends_with(if cfg!(windows) { "\\a.txt" } else { "/a.txt" }));
    assert_eq!(a.x.as_deref(), Some("."));
    let json = serde_json::to_value(&unstaged[1]).unwrap();
    assert!(
        json.get("x").is_none(),
        "untracked x must be omitted: {json}"
    );
    assert_eq!(json["y"], "?");

    let staged = service
        .changes(&s(dir), ChangeSource::Staged, &never_cancelled)
        .unwrap();
    let rows: Vec<_> = staged
        .iter()
        .map(|c| (c.repo_relative_path.as_str(), c.kind, c.added, c.removed))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("pkg/b.ts", ChangeKind::Modified, 1, 0),
            ("ren new.txt", ChangeKind::Renamed, 0, 0),
        ]
    );
}

#[test]
fn subdirectory_workspace_is_scoped() {
    let tmp = fixture();
    let pkg = tmp.path().join("pkg");
    let service = GitService::new();
    let refresh = service
        .refresh(&s(&pkg), true, true, &never_cancelled)
        .unwrap();
    assert_eq!(refresh.summary.workspace_in_repo_path, "pkg");
    let unstaged: Vec<_> = refresh
        .unstaged_changes
        .iter()
        .map(|c| c.workspace_relative_path.as_str())
        .collect();
    assert_eq!(unstaged, vec!["new file ü.md"]);
    let staged: Vec<_> = refresh
        .staged_changes
        .iter()
        .map(|c| c.workspace_relative_path.as_str())
        .collect();
    assert_eq!(staged, vec!["b.ts"]);
    let identity = refresh.identity.unwrap();
    assert_eq!(identity.user_name.as_deref(), Some("Test User"));
    assert_eq!(identity.scope_label.as_deref(), Some("local"));
    // No upstream: empty comparison with headRef = branch.
    let comparison = refresh.branch_comparison.unwrap();
    assert_eq!(comparison.base_ref, None);
    assert_eq!(comparison.head_ref.as_deref(), Some("main"));
    assert_eq!(comparison.changes.len(), 0);
}

#[test]
fn diffs_for_each_source() {
    let tmp = fixture();
    let dir = tmp.path();
    let service = GitService::new();
    let query = |path: &str, source: Option<&str>| DiffQuery {
        workspace_path: s(dir),
        path: path.to_owned(),
        staged: None,
        source_id: source.map(str::to_owned),
    };
    let unstaged = service
        .diff(&query("a.txt", None), &never_cancelled)
        .unwrap();
    assert_eq!(unstaged.availability, DiffAvailability::Patch);
    assert!(unstaged.patch.as_deref().unwrap().contains("+TWO"));
    assert_eq!(unstaged.before_content.as_deref(), Some("one\ntwo\n"));
    assert_eq!(unstaged.after_content.as_deref(), Some("one\nTWO\nthree\n"));
    assert_eq!(unstaged.summary, None);

    let staged = service
        .diff(&query("pkg/b.ts", Some("staged")), &never_cancelled)
        .unwrap();
    assert_eq!(staged.availability, DiffAvailability::Patch);
    assert_eq!(staged.before_content.as_deref(), Some("export {}\n"));

    let untracked = service
        .diff(&query("pkg/new file ü.md", None), &never_cancelled)
        .unwrap();
    assert_eq!(
        untracked.patch.as_deref(),
        Some("--- /dev/null\n+++ b/pkg/new file ü.md\n@@ -0,0 +1,2 @@\n+hello\n+world\n\\ No newline at end of file\n")
    );
    assert_eq!(untracked.before_content.as_deref(), Some(""));

    let binary = service
        .diff(&query("blob.bin", None), &never_cancelled)
        .unwrap();
    assert_eq!(binary.availability, DiffAvailability::Binary);
    assert_eq!(
        binary.summary.as_deref(),
        Some("Binary diff is not previewable.")
    );

    let missing = service
        .diff(&query("nope.txt", None), &never_cancelled)
        .unwrap();
    assert_eq!(missing.availability, DiffAvailability::Unavailable);
    assert_eq!(
        missing.summary.as_deref(),
        Some("No Git diff is available for this file.")
    );

    let branch = service
        .diff(&query("a.txt", Some("branch")), &never_cancelled)
        .unwrap();
    assert_eq!(
        branch.summary.as_deref(),
        Some("Current branch does not have an upstream branch.")
    );

    let outside = service.diff(&query("../outside.txt", None), &never_cancelled);
    assert!(
        matches!(outside, Err(GitError::App(m)) if m == "Path is outside repository scope: ../outside.txt")
    );
}

#[test]
fn graph_branches_ignored_and_worktree() {
    let tmp = fixture();
    let dir = tmp.path();
    git(dir, &["commit", "-q", "-m", "second"]);
    git(dir, &["tag", "v1"]);
    git(dir, &["branch", "side"]);
    let service = GitService::new();

    let graph = service
        .commit_graph(&s(dir), Some(1.0), None, &never_cancelled)
        .unwrap();
    assert_eq!(graph.commits.len(), 1);
    assert!(graph.has_more);
    assert_eq!(graph.commits[0].subject, "second");
    let kinds: Vec<_> = graph.commits[0]
        .refs
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(kinds, vec!["HEAD", "main", "v1", "side"]);
    let page2 = service
        .commit_graph(&s(dir), Some(5.0), Some(1.0), &never_cancelled)
        .unwrap();
    assert_eq!(page2.commits.len(), 1);
    assert!(!page2.has_more);

    let branches = service.local_branches(&s(dir), &never_cancelled).unwrap();
    let names: Vec<_> = branches
        .branches
        .iter()
        .map(|b| (b.name.as_str(), b.is_current))
        .collect();
    assert_eq!(names, vec![("main", true), ("side", false)]);
    assert!(branches.branches[0].commit_timestamp_ms.is_some());

    let ignored = service
        .ignored_paths(
            &s(dir),
            &[
                "debug.log".to_owned(),
                "a.txt".to_owned(),
                "../x".to_owned(),
            ],
            &never_cancelled,
        )
        .unwrap();
    assert_eq!(ignored.len(), 1);
    assert!(ignored[0].ends_with("debug.log"));

    let info = service
        .workspace_repository_info(&s(dir), &never_cancelled)
        .unwrap();
    assert_eq!(info.kind, WorkspaceRepositoryKind::MainTree);
    let wt = tmp.path().join("wt");
    git(dir, &["worktree", "add", "-q", &s(&wt), "side"]);
    let info = service
        .workspace_repository_info(&s(&wt), &never_cancelled)
        .unwrap();
    assert_eq!(info.kind, WorkspaceRepositoryKind::LinkedWorktree);
}

#[test]
fn non_repository_and_missing_git() {
    let tmp = tempfile::tempdir().unwrap();
    let service = GitService::new();
    let summary = service
        .repository_summary(&s(tmp.path()), &never_cancelled)
        .unwrap();
    assert!(summary.is_git_available);
    assert!(!summary.is_repository);
    assert_eq!(summary.repo_root, s(tmp.path()));
    assert_eq!(summary.workspace_in_repo_path, ".");

    let missing = tmp.path().join("does-not-exist");
    let summary = service
        .repository_summary(&s(&missing), &never_cancelled)
        .unwrap();
    assert!(!summary.is_repository);

    let graph = service
        .commit_graph(&s(tmp.path()), None, None, &never_cancelled)
        .unwrap();
    assert_eq!(graph.commits.len(), 0);

    let no_git = GitService::with_binary(None);
    let summary = no_git
        .repository_summary(&s(tmp.path()), &never_cancelled)
        .unwrap();
    assert!(!summary.is_git_available);
    let diff = no_git
        .diff(
            &DiffQuery {
                workspace_path: s(tmp.path()),
                path: "x".to_owned(),
                ..DiffQuery::default()
            },
            &never_cancelled,
        )
        .unwrap();
    assert_eq!(
        diff.summary.as_deref(),
        Some("Git binary is not available in the current environment.")
    );
    let json =
        serde_json::to_value(no_git.identity(&s(tmp.path()), &never_cancelled).unwrap()).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"userName":null,"userEmail":null,"nameSource":null,"emailSource":null,"scopeLabel":null})
    );
}

#[test]
fn empty_repository_has_empty_graph() {
    let tmp = tempfile::tempdir().unwrap();
    init_repo(tmp.path());
    let service = GitService::new();
    let graph = service
        .commit_graph(&s(tmp.path()), None, None, &never_cancelled)
        .unwrap();
    assert_eq!(graph.commits.len(), 0);
    let branches = service
        .local_branches(&s(tmp.path()), &never_cancelled)
        .unwrap();
    assert_eq!(branches.branches.len(), 0);
    assert_eq!(branches.current_branch_name.as_deref(), Some("main"));
}

#[test]
fn cancellation_kills_command() {
    let tmp = fixture();
    let service = GitService::new();
    let result = service.repository_summary(&s(tmp.path()), &|| true);
    assert_eq!(result.unwrap_err(), GitError::Cancelled);
}

/// Long-running non-git command for the runner's timeout/cancel paths.
fn sleeper() -> (String, Vec<&'static str>) {
    if cfg!(windows) {
        ("ping".to_owned(), vec!["-n", "6", "127.0.0.1"])
    } else {
        ("sleep".to_owned(), vec!["5"])
    }
}

#[test]
fn runner_truncates_times_out_and_cancels_mid_run() {
    use crate::runner::GitRunner;
    use std::time::{Duration, Instant};

    let tmp = tempfile::tempdir().unwrap();
    let cwd = s(tmp.path());
    let git_runner = GitRunner::new();
    let ok = git_runner
        .run(
            &cwd,
            &["--version"],
            Duration::from_secs(10),
            1024,
            &never_cancelled,
        )
        .unwrap();
    assert_eq!(ok.exit_code, Some(0));
    assert!(ok.stdout.starts_with("git version"));
    let truncated = git_runner
        .run(
            &cwd,
            &["--version"],
            Duration::from_secs(10),
            4,
            &never_cancelled,
        )
        .unwrap();
    assert!(truncated.output_truncated);
    assert_eq!(truncated.exit_code, None);

    let (binary, args) = sleeper();
    let runner = GitRunner::with_binary(Some(binary));
    let started = Instant::now();
    let timed_out = runner
        .run(
            &cwd,
            &args,
            Duration::from_millis(200),
            1024,
            &never_cancelled,
        )
        .unwrap();
    assert!(timed_out.timed_out);
    assert_eq!(timed_out.exit_code, None);
    assert!(started.elapsed() < Duration::from_secs(4));

    let started = Instant::now();
    let cancel = move || started.elapsed() > Duration::from_millis(100);
    let cancelled = runner.run(&cwd, &args, Duration::from_secs(30), 1024, &cancel);
    assert_eq!(cancelled.unwrap_err(), GitError::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[test]
fn branch_diff_and_refresh_share_resolution() {
    let tmp = fixture();
    let dir = tmp.path();
    git(dir, &["branch", "base"]);
    git(dir, &["branch", "--set-upstream-to=base"]);
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "second"]);
    let service = GitService::new();
    let branch = service
        .diff(
            &DiffQuery {
                workspace_path: s(dir),
                path: "a.txt".to_owned(),
                staged: None,
                source_id: Some("branch".to_owned()),
            },
            &never_cancelled,
        )
        .unwrap();
    assert_eq!(branch.availability, DiffAvailability::Patch);
    assert_eq!(branch.before_content.as_deref(), Some("one\ntwo\n"));
    assert_eq!(branch.after_content.as_deref(), Some("one\nTWO\nthree\n"));

    let refresh = service
        .refresh(&s(dir), true, true, &never_cancelled)
        .unwrap();
    assert_eq!(
        refresh.summary.tracking_branch_name.as_deref(),
        Some("base")
    );
    let identity = refresh.identity.unwrap();
    assert_eq!(identity.user_name.as_deref(), Some("Test User"));
    let comparison = refresh.branch_comparison.unwrap();
    assert!(comparison.changes.iter().any(|c| c.path.ends_with("a.txt")));
    let branches = service.local_branches(&s(dir), &never_cancelled).unwrap();
    assert_eq!(branches.current_branch_name.as_deref(), Some("main"));
    assert_eq!(branches.branches.len(), 2);
}
