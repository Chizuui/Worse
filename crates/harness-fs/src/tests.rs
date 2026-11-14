use std::cmp::Ordering;
use std::fs;
use std::path::Path;

use tempfile::TempDir;

use crate::*;

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn fixture() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("b file.txt"), "hello").unwrap();
    fs::write(dir.path().join("Ärger 你好.md"), "unicode ✓").unwrap();
    fs::write(dir.path().join(".hidden"), "x").unwrap();
    fs::create_dir(dir.path().join("sub dir")).unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
    dir
}

#[test]
fn stat_file_and_directory() {
    let dir = fixture();
    let file = s(&dir.path().join("b file.txt"));
    let st = stat(&file).unwrap();
    assert_eq!(st.kind, EntryKind::File);
    assert_eq!(st.size, Some(5));
    assert!(st.mtime_ms.unwrap() > 0.0);
    assert_eq!(st.path, file);
    let d = stat(&s(dir.path())).unwrap();
    assert_eq!(d.kind, EntryKind::Directory);
    assert!(d.size.is_none());
    let json = serde_json::to_value(&d).unwrap();
    assert_eq!(json["type"], "directory");
    assert!(json.get("size").is_none());
    let err = stat(&s(&dir.path().join("missing"))).unwrap_err();
    assert_eq!(err.errno(), Some("ENOENT"));
    assert!(err
        .to_string()
        .starts_with("ENOENT: no such file or directory, stat '"));
}

#[test]
fn readdir_sorts_and_hides() {
    let dir = fixture();
    let root = s(dir.path());
    let visible = readdir(&root, false).unwrap();
    let names: Vec<_> = visible.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["sub dir", "Ärger 你好.md", "b file.txt", "bin.dat"]);
    assert_eq!(visible[0].kind, EntryKind::Directory);
    assert_eq!(visible[2].path, node_join(&root, "b file.txt"));
    let all = readdir(&root, true).unwrap();
    assert_eq!(all.len(), 6);
    assert_eq!(all[0].name, ".git");
}

#[cfg(unix)]
#[test]
fn readdir_follows_symlinked_directory() {
    let dir = fixture();
    std::os::unix::fs::symlink(dir.path().join("sub dir"), dir.path().join("link")).unwrap();
    let entries = readdir(&s(dir.path()), false).unwrap();
    let link = entries.iter().find(|e| e.name == "link").unwrap();
    assert!(link.is_symbolic_link);
    assert_eq!(link.kind, EntryKind::Directory);
}

#[test]
fn read_text_windows_and_binary() {
    let dir = fixture();
    let file = s(&dir.path().join("Ärger 你好.md"));
    let all = read_text(&file, None, None).unwrap();
    assert_eq!(all.content, "unicode ✓");
    assert!(!all.truncated && !all.is_binary);
    let part = read_text(&file, Some(2.0), Some(3.0)).unwrap();
    assert_eq!(part.content, "ico");
    assert!(part.truncated);
    let eof = read_text(&file, Some(1000.0), None).unwrap();
    assert_eq!(eof.bytes_read, 0);
    let bin = read_text(&s(&dir.path().join("bin.dat")), None, None).unwrap();
    assert!(bin.is_binary);
    assert_eq!(bin.content, "");
    let err = read_text(&s(dir.path()), None, None).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("Path is not a file: {}", s(dir.path()))
    );
}

#[test]
fn read_text_clamps_large_files() {
    let dir = tempfile::tempdir().unwrap();
    let big = dir.path().join("big.txt");
    fs::write(&big, vec![b'a'; 600 * 1024]).unwrap();
    let slice = read_text(&s(&big), None, Some(10.0 * 1024.0 * 1024.0)).unwrap();
    assert_eq!(slice.bytes_read, MAX_TEXT_READ_BYTES);
    assert!(slice.truncated);
    let range = read_range(&s(&big), Some(599.0 * 1024.0), Some(4096.0)).unwrap();
    assert_eq!(range.len(), 1024);
    assert_eq!(
        read_range(&s(&big), Some(700.0 * 1024.0), Some(10.0)).unwrap(),
        Vec::<u8>::new()
    );
    let err = read_bytes_limited(&s(&big), 1024).unwrap_err();
    assert_eq!(
        err,
        FsError::app(format!("File is too large to preview: {}", s(&big)))
    );
    assert_eq!(clamp_js_length(Some(0.5), 10, 20), 1);
    assert_eq!(clamp_js_length(None, 10, 20), 10);
    assert_eq!(js_offset(Some(-3.0)), 0);
}

#[test]
fn atomic_write_replaces_and_leaves_no_temp() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("nested dir").join("ünï.json");
    fs::create_dir(target.parent().unwrap()).unwrap();
    atomic_write(&s(&target), b"one", Some(0o600)).unwrap();
    atomic_write(&s(&target), b"two", None).unwrap();
    assert_eq!(fs::read_to_string(&target).unwrap(), "two");
    let leftovers: Vec<_> = fs::read_dir(target.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(leftovers.len(), 1);
    let missing = dir.path().join("no-such-dir").join("x");
    let err = atomic_write(&s(&missing), b"x", None).unwrap_err();
    assert_eq!(err.errno(), Some("ENOENT"));
}

#[test]
fn mkdir_remove_rename() {
    let dir = tempfile::tempdir().unwrap();
    let a = s(&dir.path().join("a").join("b"));
    assert!(mkdir(&a, true).unwrap());
    assert!(!mkdir(&a, true).unwrap());
    let f = s(&dir.path().join("a").join("f.txt"));
    write_file(&f, b"x", false).unwrap();
    let g = s(&dir.path().join("a").join("g.txt"));
    rename(&f, &g).unwrap();
    assert!(Path::new(&g).exists());
    let parent = s(&dir.path().join("a"));
    assert_eq!(
        remove(&parent, false, false).unwrap_err().errno(),
        Some("ENOTEMPTY")
    );
    remove(&parent, true, false).unwrap();
    remove(&parent, true, true).unwrap();
    assert_eq!(
        remove(&parent, true, false).unwrap_err().errno(),
        Some("ENOENT")
    );
    let keep = s(&dir.path().join("keep.txt"));
    write_file(&keep, b"k", false).unwrap();
    let exists = check_files_exist(&[g.clone(), s(dir.path()), keep]).unwrap();
    // 目录不算存在的文件（TS 使用 stat().isFile()）。
    assert!(!exists[0].exists && !exists[1].exists && exists[2].exists);
    assert!(check_files_exist(&vec![g; 16]).is_err());
}

#[test]
fn verbatim_and_identity() {
    assert_eq!(strip_verbatim(r"\\?\C:\Work\x"), r"C:\Work\x");
    assert_eq!(strip_verbatim(r"\\?\UNC\srv\share\x"), r"\\srv\share\x");
    assert_eq!(strip_verbatim(r"\\?\Volume{1}\x"), r"\\?\Volume{1}\x");
    let ci = CaseSensitivity::Insensitive;
    let cs = CaseSensitivity::Sensitive;
    assert_eq!(
        identity_key(Path::new("/Repo/./a/../B/"), ci),
        identity_key(Path::new("/repo/b"), ci)
    );
    assert_ne!(
        identity_key(Path::new("/Repo/B"), cs),
        identity_key(Path::new("/repo/b"), cs)
    );
    assert!(is_within(Path::new("/repo"), Path::new("/REPO/x/y"), ci));
    assert!(!is_within(Path::new("/repo"), Path::new("/repo2/x"), ci));
    assert!(!is_within(
        Path::new("/repo"),
        Path::new("/repo/../etc"),
        ci
    ));
}

#[cfg(windows)]
#[test]
fn windows_identity_and_node_join() {
    let ci = CaseSensitivity::platform_default();
    assert_eq!(
        identity_key(Path::new(r"\\?\C:\Work\Repo\"), ci),
        identity_key(Path::new("c:/work/repo"), ci)
    );
    assert!(is_within(
        Path::new(r"C:\Work"),
        Path::new(r"\\?\c:\WORK\a"),
        ci
    ));
    assert_eq!(node_join(r"C:\a\", "b"), r"C:\a\b");
    assert_eq!(node_join("C:/a/x/..", "b"), r"C:\a\b");
    assert_eq!(node_join(r"\\srv\share", "b"), r"\\srv\share\b");
}

#[cfg(unix)]
#[test]
fn unix_node_join() {
    assert_eq!(node_join("/a/", "b"), "/a/b");
    assert_eq!(node_join("/a//x/..", "b"), "/a/b");
    assert_eq!(node_join("/", ".."), "/");
}

#[test]
fn workspace_root_policy() {
    let dir = fixture();
    let root = WorkspaceRoot::new(dir.path(), SymlinkPolicy::DenyEscape).unwrap();
    assert!(root.resolve(Path::new("sub dir/new.txt")).is_ok());
    assert_eq!(
        root.resolve(Path::new("../outside")).unwrap_err().errno(),
        None
    );
    let outside = tempfile::tempdir().unwrap();
    let link = dir.path().join("escape");
    let linked = make_dir_link(outside.path(), &link);
    if linked {
        assert_eq!(root.resolve(&link.join("x")).unwrap_err().errno(), None);
        let allow = WorkspaceRoot::new(dir.path(), SymlinkPolicy::AllowEscape).unwrap();
        assert!(allow.resolve(&link.join("x")).is_ok());
    }
}

#[cfg(unix)]
fn make_dir_link(target: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).is_ok()
}

#[cfg(windows)]
fn make_dir_link(target: &Path, link: &Path) -> bool {
    // 目录软链接在未开启开发者模式的 Windows 上需要管理员权限；改用 junction 覆盖逃逸场景。
    std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn binary_sniff_matches_ts() {
    assert!(!is_probably_binary(b""));
    assert!(!is_probably_binary(b"hello\r\n\tworld\x0c"));
    assert!(is_probably_binary(b"a\0b"));
    assert!(is_probably_binary(&[1, 2, 3, b'a']));
    assert!(!is_probably_binary(&[1, b'a', b'b', b'c']));
}

#[test]
fn locale_compare_like_icu() {
    let mut names = vec!["b", "B", "a", "_x", "10", "2", "Z", "é", "e", "-y", "ab"];
    names.sort_by(|a, b| locale_compare(a, b));
    assert_eq!(
        names,
        ["_x", "-y", "10", "2", "a", "ab", "b", "B", "e", "é", "Z"]
    );
    assert_eq!(locale_compare("a", "a"), Ordering::Equal);
}

#[test]
fn media_types() {
    assert_eq!(media_type_for_path(r"C:\x.dir\Pic.PNG"), "image/png");
    assert_eq!(media_type_for_path("/a/clip.MOV"), "video/quicktime");
    assert_eq!(media_type_for_path("/a/.png"), "application/octet-stream");
    assert_eq!(media_type_for_path("/a/x.bin"), "application/octet-stream");
}
