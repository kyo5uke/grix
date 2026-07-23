//! Where indexes live: one file per indexed root, under the user cache dir,
//! named by a hash of the canonical root path. Repos stay untouched.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A live watcher is considered fresh if its heartbeat is newer than this.
/// `grix watch` refreshes the heartbeat well within this window.
const WATCH_FRESH_MS: u64 = 30_000;

pub fn data_dir() -> io::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("GRIX_DATA_DIR") {
        return Ok(PathBuf::from(dir));
    }
    #[cfg(windows)]
    {
        if let Some(base) = std::env::var_os("LOCALAPPDATA") {
            return Ok(PathBuf::from(base).join("grix"));
        }
    }
    if let Some(base) = std::env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(base).join("grix"));
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        return Ok(PathBuf::from(home).join(".cache").join("grix"));
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "cannot determine a cache directory (LOCALAPPDATA/XDG_CACHE_HOME/HOME unset)",
    ))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Canonicalize for identity purposes: resolve symlinks/relative parts,
/// strip Windows' verbatim prefix, fold case on Windows.
pub fn canonical_root(path: &Path) -> io::Result<PathBuf> {
    let c = std::fs::canonicalize(path)?;
    Ok(PathBuf::from(strip_verbatim(&c.to_string_lossy())))
}

/// Remove Windows' verbatim prefix while keeping the path usable:
/// `\\?\C:\x` -> `C:\x`, and `\\?\UNC\server\share\x` -> `\\server\share\x`
/// (naively stripping the latter would leave a relative-looking `UNC\...`).
fn strip_verbatim(s: &str) -> String {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s.to_string()
    }
}

fn root_key(root: &Path) -> u64 {
    let mut s = root.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        s = s.to_lowercase();
    }
    fnv1a(s.as_bytes())
}

/// Index file path for a canonical root.
pub fn index_path(root: &Path) -> io::Result<PathBuf> {
    Ok(data_dir()?.join(format!("{:016x}.gix", root_key(root))))
}

/// Walk up from `start` looking for the nearest ancestor that has an index.
/// Returns (index file, indexed root).
pub fn find_index_upward(start: &Path) -> Option<(PathBuf, PathBuf)> {
    let canon = canonical_root(start).ok()?;
    let dir = data_dir().ok()?;
    let mut cur: Option<&Path> = Some(canon.as_path());
    while let Some(p) = cur {
        let idx = dir.join(format!("{:016x}.gix", root_key(p)));
        if idx.is_file() {
            return Some((idx, p.to_path_buf()));
        }
        cur = p.parent();
    }
    None
}

// ---- watch marker ----
//
// `grix watch` keeps the index fresh in the background; a sidecar file next to
// the index records a heartbeat so `grix <pattern>` can skip its own refresh
// while a watcher is alive. The heartbeat (not a lock) means a crashed watcher
// simply goes stale and searches resume self-refreshing.

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn watch_marker_path(index_path: &Path) -> PathBuf {
    index_path.with_extension("watch")
}

/// Write/refresh the watcher heartbeat (`pid\nmillis`).
pub fn write_watch_heartbeat(index_path: &Path) -> io::Result<()> {
    std::fs::write(
        watch_marker_path(index_path),
        format!("{}\n{}\n", std::process::id(), now_millis()),
    )
}

pub fn remove_watch_marker(index_path: &Path) {
    let _ = std::fs::remove_file(watch_marker_path(index_path));
}

/// True if a watcher refreshed the marker within the freshness window.
pub fn watcher_is_live(index_path: &Path) -> bool {
    let Ok(s) = std::fs::read_to_string(watch_marker_path(index_path)) else {
        return false;
    };
    watcher_is_live_at(&s, now_millis())
}

/// Like `watcher_is_live`, but a fresh marker written by *this* process does
/// not count. Used when deciding whether to spawn a background builder after
/// having claimed the marker ourselves earlier in the run.
pub fn watcher_is_live_other(index_path: &Path) -> bool {
    let Ok(s) = std::fs::read_to_string(watch_marker_path(index_path)) else {
        return false;
    };
    let other_pid = s
        .lines()
        .next()
        .and_then(|l| l.trim().parse::<u32>().ok())
        .map_or(true, |pid| pid != std::process::id());
    other_pid && watcher_is_live_at(&s, now_millis())
}

/// Testable core: is the marker's heartbeat fresh relative to `now`?
fn watcher_is_live_at(marker: &str, now: u64) -> bool {
    let Some(hb) = marker
        .lines()
        .nth(1)
        .and_then(|l| l.trim().parse::<u64>().ok())
    else {
        return false;
    };
    now >= hb && now - hb < WATCH_FRESH_MS
}

/// A background thread keeping the watch heartbeat fresh. While it runs,
/// searches treat the index as watched and skip their own refresh — used by
/// `grix watch` and by detached background index builds. `stop_and_clear`
/// joins the thread *before* removing the marker, so no late beat can
/// resurrect it.
pub struct Heartbeat {
    stop: std::sync::mpsc::Sender<()>,
    handle: std::thread::JoinHandle<()>,
    index_path: PathBuf,
}

pub fn start_heartbeat(index_path: &Path, every: std::time::Duration) -> Heartbeat {
    let (stop, rx) = std::sync::mpsc::channel::<()>();
    let p = index_path.to_path_buf();
    let handle = std::thread::spawn(move || loop {
        let _ = write_watch_heartbeat(&p);
        match rx.recv_timeout(every) {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            _ => break, // stop: sender dropped
        }
    });
    Heartbeat {
        stop,
        handle,
        index_path: index_path.to_path_buf(),
    }
}

impl Heartbeat {
    pub fn stop_and_clear(self) {
        drop(self.stop);
        let _ = self.handle.join();
        remove_watch_marker(&self.index_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_known_value() {
        // FNV-1a 64 of "a" is a published constant.
        assert_eq!(fnv1a(b"a"), 0xaf63dc4c8601ec8c);
    }

    #[test]
    fn verbatim_prefix_stripping() {
        assert_eq!(strip_verbatim(r"\\?\C:\repo\x"), r"C:\repo\x");
        assert_eq!(
            strip_verbatim(r"\\?\UNC\server\share\repo"),
            r"\\server\share\repo"
        );
        assert_eq!(strip_verbatim(r"C:\repo"), r"C:\repo");
        assert_eq!(strip_verbatim("/home/x"), "/home/x");
    }

    #[test]
    fn key_separator_insensitive() {
        let a = root_key(Path::new(r"C:\repo\x"));
        let b = root_key(Path::new("C:/repo/x"));
        assert_eq!(a, b);
    }

    #[test]
    fn watch_liveness() {
        // fresh heartbeat -> live
        let m = format!("1234\n{}\n", 100_000);
        assert!(watcher_is_live_at(&m, 100_000 + 5_000)); // 5s old
        assert!(!watcher_is_live_at(&m, 100_000 + 40_000)); // 40s old -> stale
        assert!(!watcher_is_live_at(&m, 100_000 - 1)); // clock went backwards
                                                       // malformed markers -> not live
        assert!(!watcher_is_live_at("", 100_000));
        assert!(!watcher_is_live_at("1234\n", 100_000));
        assert!(!watcher_is_live_at("1234\nnotanumber\n", 100_000));
    }

    #[test]
    fn marker_path_sidecar() {
        let p = watch_marker_path(Path::new("/cache/abc.gix"));
        assert_eq!(p, Path::new("/cache/abc.watch"));
    }
}
