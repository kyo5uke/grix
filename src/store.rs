//! Where indexes live: one file per indexed root, under the user cache dir,
//! named by a hash of the canonical root path. Repos stay untouched.

use std::io;
use std::path::{Path, PathBuf};

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
    let s = c.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s).to_string();
    Ok(PathBuf::from(s))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_known_value() {
        // FNV-1a 64 of "a" is a published constant.
        assert_eq!(fnv1a(b"a"), 0xaf63dc4c8601ec8c);
    }

    #[test]
    fn key_separator_insensitive() {
        let a = root_key(Path::new(r"C:\repo\x"));
        let b = root_key(Path::new("C:/repo/x"));
        assert_eq!(a, b);
    }
}
