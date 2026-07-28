//! End-to-end tests over real temp directories.
//!
//! The core property: searching WITH the index returns exactly the same
//! (path, line) set as a full walk-scan, for every pattern shape the
//! planner handles. The index must only ever narrow work, never change
//! results.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use grix::index::build::{self, BuildOptions};
use grix::index::format::{overlay_path, IndexReader};
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
    // Hidden files: indexed, but only searched with --hidden.
    write(&root, ".dotfile.txt", b"foo in dotfile\n");
    write(&root, ".hiddendir/nested.txt", b"foo nested hidden\n");
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

fn build_fixture_index(fx: &Fixture) {
    build::build(&fx.root, &fx.index_path, &opts_small_cap()).unwrap();
}

/// Search through base + (if present) overlay, exactly like the CLI does.
fn search_all(
    fx: &Fixture,
    pattern: &str,
    opts: &SearchOptions,
) -> (Vec<grix::search::FileResult>, grix::search::SearchStats) {
    let matcher = search::compile(pattern, opts).unwrap();
    let base = IndexReader::open(&fx.index_path).unwrap();
    let over = IndexReader::open(&overlay_path(&fx.index_path))
        .ok()
        .filter(|o| o.index_ids().parent_id == base.index_ids().build_id);
    let view = search::View::new(&base, over.as_ref());
    search::search_index(&view, &fx.root, &matcher, opts).unwrap()
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
    build_fixture_index(&fx);
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
        let (with_index, _) = search_all(&fx, pattern, &opts);
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
    build_fixture_index(&fx);
    let opts = SearchOptions::default();
    let (results, stats) = search_all(&fx, "foo", &opts);
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
    build_fixture_index(&fx);
    let opts = SearchOptions::default();
    let base_mtime = std::fs::metadata(&fx.index_path)
        .unwrap()
        .modified()
        .unwrap();

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

    let stats = build::build(&fx.root, &fx.index_path, &opts_small_cap()).unwrap();
    assert!(
        stats.files_reused > 0,
        "expected unchanged files to be reused, got {stats:?}"
    );
    // The refresh went to the overlay; the base was not rewritten.
    assert!(overlay_path(&fx.index_path).exists());
    assert_eq!(
        std::fs::metadata(&fx.index_path)
            .unwrap()
            .modified()
            .unwrap(),
        base_mtime,
        "small refresh must not rewrite the base index"
    );

    let (results, _) = search_all(&fx, "SENTINEL_XYZQ", &opts);
    let set = result_set(&results);
    assert!(set.contains(&("src/new_module.rs".into(), 1)));
    assert!(set.contains(&("src/lib.rs".into(), 2)));

    // Deleted file is gone from results (tombstoned in the overlay).
    let (results, _) = search_all(&fx, "Searching with", &opts);
    assert!(results.is_empty());

    // And the equivalence property still holds through the overlay.
    for pattern in ["foo", "SENTINEL_XYZQ", "fn "] {
        let matcher = search::compile(pattern, &opts).unwrap();
        let (a, _) = search_all(&fx, pattern, &opts);
        let (b, _) = search::search_walk(&fx.root, &matcher, &opts).unwrap();
        assert_eq!(result_set(&a), result_set(&b), "diverged for {pattern:?}");
    }
}

#[test]
fn multiline_search_equals_full_scan() {
    let fx = fixture();
    build_fixture_index(&fx);
    let opts = SearchOptions {
        multiline: true,
        ..Default::default()
    };
    // Patterns spanning line boundaries: the byte-trigram index knows about
    // "o\nb"-style trigrams, so narrowing must still be exact — including
    // across CRLF.
    for pattern in [
        r"42 \}\npub fn",
        r"alpha foo\r\nbeta",
        r"foo\nbar",
        r"grix\W+\}",
    ] {
        let matcher = search::compile(pattern, &opts).unwrap();
        let (a, _) = search_all(&fx, pattern, &opts);
        let (b, _) = search::search_walk(&fx.root, &matcher, &opts).unwrap();
        assert_eq!(
            result_set(&a),
            result_set(&b),
            "multiline diverged for {pattern:?}"
        );
    }
}

#[test]
fn flag_modes_equal_full_scan() {
    let fx = fixture();
    build_fixture_index(&fx);
    // Each flag combination must agree between indexed search and a full
    // walk — especially -v, where the index cannot narrow anything.
    let variants: &[SearchOptions] = &[
        SearchOptions {
            word: true,
            ..Default::default()
        },
        SearchOptions {
            invert: true,
            ..Default::default()
        },
        SearchOptions {
            only_matching: true,
            ..Default::default()
        },
        SearchOptions {
            word: true,
            invert: true,
            ..Default::default()
        },
        SearchOptions {
            invert: true,
            multiline: true,
            ..Default::default()
        },
        SearchOptions {
            smart_case: true,
            ..Default::default()
        },
    ];
    for (i, opts) in variants.iter().enumerate() {
        for pattern in ["foo", "FOO"] {
            let matcher = search::compile(pattern, opts).unwrap();
            let (a, _) = search_all(&fx, pattern, opts);
            let (b, _) = search::search_walk(&fx.root, &matcher, opts).unwrap();
            assert_eq!(
                result_set(&a),
                result_set(&b),
                "variant {i} diverged for {pattern:?}"
            );
        }
    }
}

#[test]
fn hidden_and_no_ignore() {
    let fx = fixture();
    build_fixture_index(&fx);

    // Default: hidden files stay invisible (but they ARE in the index).
    let opts = SearchOptions::default();
    let (r, _) = search_all(&fx, "foo", &opts);
    assert!(!result_set(&r).iter().any(|(p, _)| p.starts_with('.')));

    // --hidden serves them straight from the index, no rebuild.
    let opts_h = SearchOptions {
        hidden: true,
        ..Default::default()
    };
    let (r, _) = search_all(&fx, "foo", &opts_h);
    let set = result_set(&r);
    assert!(set.contains(&(".dotfile.txt".into(), 1)));
    assert!(set.contains(&(".hiddendir/nested.txt".into(), 1)));

    // Equivalence with a walk under --hidden.
    let matcher = search::compile("foo", &opts_h).unwrap();
    let (w, _) = search::search_walk(&fx.root, &matcher, &opts_h).unwrap();
    assert_eq!(set, result_set(&w));

    // --no-ignore (always a walk) reaches the gitignored file.
    let opts_u = SearchOptions {
        no_ignore: true,
        ..Default::default()
    };
    let matcher = search::compile("foo", &opts_u).unwrap();
    let (w, _) = search::search_walk(&fx.root, &matcher, &opts_u).unwrap();
    assert!(result_set(&w).iter().any(|(p, _)| p == "ignored.txt"));
    // .git contents stay pruned even with everything else off.
    assert!(!result_set(&w).iter().any(|(p, _)| p.starts_with(".git/")));
}

#[test]
fn path_scope_dir_filters() {
    let fx = fixture();
    build_fixture_index(&fx);
    let opts = SearchOptions {
        path_scopes: vec!["src".into()],
        ..Default::default()
    };
    let (results, _) = search_all(&fx, "foo", &opts);
    assert!(!results.is_empty());
    assert!(results.iter().all(|r| r.rel_path.starts_with("src/")));
}

#[test]
fn path_scope_single_file() {
    let fx = fixture();
    build_fixture_index(&fx);
    let opts = SearchOptions {
        path_scopes: vec!["src/lib.rs".into()],
        ..Default::default()
    };
    let (results, _) = search_all(&fx, "foo", &opts);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].rel_path, "src/lib.rs");
}

#[test]
fn path_scope_multiple() {
    let fx = fixture();
    build_fixture_index(&fx);
    let opts = SearchOptions {
        path_scopes: vec!["src".into(), "docs/guide.md".into()],
        ..Default::default()
    };
    let (results, _) = search_all(&fx, "foo", &opts);
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

    // First search answers immediately from a full scan (the index builds
    // in a detached child) -> exit 0 with the same results.
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

    // Wait for the background builder to land the index and release the
    // marker, so the remaining steps are deterministic. `watch: off` is
    // printed only once the index opens and no heartbeat is fresh.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let st = run(&["status"]);
        let text = String::from_utf8_lossy(&st.stdout).into_owned();
        if text.contains("files:") && text.contains("watch:    off") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "background index build did not finish: {text}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

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

/// Drive the MCP server over stdio with a real JSON-RPC session and check the
/// handshake, tool list, and a tool call. stdout must be JSON-RPC only.
#[test]
fn mcp_server_speaks_jsonrpc() {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let exe = env!("CARGO_BIN_EXE_grix");
    let fx = fixture();
    let data_dir = fx.root.join(".grix-data");

    let mut child = Command::new(exe)
        .arg("mcp")
        .env("GRIX_DATA_DIR", &data_dir)
        .current_dir(&fx.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let session = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"code_search","arguments":{"pattern":"foo","type":"md"}}}"#,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"code_search","arguments":{"pattern":"f(oo"}}}"#,
    ];
    {
        let stdin = child.stdin.as_mut().unwrap();
        for line in session {
            writeln!(stdin, "{line}").unwrap();
        }
        // dropping stdin (end of this block) sends EOF so the server exits
    }
    drop(child.stdin.take());

    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Every non-empty stdout line must be a JSON-RPC object (no log leakage).
    let mut by_id = std::collections::HashMap::new();
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|_| panic!("non-JSON on stdout: {line}"));
        assert_eq!(v["jsonrpc"], "2.0");
        if let Some(id) = v.get("id").and_then(|x| x.as_u64()) {
            by_id.insert(id, v);
        }
    }

    // initialize
    assert_eq!(by_id[&1]["result"]["serverInfo"]["name"], "grix");
    // tools/list has both tools
    let names: Vec<&str> = by_id[&2]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"code_search") && names.contains(&"list_matching_files"));
    // code_search found "foo" in the markdown doc, not an error
    let text = by_id[&3]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("docs/guide.md"), "search result: {text}");
    assert_eq!(by_id[&3]["result"]["isError"], false);
    // a bad pattern is reported as a tool error, not a protocol error
    assert_eq!(by_id[&4]["result"]["isError"], true);
}
