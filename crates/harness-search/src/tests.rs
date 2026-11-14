use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use tempfile::TempDir;

use crate::*;

fn write(root: &Path, rel: &str, content: &[u8]) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn fixture() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    write(r, ".gitignore", b"dist/\n*.log\n");
    write(
        r,
        "src/main.rs",
        b"fn main() {\n    println!(\"needle\");\n}\n",
    );
    write(
        r,
        "src/Ünïcode file.ts",
        "const 你好 = 'Needle';\r\nneedle again\n".as_bytes(),
    );
    write(r, "src/nested/deep.txt", b"no match here\n");
    write(r, "src/nested/.gitignore", b"deep.txt\n");
    write(r, "dist/bundle.js", b"needle");
    write(r, "debug.log", b"needle");
    write(r, "node_modules/pkg/index.js", b"needle");
    write(r, ".github/workflows/ci.yml", b"needle: true\n");
    write(r, ".env", b"SECRET=needle");
    write(r, "lib/native.dll", b"needle");
    write(
        r,
        "assets/blob.bin",
        &[0, 1, 2, b'n', b'e', b'e', b'd', b'l', b'e'],
    );
    write(r, "_underscore.md", b"x");
    write(r, "README.md", b"# needle\n");
    dir
}

fn root_str(dir: &TempDir) -> String {
    dir.path().to_string_lossy().into_owned()
}

fn rels(index: &WorkspaceIndex) -> Vec<String> {
    index
        .entries
        .iter()
        .map(|e| format!("{}:{}", e.kind.as_str(), e.relative_path))
        .collect()
}

#[test]
fn index_applies_ts_rules_and_order() {
    let dir = fixture();
    let index = build_index(&root_str(&dir), &CancellationToken::new()).unwrap();
    assert_eq!(
        rels(&index),
        [
            "directory:assets",
            "directory:lib",
            "directory:src",
            "directory:src/nested",
            "file:_underscore.md",
            "file:.github/workflows/ci.yml",
            "file:.gitignore",
            "file:assets/blob.bin",
            "file:README.md",
            "file:src/main.rs",
            "file:src/nested/.gitignore",
            "file:src/nested/deep.txt",
            "file:src/Ünïcode file.ts",
        ]
    );
    let packed = index.packed();
    assert!(packed.starts_with("directory\tassets\ndirectory\tlib\n"));
    assert_eq!(index.packed_len(), packed.encode_utf16().count());
    assert_eq!(index.packed_range(0, 9), "directory");
    assert_eq!(index.packed_range(index.packed_len(), 10), "");
}

#[test]
fn zcodeignore_replaces_template() {
    let dir = fixture();
    write(dir.path(), ".zcodeignore", b"SRC/\n");
    let index = build_index(&root_str(&dir), &CancellationToken::new()).unwrap();
    let all = rels(&index);
    // .zcodeignore 生效时内置排除退役：node_modules、dist 回到索引；src 被（大小写不敏感地）排除。
    assert!(all.contains(&"directory:node_modules".to_owned()));
    assert!(all.contains(&"file:dist/bundle.js".to_owned()));
    assert!(!all.iter().any(|e| e.contains("src")));
    assert!(!all.iter().any(|e| e.ends_with(".zcodeignore")));
}

#[test]
fn codec_roundtrip_with_escapes() {
    let entries = [
        (EntryType::File, "a\tb/c\\d"),
        (EntryType::Directory, "x\ny"),
    ];
    let packed = codec::pack(entries);
    assert_eq!(packed, "file\ta\\tb/c\\\\d\ndirectory\tx\\ny");
    let back = codec::unpack(&packed, "C:\\w");
    assert_eq!(back[0].relative_path, "a\tb/c\\d");
    assert_eq!(back[0].path, "C:\\w\\a\tb\\c\\d");
    assert_eq!(back[0].name, "c\\d");
    assert_eq!(back[1].kind, EntryType::Directory);
    let unix = codec::unpack("file\tsrc/a.rs", "/w/");
    assert_eq!(unix[0].path, "/w/src/a.rs");
}

#[test]
fn fuzzy_ranking_matches_ts() {
    let entries = vec![
        IndexEntry {
            kind: EntryType::Directory,
            relative_path: "src".into(),
        },
        IndexEntry {
            kind: EntryType::File,
            relative_path: "src/main.rs".into(),
        },
        IndexEntry {
            kind: EntryType::File,
            relative_path: "docs/domain.md".into(),
        },
        IndexEntry {
            kind: EntryType::File,
            relative_path: "main.rs".into(),
        },
        IndexEntry {
            kind: EntryType::File,
            relative_path: "x/m_a_i_n.txt".into(),
        },
    ];
    let index = WorkspaceIndex::from_sorted("/w", entries);
    let names = |q: &str, limit: usize| -> Vec<String> {
        search_entries(&index, "/w", q, limit)
            .into_iter()
            .map(|e| e.relative_path)
            .collect()
    };
    // 前缀命中 score = len 差；同分按原序。
    assert_eq!(
        names("main", 10),
        ["src/main.rs", "main.rs", "docs/domain.md", "x/m_a_i_n.txt"]
    );
    assert_eq!(names("main", 1), ["src/main.rs"]);
    // 空 query：文件优先、目录在后，组内原序。
    assert_eq!(names("  ", 10)[4], "src");
    assert_eq!(names("zzz", 10), Vec::<String>::new());
    assert_eq!(names("main", 0), Vec::<String>::new());
    let hit = &search_entries(&index, "/w", "MAIN.RS", 1)[0];
    assert_eq!(hit.path, "/w/src/main.rs");
    assert_eq!(hit.name, "main.rs");
}

#[test]
fn utf16_semantics() {
    use crate::utf16::*;
    let units: Vec<u16> = "a😀b".encode_utf16().collect();
    assert_eq!(slice_lossy(&units, 0, 2), "a\u{fffd}");
    assert_eq!(truncate_utf16("a😀b", 2), "a\u{fffd}");
    assert_eq!(truncate_utf16("a😀b", 3), "a😀");
    assert_eq!(js_trim("\u{feff} x \u{a0}"), "x");
    assert_eq!(index_of(&units, &[u16::from(b'b')], 0), Some(3));
}

#[test]
fn cache_shares_and_refreshes() {
    let dir = fixture();
    let root = root_str(&dir);
    let cache = IndexCache::new();
    let token = CancellationToken::new();
    let a = cache.get(&root, Some("  id-1 "), false, &token).unwrap();
    let b = cache.get(&root, Some("id-1"), false, &token).unwrap();
    assert!(std::sync::Arc::ptr_eq(&a, &b));
    write(dir.path(), "new.txt", b"x");
    let c = cache.get(&root, Some("id-1"), true, &token).unwrap();
    assert!(!std::sync::Arc::ptr_eq(&a, &c));
    assert!(c.entries.iter().any(|e| e.relative_path == "new.txt"));
    // .zcodeignore 指纹变化使缓存失效。
    write(dir.path(), ".zcodeignore", b"*.txt\n");
    let d = cache.get(&root, Some("id-1"), false, &token).unwrap();
    assert!(!d.entries.iter().any(|e| e.relative_path == "new.txt"));
}

fn run_text(
    index: &WorkspaceIndex,
    root: &str,
    opts: &TextSearchOptions,
) -> (Vec<TextMatch>, TextSearchSummary, usize) {
    let mut all = Vec::new();
    let mut batches = 0;
    let summary = search_text(index, root, opts, &CancellationToken::new(), &mut |m| {
        batches += 1;
        all.extend(m);
    })
    .unwrap();
    (all, summary, batches)
}

fn opts(query: &str) -> TextSearchOptions {
    TextSearchOptions {
        query: query.into(),
        is_regex: false,
        case_sensitive: false,
        max_results: 2000,
        max_file_bytes: 1024 * 1024,
    }
}

#[test]
fn text_search_order_columns_and_skips() {
    let dir = fixture();
    let root = root_str(&dir);
    let index = build_index(&root, &CancellationToken::new()).unwrap();
    let (matches, summary, _) = run_text(&index, &root, &opts("needle"));
    let found: Vec<_> = matches
        .iter()
        .map(|m| format!("{}:{}:{}", m.relative_path, m.line, m.column))
        .collect();
    assert_eq!(
        found,
        [
            ".github/workflows/ci.yml:1:1",
            "README.md:1:3",
            "src/main.rs:2:15",
            "src/Ünïcode file.ts:1:13",
            "src/Ünïcode file.ts:2:1",
        ]
    );
    assert_eq!(matches[3].text, "const 你好 = 'Needle';");
    assert_eq!(summary.match_count, 5);
    assert_eq!(summary.file_count, 4);
    assert!(!summary.truncated);
    let mut sensitive = opts("Needle");
    sensitive.case_sensitive = true;
    assert_eq!(run_text(&index, &root, &sensitive).1.match_count, 1);
    let mut regex = opts(r"need\w+ again");
    regex.is_regex = true;
    assert_eq!(run_text(&index, &root, &regex).1.match_count, 1);
    let mut capped = opts("needle");
    capped.max_results = 2;
    let (m, s, _) = run_text(&index, &root, &capped);
    assert_eq!(m.len(), 2);
    assert!(s.truncated);
    let mut bad = opts("(");
    bad.is_regex = true;
    assert!(matches!(
        search_text(&index, &root, &bad, &CancellationToken::new(), &mut |_| {}),
        Err(SearchError::InvalidPattern(_))
    ));
}

#[test]
fn text_search_large_file_and_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let mut big = String::new();
    for i in 0..3000 {
        let _ = writeln!(big, "line {i} hit");
    }
    write(dir.path(), "big.txt", big.as_bytes());
    write(dir.path(), "huge.txt", &vec![b'x'; 2 * 1024 * 1024]);
    let root = root_str(&dir);
    let index = build_index(&root, &CancellationToken::new()).unwrap();
    let (m, s, batches) = run_text(&index, &root, &opts("hit"));
    assert_eq!(m.len(), 2000);
    assert!(s.truncated);
    assert!(batches >= 10, "progress must be batched by 200");
    let mut long = opts("x");
    long.max_file_bytes = 4 * 1024 * 1024;
    let (m, _, _) = run_text(&index, &root, &long);
    assert_eq!(m[0].text.len(), 500);
    let token = CancellationToken::new();
    token.cancel();
    assert!(matches!(
        search_text(&index, &root, &opts("hit"), &token, &mut |_| {}),
        Err(SearchError::Cancelled)
    ));
}

#[test]
fn template_dedups_builtin_lines() {
    let template = ignore_rules::build_template(Some("node_modules\n/dist/\n"));
    assert!(!template.contains("\nnode_modules/\n"));
    assert!(template.contains("\nvenv/\n"));
    let rules = ignore_rules::IgnoreRules::from_content("Build/\n!keep.log\n*.log\n");
    assert!(rules.is_ignored("build", true));
    assert!(!rules.is_ignored("build", false));
    assert!(rules.is_ignored("a/x.LOG", false));
}

#[cfg(unix)]
#[test]
fn symlinked_dirs_listed_not_traversed() {
    let dir = fixture();
    std::os::unix::fs::symlink(dir.path().join("src"), dir.path().join("link")).unwrap();
    let index = build_index(&root_str(&dir), &CancellationToken::new()).unwrap();
    let all = rels(&index);
    assert!(all.contains(&"directory:link".to_owned()));
    assert!(!all.iter().any(|e| e.contains("link/")));
}
