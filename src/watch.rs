//! `grix watch`: keep the index fresh in the background.
//!
//! Instead of walking the tree on every search, a watcher subscribes to
//! filesystem events and reindexes incrementally as files change. Searches
//! then skip their own refresh (see the heartbeat marker in `store`) and stay
//! instant *and* current.
//!
//! The reindex itself reuses the normal incremental build: a debounced
//! `index::build::build` after a burst of changes. The build reuses unchanged
//! files (size + mtime), so it only re-reads what actually changed. Event
//! filtering (gitignore + `.git`) keeps build churn — e.g. `cargo build`
//! writing into `target/` — from triggering pointless reindexes.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::{Duration, Instant};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::index::build::{self, BuildOptions};
use crate::index::format::IndexReader;
use crate::store;

/// Quiet period after the last change before reindexing.
const DEBOUNCE: Duration = Duration::from_millis(400);
/// How often to refresh the heartbeat while idle.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(5);

fn human_count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn to_io<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}

/// Build the index for an event-path filter from the root's `.gitignore`.
fn build_ignore(root: &Path) -> Gitignore {
    let mut b = GitignoreBuilder::new(root);
    let _ = b.add(root.join(".gitignore"));
    b.build().unwrap_or_else(|_| Gitignore::empty())
}

/// True if an event path should be ignored (gitignored or inside `.git`).
/// Filtering is best-effort: a missed ignore only costs a wasted reindex,
/// never correctness (the build re-applies gitignore rules anyway).
fn is_ignored(ig: &Gitignore, path: &Path) -> bool {
    if path.components().any(|c| c.as_os_str() == ".git") {
        return true;
    }
    let is_dir = path.is_dir();
    ig.matched_path_or_any_parents(path, is_dir).is_ignore()
}

fn relevant(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

/// Run the incremental build, retrying a few times: on Windows the atomic
/// rename can briefly fail while a concurrent search holds the index mmap.
fn reindex_with_retry(
    root: &Path,
    index_path: &Path,
    opts: &BuildOptions,
) -> io::Result<build::BuildStats> {
    let mut last = None;
    for attempt in 0..4 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(200));
        }
        let old = IndexReader::open(index_path).ok();
        match build::build(root, index_path, old.as_ref(), opts) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("reindex failed")))
}

/// Watch `root`, keeping `index_path` current until the process is stopped.
pub fn run(root: &Path, index_path: &Path, opts: &BuildOptions) -> io::Result<()> {
    if let Some(parent) = index_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Initial (incremental) build so the index is correct before watching.
    let t = Instant::now();
    let stats = reindex_with_retry(root, index_path, opts)?;
    store::write_watch_heartbeat(index_path)?;
    eprintln!(
        "grix: watching {} ({} files indexed) — built in {:.2}s. Press Ctrl-C to stop.",
        root.display(),
        human_count(stats.files_indexed),
        t.elapsed().as_secs_f64(),
    );

    let ignore = build_ignore(root);

    let (tx, rx) = channel();
    let mut watcher = RecommendedWatcher::new(
        move |res| {
            let _ = tx.send(res);
        },
        Config::default(),
    )
    .map_err(to_io)?;
    watcher
        .watch(root, RecursiveMode::Recursive)
        .map_err(to_io)?;

    let mut changed: BTreeSet<PathBuf> = BTreeSet::new();
    let mut last_event = Instant::now();
    let mut last_heartbeat = Instant::now();

    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(event)) => {
                if relevant(&event.kind) {
                    for p in event.paths {
                        if !is_ignored(&ignore, &p) {
                            changed.insert(p);
                        }
                    }
                    if !changed.is_empty() {
                        last_event = Instant::now();
                    }
                }
            }
            Ok(Err(_)) => {} // watch backend error; keep going
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if last_heartbeat.elapsed() >= HEARTBEAT_EVERY {
            let _ = store::write_watch_heartbeat(index_path);
            last_heartbeat = Instant::now();
        }

        if !changed.is_empty() && last_event.elapsed() >= DEBOUNCE {
            let n = changed.len();
            changed.clear();
            let t = Instant::now();
            match reindex_with_retry(root, index_path, opts) {
                Ok(s) => eprintln!(
                    "grix: reindexed ({} changed → {} files) in {:.0}ms",
                    human_count(n),
                    human_count(s.files_indexed),
                    t.elapsed().as_secs_f64() * 1e3,
                ),
                Err(e) => eprintln!("grix: reindex failed: {e}"),
            }
            let _ = store::write_watch_heartbeat(index_path);
            last_heartbeat = Instant::now();
        }
    }

    store::remove_watch_marker(index_path);
    Ok(())
}
