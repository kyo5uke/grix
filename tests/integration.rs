//! End-to-end tests over real temp directories.
//!
//! The core property: searching WITH the index returns exactly the same
//! (path, line) set as a full walk-scan, for every pattern shape the
//! planner handles. The index must only ever narrow work, never change
//! results.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use grix::index::build::{self, BuildOptions};
use grix::index::format::IndexReader;
use grix::search::{self, SearchOptions};

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    index_path: PathBuf,
}

fn write(root: &Path, rel: &str, content: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    write(
        &root,
        "src/main.rs",
        b"fn main() {\n    println!(\"hello grix\");\n}\n",
    );
    write(
        &root,
        "src/lib.rs",
        b"pub fn foo() -> u32 { 42 }\npub fn foobar() {}\n// TODO: cleanup\n",
    );
    write(
        &root,
        "docs/guide.md",
        b"Searching with foo and bar.\nfoo bar baz\nFOO IN CAPS\n",
    );
    write(
        &root,
        "data/crlf.txt",
        b"alpha foo\r\nbeta\r\nfoo gamma\r\n",
    );
    write(
        &root,
        "data/unicode.txt",
        "日本語のテスト foo 行\nfooの行\n".as_bytes(),
    );
    write(&root, "data/binary.bin", b"\x00\x01\x02foo\x00bar");
    write(&root, "deep/a/b/c/needle.txt", b"the deep needle foo\n");
    // Large file: exceeds the tiny test cap -> scan-always path.
    let mut big = Vec::new();
    for i in 0..200 {
        big.extend_from_slice(format!("filler line {i} with foo inside\n").as_bytes());
    }
    write(&root, "data/big.log", &big);
    // Ignored file must not be searched. Like ripgrep, .gitignore only
    // applies inside a git repository, so give the fixture a .git dir.
    std::fs::create_dir(root.join(".git")).unwrap();
    write(&root, ".gitignore", b"ignored.txt\n");
    write(&root, "ignored.txt", b"foo should never appear\n");

    let index_path = root.join(".grix-test.gix");
    Fixture {
        _dir: dir,
        root,
        index_path,
    }
}

fn opts_small_cap() -> BuildOptions {
    BuildOptions {
        max_file_size: 1024, // force data/big.log onto the scan-always path
        ..Default::default()
    }
}

fn build_fixture_index(fx: &Fixture) -> IndexReader {
    build::build(&fx.root, &fx.index_path, None, &opts_small_cap()).unwrap();
    IndexReader::open(&fx.index_path).unwrap()
}

fn result_set(results: &[grix::search::FileResult]) -> BTreeSet<(String, u64)> {
    let mut set = BTreeSet::new();
    for fr in results {
        for line in &fr.lines {
            set.insert((fr.rel_path.clone(), line.line_number));
        }
    }
    set
}

#[test]
fn index_search_equals_full_scan() {
    let fx = fixture();
    let reader = build_fixture_index(&fx);
    let patterns: &[(&str, bool)] = &[
        ("foo", false),
        ("foo", true),
        ("fo", false), // too short: plan must degrade to ALL, not break
        ("f.o", false),
        ("foo|bar", false),
        ("FOO", false),
        ("FOO", true),
        ("^foo", false),
        ("foo$", false),
        (r"\bfoo\b", false),
        ("fo+o?", false),
        ("[fg]oo", false),
        ("foo.*bar", false),
        ("needle", false),
        (r"println!\(", false),
        ("日本語", false),
        ("filler line 1[0-9]", false),
        ("zzz_no_match_zzz", false),
    ];
    for &(pattern, ci) in patterns {
        let opts = SearchOptions {
            case_insensitive: ci,
            ..Default::default()
        };
        let matcher = search::compile(pattern, &opts).unwrap();
        let (with_index, _) = search::search_index(&reader, &fx.root, &matcher, &opts).unwrap();
        let (walked, _) = search::search_walk(&fx.root, &matcher, &opts).unwrap();
        assert_eq!(
            result_set(&with_index),
            result_set(&walked),
            "index vs walk diverged for pattern {pattern:?} (ci={ci})"
        );
    }
}

#[test]
fn finds_expected_lines() {
    let fx = fixture();
    let reader = build_fixture_index(&fx);
    let opts = SearchOptions::default();
    let matcher = search::compile("foo", &opts).unwrap();
    let (results, stats) = search::search_index(&reader, &fx.root, &matcher, &opts).unwrap();
    let set = result_set(&results);

    assert!(set.contains(&("src/lib.rs".into(), 1)));
    assert!(set.contains(&("src/lib.rs".into(), 2)));
    assert!(set.contains(&("docs/guide.md".into(), 1)));
    assert!(set.contains(&("data/crlf.txt".into(), 1)));
    assert!(set.contains(&("data/crlf.txt".into(), 3)));
    assert!(set.contains(&("data/unicode.txt".into(), 1)));
    assert!(set.contains(&("deep/a/b/c/needle.txt".into(), 1)));
    // Scan-always file is still searched.
    assert!(set.contains(&("data/big.log".into(), 1)));
    // Binary and gitignored files are not.
    assert!(!set.iter().any(|(p, _)| p == "data/binary.bin"));
    assert!(!set.iter().any(|(p, _)| p == "ignored.txt"));
    // The index actually narrowed the scan.
    assert!(stats.candidates < stats.files_in_index);
}

#[test]
fn incremental_update_reflects_edits() {
    let fx = fixture();
    let reader = build_fixture_index(&fx);
    let opts = SearchOptions::default();

    // New file + modified file + deleted file.
    write(
        &fx.root,
        "src/new_module.rs",
        b"const SENTINEL_XYZQ: u32 = 1;\n",
    );
    // Force a different mtime even on coarse filesystems.
    std::thread::sleep(std::time::Duration::from_millis(20));
    write(
        &fx.root,
        "src/lib.rs",
        b"pub fn foo() -> u32 { 42 }\n// SENTINEL_XYZQ here too\n",
    );
    std::fs::remove_file(fx.root.join("docs/guide.md")).unwrap();

    let old = reader;
    let stats = build::build(&fx.root, &fx.index_path, Some(&old), &opts_small_cap()).unwrap();
    assert!(
        stats.files_reused > 0,
        "expected unchanged files to be reused, got {stats:?}"
    );
    let reader = IndexReader::open(&fx.index_path).unwrap();

    let matcher = search::compile("SENTINEL_XYZQ", &opts).unwrap();
    let (results, _) = search::search_index(&reader, &fx.root, &matcher, &opts).unwrap();
    let set = result_set(&results);
    assert!(set.contains(&("src/new_module.rs".into(), 1)));
    assert!(set.contains(&("src/lib.rs".into(), 2)));

    // Deleted file is gone from results.
    let matcher = search::compile("Searching with", &opts).unwrap();
    let (results, _) = search::search_index(&reader, &fx.root, &matcher, &opts).unwrap();
    assert!(results.is_empty());

    // And the equivalence property still holds after the incremental build.
    for pattern in ["foo", "SENTINEL_XYZQ", "fn "] {
        let matcher = search::compile(pattern, &opts).unwrap();
        let (a, _) = search::search_index(&reader, &fx.root, &matcher, &opts).unwrap();
        let (b, _) = search::search_walk(&fx.root, &matcher, &opts).unwrap();
        assert_eq!(result_set(&a), result_set(&b), "diverged for {pattern:?}");
    }
}

#[test]
fn path_scope_dir_filters() {
    let fx = fixture();
    let reader = build_fixture_index(&fx);
    let opts = SearchOptions {
        path_scopes: vec!["src".into()],
        ..Default::default()
    };
    let matcher = search::compile("foo", &opts).unwrap();
    let (results, _) = search::search_index(&reader, &fx.root, &matcher, &opts).unwrap();
    assert!(!results.is_empty());
    assert!(results.iter().all(|r| r.rel_path.starts_with("src/")));
}

#[test]
fn path_scope_single_file() {
    let fx = fixture();
    let reader = build_fixture_index(&fx);
    let opts = SearchOptions {
        path_scopes: vec!["src/lib.rs".into()],
        ..Default::default()
    };
    let matcher = search::compile("foo", &opts).unwrap();
    let (results, _) = search::search_index(&reader, &fx.root, &matcher, &opts).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].rel_path, "src/lib.rs");
}

#[test]
fn path_scope_multiple() {
    let fx = fixture();
    let reader = build_fixture_index(&fx);
    let opts = SearchOptions {
        path_scopes: vec!["src".into(), "docs/guide.md".into()],
        ..Default::default()
    };
    let matcher = search::compile("foo", &opts).unwrap();
    let (results, _) = search::search_index(&reader, &fx.root, &matcher, &opts).unwrap();
    assert!(results.iter().any(|r| r.rel_path.starts_with("src/")));
    assert!(results.iter().any(|r| r.rel_path == "docs/guide.md"));
    assert!(results
        .iter()
        .all(|r| r.rel_path.starts_with("src/") || r.rel_path == "docs/guide.md"));
}

#[test]
fn binary_smoke_exit_codes() {
    let exe = env!("CARGO_BIN_EXE_grix");
    let fx = fixture();
    let data_dir = fx.root.join(".grix-data");

    let run = |args: &[&str]| {
        std::process::Command::new(exe)
            .args(args)
            .env("GRIX_DATA_DIR", &data_dir)
            .current_dir(&fx.root)
            .output()
            .unwrap()
    };

    // First search auto-indexes and finds matches -> exit 0.
    let out = run(&["foo", ".", "--color", "never"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("hello grix") || stdout.contains("foo"),
        "{stdout}"
    );

    // No match -> exit 1.
    let out = run(&["qqqqqq_nothing", ".", "--color", "never"]);
    assert_eq!(out.status.code(), Some(1));

    // Bad pattern -> exit 2.
    let out = run(&["f(oo", "."]);
    assert_eq!(out.status.code(), Some(2));

    // status reports the index.
    let out = run(&["status"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("files:"));

    // Without context, plain output has no "--" dividers between matches,
    // even when matches are on non-adjacent lines (regression guard).
    let out = run(&["foo", "--color", "never", "--no-heading"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("--\n"), "unexpected divider:\n{stdout}");

    // -g scopes to a glob (only .md files here contain "foo" in docs/).
    let out = run(&["foo", "-g", "*.md", "--color", "never", "--no-heading"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.lines().all(|l| l.starts_with("docs/")), "{stdout}");

    // -C adds context and the "--" divider returns.
    let out = run(&["needle", "-C1", "--color", "never", "--no-heading"]);
    assert_eq!(out.status.code(), Some(0));

    // A file created AFTER the index exists is still found: each search
    // refreshes the index by default (regression guard for the silent
    // stale-index miss).
    std::fs::write(
        fx.root.join("added_after.rs"),
        b"const SURPRISE_TOK: u8 = 0;\n",
    )
    .unwrap();
    let out = run(&["SURPRISE_TOK", "--color", "never", "--no-heading"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "auto-refresh should find new file"
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("added_after.rs"));

    // With --no-auto-index the index is used as-is, so a brand-new file is
    // missed -- but grix says why instead of a silent 0 result.
    std::fs::write(
        fx.root.join("added_later.rs"),
        b"const LATER_TOK: u8 = 0;\n",
    )
    .unwrap();
    let out = run(&[
        "LATER_TOK",
        "--no-auto-index",
        "--color",
        "never",
        "--no-heading",
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("stale"));
}

/// A fresh watch marker makes a normal search skip its refresh (it trusts the
/// daemon to keep the index current) — and not warn about staleness. Removing
/// the marker restores self-refresh. This pins the search-side integration
/// without depending on filesystem-event timing.
#[test]
fn watch_marker_controls_refresh() {
    use std::time::{SystemTime, UNIX_EPOCH};

    let exe = env!("CARGO_BIN_EXE_grix");
    let fx = fixture();
    let data_dir = fx.root.join(".grix-data");

    let run = |args: &[&str]| {
        std::process::Command::new(exe)
            .args(args)
            .env("GRIX_DATA_DIR", &data_dir)
            .current_dir(&fx.root)
            .output()
            .unwrap()
    };

    // Build the index once.
    let out = run(&["index"]);
    assert_eq!(out.status.code(), Some(0));

    let gix = std::fs::read_dir(&data_dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|x| x == "gix"))
        .expect("index file");
    let marker = gix.with_extension("watch");

    // Add a file the index does not know about yet.
    std::fs::write(fx.root.join("watched.rs"), b"const WATCHED_TOK: u8 = 0;\n").unwrap();

    // Fresh marker present -> search trusts it, skips refresh, misses the new
    // file, and does NOT print a stale hint.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    std::fs::write(&marker, format!("4242\n{now}\n")).unwrap();
    let out = run(&["WATCHED_TOK", "--color", "never", "--no-heading"]);
    assert_eq!(out.status.code(), Some(1), "marker should suppress refresh");
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("stale"),
        "no stale hint while a watcher is live"
    );

    // Remove the marker -> normal refresh kicks in and finds the file.
    std::fs::remove_file(&marker).unwrap();
    let out = run(&["WATCHED_TOK", "--color", "never", "--no-heading"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("watched.rs"));

    // status reflects watcher state (off after marker removed).
    let out = run(&["status"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("watch:    off"));
}
