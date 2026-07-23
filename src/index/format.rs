//! On-disk index format (version 2).
//!
//! Single file, little-endian, designed to be mmap'd and used directly:
//!
//! ```text
//! [magic "GRIXIDX2"][header][root path][paths blob][file table]
//! [scan-always ids][trigram table][postings]
//! ```
//!
//! - file table: fixed 28-byte entries (path off/len, size, mtime, flags)
//! - scan-always ids: ascending u32 file ids with FLAG_SCAN_ALWAYS, so a
//!   search adds them without walking the whole file table (v1 walked all
//!   records on every query)
//! - trigram table: fixed 16-byte entries (key, postings len, postings off),
//!   sorted by key -> binary search
//! - postings: per trigram, delta-encoded LEB128 file ids, ascending
//!
//! Version 1 indexes are rejected with `WrongVersion`; the auto-refresh
//! before a search then rebuilds them transparently.
//!
//! Every read is bounds-checked; a corrupt index yields an error, never UB.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use memmap2::Mmap;

use crate::varint;

pub const MAGIC: &[u8; 8] = b"GRIXIDX2";
pub const VERSION: u32 = 2;
const HEADER_LEN: usize = 128;

/// Leftover temps from a crashed build are collected once they are this old.
/// Far longer than any live build, so an in-flight sibling is never touched.
const TEMP_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Tag making temp names unique per build attempt. Concurrent builds of the
/// same index (parallel searches, watcher + search, another process) must
/// never share a temp path — interleaved writes through separate handles
/// would install a corrupt index. With unique names, whichever build renames
/// last wins with a complete file.
pub(crate) fn temp_tag() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// `<index file>.<kind>.<tag>.tmp` next to the index — same directory, so
/// the final rename stays on one volume and atomic.
pub(crate) fn temp_sibling(index_path: &Path, kind: &str, tag: &str) -> PathBuf {
    let mut name = index_path.as_os_str().to_os_string();
    name.push(format!(".{kind}.{tag}.tmp"));
    PathBuf::from(name)
}

/// Open for reading with a sequential-access hint. On Windows this sets
/// FILE_FLAG_SEQUENTIAL_SCAN, which noticeably helps the scan pattern of
/// "open and read many files front to back"; elsewhere it is a plain open.
pub(crate) fn open_sequential(path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
            .open(path)
    }
    #[cfg(not(windows))]
    {
        File::open(path)
    }
}

/// Delete stale build temps (`<index file>.*.tmp`) left behind by crashed
/// builds; uniquely-named temps are never overwritten, so without a sweep
/// they would accumulate.
pub(crate) fn sweep_stale_temps(index_path: &Path) {
    sweep_stale_temps_older_than(index_path, TEMP_MAX_AGE);
}

fn sweep_stale_temps_older_than(index_path: &Path, max_age: Duration) {
    let Some(dir) = index_path.parent() else {
        return;
    };
    let Some(file_name) = index_path.file_name() else {
        return;
    };
    let prefix = format!("{}.", file_name.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&prefix) || !name.ends_with(".tmp") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok()) // future mtime -> Err -> keep
            .is_some_and(|age| age >= max_age);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// File was too large to index; search must always scan it.
pub const FLAG_SCAN_ALWAYS: u32 = 1;
/// File looked binary (NUL byte); excluded from search entirely.
pub const FLAG_BINARY: u32 = 2;

#[derive(Debug, Clone)]
pub struct FileRecord {
    /// Path relative to the index root, '/'-separated.
    pub rel_path: String,
    pub size: u64,
    /// Nanoseconds since the unix epoch (0 if unknown).
    pub mtime: u64,
    pub flags: u32,
}

#[derive(Debug)]
pub enum IndexError {
    Io(io::Error),
    Corrupt(&'static str),
    WrongVersion(u32),
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexError::Io(e) => write!(f, "index io error: {e}"),
            IndexError::Corrupt(what) => write!(f, "corrupt index ({what})"),
            IndexError::WrongVersion(v) => write!(f, "unsupported index version {v}"),
        }
    }
}

impl std::error::Error for IndexError {}

impl From<io::Error> for IndexError {
    fn from(e: io::Error) -> Self {
        IndexError::Io(e)
    }
}

/// Streaming source of postings for `write_index`: yields each trigram key
/// with its sorted, deduplicated file ids, keys strictly ascending.
/// Lending-style (the slice borrows the source) so producers can reuse one
/// ids buffer across millions of keys instead of allocating per key.
pub trait PostingsSource {
    fn next(&mut self) -> io::Result<Option<(u32, &[u32])>>;
}

/// Adapter for in-memory (key, ids) sequences (tests, tiny builds).
pub struct VecPostings {
    items: std::vec::IntoIter<(u32, Vec<u32>)>,
    cur: Vec<u32>,
}

impl VecPostings {
    pub fn new(items: Vec<(u32, Vec<u32>)>) -> Self {
        VecPostings {
            items: items.into_iter(),
            cur: Vec::new(),
        }
    }
}

impl PostingsSource for VecPostings {
    fn next(&mut self) -> io::Result<Option<(u32, &[u32])>> {
        match self.items.next() {
            None => Ok(None),
            Some((k, ids)) => {
                self.cur = ids;
                Ok(Some((k, &self.cur)))
            }
        }
    }
}

/// Write a complete index. The posting blob is streamed through a temp file
/// so peak memory stays at one posting list plus 16 bytes per trigram,
/// independent of index size.
pub fn write_index(
    path: &Path,
    root: &str,
    files: &[FileRecord],
    postings: impl PostingsSource,
) -> io::Result<()> {
    let tag = temp_tag();
    let tmp = temp_sibling(path, "new", &tag);
    let post_tmp = temp_sibling(path, "post", &tag);
    let res = write_index_streamed(&tmp, &post_tmp, root, files, postings).and_then(|()| {
        // Atomic-ish replace.
        match std::fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(_) => {
                // Windows: rename fails if target exists and is open; retry after remove.
                let _ = std::fs::remove_file(path);
                std::fs::rename(&tmp, path)
            }
        }
    });
    let _ = std::fs::remove_file(&post_tmp);
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

fn write_index_streamed(
    tmp: &Path,
    post_tmp: &Path,
    root: &str,
    files: &[FileRecord],
    mut postings: impl PostingsSource,
) -> io::Result<()> {
    // Encode postings first: the header needs the trigram count and blob
    // length up front. The blob goes to a temp file; only the fixed 16-byte
    // table entries are kept in memory.
    let mut tri_table: Vec<u8> = Vec::new();
    let mut post_len: u64 = 0;
    {
        let f = File::create(post_tmp)?;
        let mut pw = BufWriter::with_capacity(1 << 20, f);
        let mut buf: Vec<u8> = Vec::new();
        while let Some((key, ids)) = postings.next()? {
            buf.clear();
            let mut prev = 0u32;
            for (i, &id) in ids.iter().enumerate() {
                let delta = if i == 0 { id } else { id - prev };
                varint::write_u64(&mut buf, u64::from(delta));
                prev = id;
            }
            tri_table.extend_from_slice(&key.to_le_bytes());
            tri_table.extend_from_slice(&(buf.len() as u32).to_le_bytes());
            tri_table.extend_from_slice(&post_len.to_le_bytes());
            pw.write_all(&buf)?;
            post_len += buf.len() as u64;
        }
        pw.flush()?;
    }
    let tri_count = (tri_table.len() / 16) as u64;

    let f = File::create(tmp)?;
    let mut w = BufWriter::with_capacity(1 << 20, f);

    // Derived section: ids of files a search must always scan. Stored so a
    // query touches only its candidates, never the whole file table.
    let scan_always: Vec<u32> = files
        .iter()
        .enumerate()
        .filter(|(_, fr)| fr.flags & FLAG_SCAN_ALWAYS != 0)
        .map(|(i, _)| i as u32)
        .collect();

    // Lay out variable sections (all lengths are known now).
    let root_off = HEADER_LEN as u64;
    let root_len = root.len() as u64;

    let paths_off = root_off + root_len;
    let mut paths_len: u64 = 0;
    for fr in files {
        paths_len += fr.rel_path.len() as u64;
    }

    let file_table_off = paths_off + paths_len;
    let file_table_len = files.len() as u64 * 28;

    let scan_always_off = file_table_off + file_table_len;
    let scan_always_len = scan_always.len() as u64 * 4;

    let tri_table_off = scan_always_off + scan_always_len;
    let tri_table_len = tri_count * 16;
    let postings_off = tri_table_off + tri_table_len;

    // Header (fixed 128 bytes; trailing bytes reserved as zeros).
    w.write_all(MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())?;
    w.write_all(&0u32.to_le_bytes())?; // reserved
    w.write_all(&(files.len() as u64).to_le_bytes())?;
    w.write_all(&tri_count.to_le_bytes())?;
    w.write_all(&file_table_off.to_le_bytes())?;
    w.write_all(&paths_off.to_le_bytes())?;
    w.write_all(&paths_len.to_le_bytes())?;
    w.write_all(&tri_table_off.to_le_bytes())?;
    w.write_all(&postings_off.to_le_bytes())?;
    w.write_all(&post_len.to_le_bytes())?;
    w.write_all(&root_off.to_le_bytes())?;
    w.write_all(&root_len.to_le_bytes())?;
    w.write_all(&scan_always_off.to_le_bytes())?;
    w.write_all(&(scan_always.len() as u64).to_le_bytes())?;
    w.write_all(&[0u8; HEADER_LEN - 112])?; // reserved tail

    // Sections.
    w.write_all(root.as_bytes())?;
    for fr in files {
        // paths blob
        w.write_all(fr.rel_path.as_bytes())?;
    }
    let mut off_acc: u32 = 0;
    for fr in files {
        w.write_all(&off_acc.to_le_bytes())?;
        w.write_all(&(fr.rel_path.len() as u32).to_le_bytes())?;
        w.write_all(&fr.size.to_le_bytes())?;
        w.write_all(&fr.mtime.to_le_bytes())?;
        w.write_all(&fr.flags.to_le_bytes())?;
        off_acc += fr.rel_path.len() as u32;
    }
    for &id in &scan_always {
        w.write_all(&id.to_le_bytes())?;
    }
    w.write_all(&tri_table)?;
    let mut pf = File::open(post_tmp)?;
    io::copy(&mut pf, &mut w)?;
    w.flush()?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct FileMeta<'a> {
    pub rel_path: &'a str,
    pub size: u64,
    pub mtime: u64,
    pub flags: u32,
}

pub struct IndexReader {
    mmap: Mmap,
    file_count: usize,
    tri_count: usize,
    file_table_off: usize,
    paths_off: usize,
    tri_table_off: usize,
    postings_off: usize,
    root_range: (usize, usize),
    scan_always_off: usize,
    scan_always_count: usize,
}

impl IndexReader {
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        let f = File::open(path)?;
        // Safety: we treat the mmap as a plain byte slice and bounds-check
        // every access. Concurrent truncation can still fault the process on
        // some platforms; the index is replaced atomically via rename to
        // avoid that in normal operation.
        let mmap = unsafe { Mmap::map(&f)? };
        Self::parse(mmap)
    }

    fn parse(mmap: Mmap) -> Result<Self, IndexError> {
        let buf: &[u8] = &mmap;
        if buf.len() < 12 || &buf[..7] != b"GRIXIDX" {
            return Err(IndexError::Corrupt("bad magic"));
        }
        if &buf[..8] != MAGIC {
            // An older grix wrote this; the caller rebuilds it.
            let v = u32::from_le_bytes(buf[8..12].try_into().unwrap());
            return Err(IndexError::WrongVersion(v));
        }
        if buf.len() < HEADER_LEN {
            return Err(IndexError::Corrupt("truncated header"));
        }
        let u32_at =
            |off: usize| -> u32 { u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) };
        let u64_at =
            |off: usize| -> u64 { u64::from_le_bytes(buf[off..off + 8].try_into().unwrap()) };
        let version = u32_at(8);
        if version != VERSION {
            return Err(IndexError::WrongVersion(version));
        }
        let file_count = u64_at(16) as usize;
        let tri_count = u64_at(24) as usize;
        let file_table_off = u64_at(32) as usize;
        let paths_off = u64_at(40) as usize;
        let paths_len = u64_at(48) as usize;
        let tri_table_off = u64_at(56) as usize;
        let postings_off = u64_at(64) as usize;
        let postings_len = u64_at(72) as usize;
        let root_off = u64_at(80) as usize;
        let root_len = u64_at(88) as usize;
        let scan_always_off = u64_at(96) as usize;
        let scan_always_count = u64_at(104) as usize;

        // Validate section bounds once so accessors can stay cheap.
        let need = |off: usize, len: usize, what: &'static str| -> Result<(), IndexError> {
            if off.checked_add(len).map_or(true, |end| end > buf.len()) {
                Err(IndexError::Corrupt(what))
            } else {
                Ok(())
            }
        };
        need(root_off, root_len, "root out of bounds")?;
        need(paths_off, paths_len, "paths out of bounds")?;
        need(
            file_table_off,
            file_count
                .checked_mul(28)
                .ok_or(IndexError::Corrupt("file table overflow"))?,
            "file table out of bounds",
        )?;
        need(
            tri_table_off,
            tri_count
                .checked_mul(16)
                .ok_or(IndexError::Corrupt("tri table overflow"))?,
            "tri table out of bounds",
        )?;
        need(postings_off, postings_len, "postings out of bounds")?;
        need(
            scan_always_off,
            scan_always_count
                .checked_mul(4)
                .ok_or(IndexError::Corrupt("scan-always overflow"))?,
            "scan-always out of bounds",
        )?;
        if scan_always_count > file_count {
            return Err(IndexError::Corrupt("scan-always count out of range"));
        }
        std::str::from_utf8(&buf[root_off..root_off + root_len])
            .map_err(|_| IndexError::Corrupt("root not utf-8"))?;

        Ok(IndexReader {
            mmap,
            file_count,
            tri_count,
            file_table_off,
            paths_off,
            tri_table_off,
            postings_off,
            root_range: (root_off, root_len),
            scan_always_off,
            scan_always_count,
        })
    }

    fn buf(&self) -> &[u8] {
        &self.mmap
    }

    pub fn root(&self) -> &str {
        let (off, len) = self.root_range;
        // Validated in parse().
        std::str::from_utf8(&self.buf()[off..off + len]).unwrap_or("")
    }

    pub fn file_count(&self) -> usize {
        self.file_count
    }

    pub fn trigram_count(&self) -> usize {
        self.tri_count
    }

    /// Ids of files a search must always scan (too large to index),
    /// ascending. O(that list), not O(all files).
    pub fn scan_always_ids(&self) -> impl Iterator<Item = u32> + '_ {
        let buf = self.buf();
        (0..self.scan_always_count).map(move |i| {
            let e = self.scan_always_off + i * 4;
            u32::from_le_bytes(buf[e..e + 4].try_into().unwrap())
        })
    }

    pub fn file(&self, id: u32) -> Result<FileMeta<'_>, IndexError> {
        let id = id as usize;
        if id >= self.file_count {
            return Err(IndexError::Corrupt("file id out of range"));
        }
        let buf = self.buf();
        let e = self.file_table_off + id * 28;
        let path_off = u32::from_le_bytes(buf[e..e + 4].try_into().unwrap()) as usize;
        let path_len = u32::from_le_bytes(buf[e + 4..e + 8].try_into().unwrap()) as usize;
        let size = u64::from_le_bytes(buf[e + 8..e + 16].try_into().unwrap());
        let mtime = u64::from_le_bytes(buf[e + 16..e + 24].try_into().unwrap());
        let flags = u32::from_le_bytes(buf[e + 24..e + 28].try_into().unwrap());
        let rel_path = self
            .paths_off
            .checked_add(path_off)
            .and_then(|p0| Some(p0..p0.checked_add(path_len)?))
            .and_then(|r| buf.get(r))
            .and_then(|b| std::str::from_utf8(b).ok())
            .ok_or(IndexError::Corrupt("bad path entry"))?;
        Ok(FileMeta {
            rel_path,
            size,
            mtime,
            flags,
        })
    }

    /// Decode the posting list for a trigram. Empty vec when absent.
    pub fn postings(&self, key: u32) -> Result<Vec<u32>, IndexError> {
        let buf = self.buf();
        let (mut lo, mut hi) = (0usize, self.tri_count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let e = self.tri_table_off + mid * 16;
            let k = u32::from_le_bytes(buf[e..e + 4].try_into().unwrap());
            match k.cmp(&key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let len = u32::from_le_bytes(buf[e + 4..e + 8].try_into().unwrap()) as usize;
                    let off = u64::from_le_bytes(buf[e + 8..e + 16].try_into().unwrap()) as usize;
                    let bytes = posting_bytes(buf, self.postings_off, off, len)?;
                    return decode_postings(bytes, self.file_count);
                }
            }
        }
        Ok(Vec::new())
    }

    /// Iterate (key, decoded ids) over every trigram, for incremental merges.
    pub fn iter_postings(&self) -> impl Iterator<Item = Result<(u32, Vec<u32>), IndexError>> + '_ {
        let buf = self.buf();
        (0..self.tri_count).map(move |i| {
            let e = self.tri_table_off + i * 16;
            let k = u32::from_le_bytes(buf[e..e + 4].try_into().unwrap());
            let len = u32::from_le_bytes(buf[e + 4..e + 8].try_into().unwrap()) as usize;
            let off = u64::from_le_bytes(buf[e + 8..e + 16].try_into().unwrap()) as usize;
            let bytes = posting_bytes(buf, self.postings_off, off, len)?;
            Ok((k, decode_postings(bytes, self.file_count)?))
        })
    }
}

/// Slice one posting list out of the blob with overflow-checked offsets, so
/// a corrupt trigram-table entry errors instead of wrapping in release mode.
fn posting_bytes(
    buf: &[u8],
    postings_off: usize,
    off: usize,
    len: usize,
) -> Result<&[u8], IndexError> {
    postings_off
        .checked_add(off)
        .and_then(|p0| Some(p0..p0.checked_add(len)?))
        .and_then(|r| buf.get(r))
        .ok_or(IndexError::Corrupt("postings out of bounds"))
}

fn decode_postings(bytes: &[u8], file_count: usize) -> Result<Vec<u32>, IndexError> {
    let mut ids = Vec::new();
    let mut pos = 0usize;
    let mut prev: u64 = 0;
    let mut first = true;
    while pos < bytes.len() {
        let (delta, np) =
            varint::read_u64(bytes, pos).ok_or(IndexError::Corrupt("truncated postings"))?;
        pos = np;
        let id = if first { delta } else { prev + delta };
        first = false;
        if id >= file_count as u64 {
            return Err(IndexError::Corrupt("posting id out of range"));
        }
        ids.push(id as u32);
        prev = id;
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn sample_files() -> Vec<FileRecord> {
        vec![
            FileRecord {
                rel_path: "src/main.rs".into(),
                size: 100,
                mtime: 1,
                flags: 0,
            },
            FileRecord {
                rel_path: "README.md".into(),
                size: 200,
                mtime: 2,
                flags: 0,
            },
            FileRecord {
                rel_path: "big.bin".into(),
                size: 1 << 30,
                mtime: 3,
                flags: FLAG_SCAN_ALWAYS,
            },
        ]
    }

    #[test]
    fn write_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let idx = dir.path().join("t.gix");
        let files = sample_files();
        let mut postings = BTreeMap::new();
        postings.insert(crate::trigram::pack_str(b"abc"), vec![0u32, 2]);
        postings.insert(crate::trigram::pack_str(b"bcd"), vec![1u32]);
        postings.insert(crate::trigram::pack_str(b"zzz"), vec![0u32, 1, 2]);
        write_index(
            &idx,
            "C:/repo",
            &files,
            VecPostings::new(postings.into_iter().collect()),
        )
        .unwrap();

        let r = IndexReader::open(&idx).unwrap();
        assert_eq!(r.root(), "C:/repo");
        assert_eq!(r.file_count(), 3);
        assert_eq!(r.trigram_count(), 3);
        assert_eq!(r.file(0).unwrap().rel_path, "src/main.rs");
        assert_eq!(r.file(2).unwrap().flags, FLAG_SCAN_ALWAYS);
        assert_eq!(r.scan_always_ids().collect::<Vec<_>>(), vec![2]);
        assert_eq!(
            r.postings(crate::trigram::pack_str(b"abc")).unwrap(),
            vec![0, 2]
        );
        assert_eq!(
            r.postings(crate::trigram::pack_str(b"zzz")).unwrap(),
            vec![0, 1, 2]
        );
        assert!(r
            .postings(crate::trigram::pack_str(b"qqq"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn rejects_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let idx = dir.path().join("bad.gix");
        std::fs::write(&idx, b"not an index at all").unwrap();
        assert!(IndexReader::open(&idx).is_err());
    }

    fn temp_leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect()
    }

    struct BoomSource;
    impl PostingsSource for BoomSource {
        fn next(&mut self) -> io::Result<Option<(u32, &[u32])>> {
            Err(io::Error::other("boom"))
        }
    }

    #[test]
    fn failed_write_keeps_index_and_leaves_no_temps() {
        let dir = tempfile::tempdir().unwrap();
        let idx = dir.path().join("t.gix");
        let files = sample_files();
        let mut postings = BTreeMap::new();
        postings.insert(crate::trigram::pack_str(b"abc"), vec![0u32]);
        write_index(
            &idx,
            "r",
            &files,
            VecPostings::new(postings.into_iter().collect()),
        )
        .unwrap();
        assert!(temp_leftovers(dir.path()).is_empty());

        assert!(write_index(&idx, "r", &files, BoomSource).is_err());
        // The failed attempt must not clobber the live index nor leave temps.
        assert!(IndexReader::open(&idx).is_ok());
        assert!(
            temp_leftovers(dir.path()).is_empty(),
            "{:?}",
            temp_leftovers(dir.path())
        );
    }

    #[test]
    fn sweep_removes_only_stale_matching_temps() {
        let dir = tempfile::tempdir().unwrap();
        let idx = dir.path().join("k.gix");
        std::fs::write(&idx, b"index").unwrap();
        let marker = dir.path().join("k.watch");
        std::fs::write(&marker, b"hb").unwrap();
        let ours = dir.path().join("k.gix.post.999-0.tmp");
        std::fs::write(&ours, b"junk").unwrap();
        let other_index = dir.path().join("other.gix.new.1-1.tmp");
        std::fs::write(&other_index, b"junk").unwrap();

        // Zero age: everything of ours qualifies; the index, the watch
        // marker and other indexes' temps must survive.
        std::thread::sleep(Duration::from_millis(20));
        sweep_stale_temps_older_than(&idx, Duration::ZERO);
        assert!(!ours.exists());
        assert!(idx.exists() && marker.exists() && other_index.exists());

        // Default age: a fresh temp (a build in flight) survives.
        let fresh = dir.path().join("k.gix.shard0.1-2.tmp");
        std::fs::write(&fresh, b"junk").unwrap();
        sweep_stale_temps(&idx);
        assert!(fresh.exists());
    }
}
