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
/// Upper bound on debounce deferral: even if changes keep arriving faster
/// than DEBOUNCE (e.g. a log writer), reindex at least this often so a
/// continuous writer cannot starve the index forever.
const MAX_PENDING: Duration = Duration::from_secs(5);
/// How often to refresh the heartbeat.
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

    // Heartbeat on its own thread: a build can take tens of seconds on a
    // large tree, and a blocked main loop must not let the marker go stale
    // (concurrent searches would start competing builds and rename-race the
    // index). The channel doubles as the stop signal — dropping the sender
    // wakes the thread immediately — and joining *before* removing the
    // marker guarantees no late beat resurrects it after cleanup.
    let (hb_stop, hb_rx) = channel::<()>();
    let hb = {
        let idx = index_path.to_path_buf();
        std::thread::spawn(move || loop {
            let _ = store::write_watch_heartbeat(&idx);
            match hb_rx.recv_timeout(HEARTBEAT_EVERY) {
                Err(RecvTimeoutError::Timeout) => continue,
                _ => break, // stop: sender dropped
            }
        })
    };
    let result = watch_loop(root, index_path, opts);
    drop(hb_stop);
    let _ = hb.join();
    store::remove_watch_marker(index_path);
    result
}

fn watch_loop(root: &Path, index_path: &Path, opts: &BuildOptions) -> io::Result<()> {
    // Initial (incremental) build so the index is correct before watching.
    let t = Instant::now();
    let stats = reindex_with_retry(root, index_path, opts)?;
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
    // When the oldest still-pending change arrived, for the MAX_PENDING cap.
    let mut first_pending: Option<Instant> = None;

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
                        first_pending.get_or_insert(last_event);
                    }
                }
            }
            Ok(Err(e)) => {
                // A backend error (e.g. ReadDirectoryChangesW buffer
                // overflow on Windows) can mean dropped events. Schedule a
                // reindex so nothing stays silently missing: the incremental
                // build rescans the tree, so it repairs any missed change.
                eprintln!("grix: watch backend error ({e}); scheduling a reindex");
                changed.insert(root.to_path_buf());
                last_event = Instant::now();
                first_pending.get_or_insert(last_event);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        let due = !changed.is_empty()
            && (last_event.elapsed() >= DEBOUNCE
                || first_pending.is_some_and(|t| t.elapsed() >= MAX_PENDING));
        if due {
            let n = changed.len();
            changed.clear();
            first_pending = None;
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
        }
    }

    Ok(())
}
