//! Index construction: full builds and incremental updates.
//!
//! Build pipeline:
//! 1. Walk the tree (gitignore-aware) and collect candidate files, sorted by
//!    relative path so file ids are deterministic.
//! 2. For an incremental update, files whose (size, mtime) match the old
//!    index are *reused*: their postings are recovered from the old index in
//!    one linear pass (old id -> new id remap) without touching the files.
//! 3. Changed/new files are read and trigram-extracted in parallel; results
//!    are merged as they arrive, never accumulated.
//! 4. Posting lists are kept under a memory budget: when the in-memory map
//!    exceeds `BuildOptions::spill_budget` it is flushed to a sorted temp
//!    shard and the shards are k-way merged at write time, so build memory
//!    stays bounded no matter how large the tree is.
//! 5. The merged stream is written atomically (see `format::write_index`).

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::UNIX_EPOCH;

use super::format::{self, FileRecord, IndexReader, FLAG_BINARY, FLAG_SCAN_ALWAYS};
use crate::trigram;
use crate::varint;

#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// Files larger than this are not indexed; they are recorded and always
    /// scanned at search time so results stay complete.
    pub max_file_size: u64,
    pub threads: usize,
    /// Approximate in-memory postings budget in bytes. Above it the partial
    /// map is spilled to a temp shard next to the index and merged back at
    /// write time.
    pub spill_budget: usize,
}

impl Default for BuildOptions {
    fn default() -> Self {
        BuildOptions {
            max_file_size: 16 << 20,
            threads: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
            spill_budget: 512 << 20,
        }
    }
}

#[derive(Debug, Default)]
pub struct BuildStats {
    pub files_total: usize,
    pub files_indexed: usize,
    pub files_reused: usize,
    pub files_binary: usize,
    pub files_scan_always: usize,
    /// Files actually read and trigram-extracted this build (changed/new).
    pub files_extracted: usize,
    pub bytes_read: u64,
}

struct Candidate {
    rel_path: String,
    size: u64,
    mtime: u64,
}

fn mtime_nanos(md: &std::fs::Metadata) -> u64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Walk `root` and return indexable candidates sorted by relative path.
fn collect_candidates(root: &Path) -> io::Result<Vec<Candidate>> {
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(root).build();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue, // unreadable entries are skipped, not fatal
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let rel = match entry.path().strip_prefix(root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let rel_path = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if rel_path.is_empty() {
            continue;
        }
        out.push(Candidate {
            rel_path,
            size: md.len(),
            mtime: mtime_nanos(&md),
        });
    }
    out.sort_unstable_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(out)
}

enum Extracted {
    Indexed(Vec<u32>),
    Binary,
}

/// Read + classify + extract one file. Returns the extraction and bytes read.
fn extract_file(abs: &Path) -> io::Result<(Extracted, u64)> {
    let data = std::fs::read(abs)?;
    if trigram::looks_binary(&data) {
        return Ok((Extracted::Binary, 0));
    }
    let bytes = data.len() as u64;
    Ok((Extracted::Indexed(trigram::extract_sorted(&data)), bytes))
}

/// Build (or incrementally rebuild) the index for `root` into `index_path`.
pub fn build(
    root: &Path,
    index_path: &Path,
    old: Option<&IndexReader>,
    opts: &BuildOptions,
) -> io::Result<BuildStats> {
    let mut stats = BuildStats::default();
    // Reclaim temps a crashed build may have left next to the index (their
    // names are unique per attempt, so nothing else ever overwrites them).
    format::sweep_stale_temps(index_path);
    let candidates = collect_candidates(root)?;
    stats.files_total = candidates.len();

    // Map old files by path for change detection.
    let mut old_by_path: HashMap<&str, (u32, u64, u64, u32)> = HashMap::new();
    if let Some(old) = old {
        for id in 0..old.file_count() as u32 {
            if let Ok(m) = old.file(id) {
                old_by_path.insert(m.rel_path, (id, m.size, m.mtime, m.flags));
            }
        }
    }

    // Final file records (ids = position) + work classification.
    let mut records: Vec<FileRecord> = Vec::with_capacity(candidates.len());
    // old id -> new id (u32::MAX = dropped / re-extracted)
    let mut remap: Vec<u32> = vec![u32::MAX; old.map_or(0, |o| o.file_count())];
    // (new_id, rel_path) pending extraction
    let mut to_extract: Vec<(u32, String)> = Vec::new();

    for cand in &candidates {
        let new_id = records.len() as u32;
        let too_large = cand.size > opts.max_file_size;
        let mut flags = if too_large { FLAG_SCAN_ALWAYS } else { 0 };
        let mut reuse_from: Option<u32> = None;

        if let Some(&(old_id, osize, omtime, oflags)) = old_by_path.get(cand.rel_path.as_str()) {
            if osize == cand.size && omtime == cand.mtime {
                // Unchanged. Keep its classification (indexed/binary/large)
                // unless the size cap moved it across the boundary.
                let was_large = oflags & FLAG_SCAN_ALWAYS != 0;
                if was_large == too_large {
                    flags = oflags;
                    reuse_from = Some(old_id);
                }
            }
        }

        match reuse_from {
            Some(old_id) => {
                stats.files_reused += 1;
                if flags == 0 {
                    // postings recovered from the old index below
                    remap[old_id as usize] = new_id;
                }
            }
            None if too_large => {}
            None => to_extract.push((new_id, cand.rel_path.clone())),
        }

        records.push(FileRecord {
            rel_path: cand.rel_path.clone(),
            size: cand.size,
            mtime: cand.mtime,
            flags,
        });
    }

    // Unchanged tree: every file reused, nothing added or removed. Reused
    // records copy the old flags/size/mtime and ids keep their order, so the
    // index we would write is byte-identical to the one on disk — skip the
    // postings decode/remap/re-encode and the rewrite entirely.
    if old.is_some_and(|o| {
        to_extract.is_empty()
            && stats.files_reused == candidates.len()
            && o.file_count() == candidates.len()
    }) {
        fill_flag_stats(&mut stats, &records);
        return Ok(stats);
    }

    // Bounded postings accumulator; overflow goes to temp shards.
    let mut acc = PostingsAcc::new(index_path, opts.spill_budget);

    // Postings recovered from the old index in one linear pass.
    if let Some(old) = old {
        for item in old.iter_postings() {
            let (key, ids) = match item {
                Ok(kv) => kv,
                Err(_) => {
                    // Corrupt old index: fall back to a full rebuild.
                    acc.reset()?;
                    for r in remap.iter_mut() {
                        *r = u32::MAX;
                    }
                    let mut seen: std::collections::HashSet<u32> =
                        to_extract.iter().map(|(id, _)| *id).collect();
                    for (i, rec) in records.iter().enumerate() {
                        let id = i as u32;
                        if rec.flags == 0 && !seen.contains(&id) {
                            to_extract.push((id, rec.rel_path.clone()));
                            seen.insert(id);
                        }
                    }
                    stats.files_reused = 0;
                    break;
                }
            };
            // Survivors keep their relative order, so the remapped list
            // stays sorted.
            let mapped: Vec<u32> = ids
                .into_iter()
                .filter_map(|oid| {
                    let nid = remap[oid as usize];
                    (nid != u32::MAX).then_some(nid)
                })
                .collect();
            if !mapped.is_empty() {
                acc.add_list(key, mapped)?;
            }
        }
    }

    // Parallel extraction of changed/new files. Results are drained here as
    // they arrive so per-file trigram sets never pile up in memory; the
    // bounded channel provides backpressure while a spill is in progress.
    stats.files_extracted = to_extract.len();
    let next = AtomicUsize::new(0);
    let nthreads = opts.threads.max(1).min(to_extract.len().max(1));
    let (tx, rx) = mpsc::sync_channel::<(u32, Extracted, u64)>(nthreads * 2);
    let mut drain_err: Option<io::Error> = None;
    std::thread::scope(|s| {
        for _ in 0..nthreads {
            let tx = tx.clone();
            let next = &next;
            let to_extract = &to_extract;
            s.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some((new_id, rel)) = to_extract.get(i) else {
                    break;
                };
                let abs = root.join(rel);
                let msg = match extract_file(&abs) {
                    Ok((ex, bytes)) => (*new_id, ex, bytes),
                    // File vanished or unreadable: mark binary so it is
                    // excluded from search rather than half-indexed.
                    Err(_) => (*new_id, Extracted::Binary, 0),
                };
                if tx.send(msg).is_err() {
                    break; // receiver bailed on an io error
                }
            });
        }
        drop(tx);
        for (new_id, ex, bytes) in rx {
            stats.bytes_read += bytes;
            match ex {
                Extracted::Binary => {
                    records[new_id as usize].flags = FLAG_BINARY;
                }
                Extracted::Indexed(tris) => {
                    if let Err(e) = acc.add_file(new_id, &tris) {
                        drain_err = Some(e);
                        break;
                    }
                }
            }
        }
    });
    if let Some(e) = drain_err {
        return Err(e);
    }

    fill_flag_stats(&mut stats, &records);

    // Sorted postings stream: in-memory for small builds, k-way shard merge
    // for spilled ones. The guard deletes shard files after the write.
    let (postings, _shards) = acc.into_postings()?;
    let root_str = root.to_string_lossy().replace('\\', "/");
    format::write_index(index_path, &root_str, &records, postings)?;
    Ok(stats)
}

fn fill_flag_stats(stats: &mut BuildStats, records: &[FileRecord]) {
    stats.files_indexed = records.iter().filter(|r| r.flags == 0).count();
    stats.files_binary = records
        .iter()
        .filter(|r| r.flags & FLAG_BINARY != 0)
        .count();
    stats.files_scan_always = records
        .iter()
        .filter(|r| r.flags & FLAG_SCAN_ALWAYS != 0)
        .count();
}

/// Approximate per-new-trigram bookkeeping cost (map slot + vec header).
const KEY_OVERHEAD: usize = 64;

/// Bounded (trigram -> file ids) accumulator. Ids arrive in completion
/// order; each spill sorts + dedups its keys and writes one shard file, so
/// peak memory stays near `budget` however many text files the tree has.
struct PostingsAcc {
    map: HashMap<u32, Vec<u32>>,
    payload: usize,
    budget: usize,
    index_path: PathBuf,
    /// Unique per build attempt so concurrent builds never share shard files.
    tag: String,
    shards: Vec<PathBuf>,
}

impl PostingsAcc {
    fn new(index_path: &Path, budget: usize) -> Self {
        PostingsAcc {
            map: HashMap::new(),
            payload: 0,
            budget: budget.max(1),
            index_path: index_path.to_path_buf(),
            tag: format::temp_tag(),
            shards: Vec::new(),
        }
    }

    fn push(&mut self, key: u32, id: u32) {
        match self.map.entry(key) {
            Entry::Occupied(mut e) => e.get_mut().push(id),
            Entry::Vacant(e) => {
                e.insert(vec![id]);
                self.payload += KEY_OVERHEAD;
            }
        }
        self.payload += 4;
    }

    /// One extracted file's sorted distinct trigrams.
    fn add_file(&mut self, id: u32, tris: &[u32]) -> io::Result<()> {
        for &t in tris {
            self.push(t, id);
        }
        self.spill_if_over()
    }

    /// Sorted, deduplicated ids recovered from the old index.
    fn add_list(&mut self, key: u32, ids: Vec<u32>) -> io::Result<()> {
        self.payload += 4 * ids.len();
        match self.map.entry(key) {
            Entry::Occupied(mut e) => e.get_mut().extend(ids),
            Entry::Vacant(e) => {
                e.insert(ids);
                self.payload += KEY_OVERHEAD;
            }
        }
        self.spill_if_over()
    }

    fn spill_if_over(&mut self) -> io::Result<()> {
        if self.payload > self.budget {
            self.spill()?;
        }
        Ok(())
    }

    /// Drop everything accumulated so far (corrupt-old-index fallback).
    fn reset(&mut self) -> io::Result<()> {
        self.map = HashMap::new();
        self.payload = 0;
        for p in self.shards.drain(..) {
            let _ = std::fs::remove_file(p);
        }
        Ok(())
    }

    fn shard_path(&self, n: usize) -> PathBuf {
        format::temp_sibling(&self.index_path, &format!("shard{n}"), &self.tag)
    }

    /// Flush the in-memory map as one shard: records of
    /// [key u32][id count u32][delta-varint ids], keys ascending.
    fn spill(&mut self) -> io::Result<()> {
        if self.map.is_empty() {
            return Ok(());
        }
        let path = self.shard_path(self.shards.len());
        let mut keys: Vec<u32> = self.map.keys().copied().collect();
        keys.sort_unstable();
        let f = File::create(&path)?;
        let mut w = BufWriter::with_capacity(1 << 20, f);
        let mut buf: Vec<u8> = Vec::new();
        for k in keys {
            let mut ids = self.map.remove(&k).unwrap();
            ids.sort_unstable();
            ids.dedup();
            buf.clear();
            let mut prev = 0u32;
            for (i, &id) in ids.iter().enumerate() {
                let delta = if i == 0 { id } else { id - prev };
                varint::write_u64(&mut buf, u64::from(delta));
                prev = id;
            }
            w.write_all(&k.to_le_bytes())?;
            w.write_all(&(ids.len() as u32).to_le_bytes())?;
            w.write_all(&buf)?;
        }
        w.flush()?;
        self.map = HashMap::new(); // release the emptied table's capacity too
        self.payload = 0;
        self.shards.push(path);
        Ok(())
    }

    /// Consume into a sorted postings stream plus a guard that deletes the
    /// shard files once the stream has been consumed and dropped.
    fn into_postings(mut self) -> io::Result<(PostingsIter, ShardGuard)> {
        if self.shards.is_empty() {
            // Small build: same pure in-memory path as before spilling existed.
            let mut keys: Vec<u32> = self.map.keys().copied().collect();
            keys.sort_unstable();
            let mut ordered: Vec<(u32, Vec<u32>)> = Vec::with_capacity(keys.len());
            for k in keys {
                let mut ids = self.map.remove(&k).unwrap();
                ids.sort_unstable();
                ids.dedup();
                ordered.push((k, ids));
            }
            return Ok((
                PostingsIter::Mem(ordered.into_iter()),
                ShardGuard(Vec::new()),
            ));
        }
        self.spill()?; // flush the remainder
        let mut cursors = Vec::with_capacity(self.shards.len());
        for p in &self.shards {
            let mut c = ShardCursor {
                r: BufReader::with_capacity(64 << 10, File::open(p)?),
                next: None,
            };
            c.advance()?;
            cursors.push(c);
        }
        let guard = ShardGuard(std::mem::take(&mut self.shards));
        Ok((PostingsIter::Merge(MergeCursors { cursors }), guard))
    }
}

impl Drop for PostingsAcc {
    fn drop(&mut self) {
        for p in self.shards.drain(..) {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Deletes spilled shard files on drop (after the index has been written).
struct ShardGuard(Vec<PathBuf>);

impl Drop for ShardGuard {
    fn drop(&mut self) {
        for p in self.0.drain(..) {
            let _ = std::fs::remove_file(p);
        }
    }
}

struct ShardCursor {
    r: BufReader<File>,
    next: Option<(u32, Vec<u32>)>,
}

impl ShardCursor {
    fn advance(&mut self) -> io::Result<()> {
        let mut kb = [0u8; 4];
        match self.r.read_exact(&mut kb) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                self.next = None;
                return Ok(());
            }
            Err(e) => return Err(e),
        }
        let key = u32::from_le_bytes(kb);
        let mut cb = [0u8; 4];
        self.r.read_exact(&mut cb)?;
        let count = u32::from_le_bytes(cb) as usize;
        let mut ids = Vec::with_capacity(count);
        let mut prev = 0u64;
        for i in 0..count {
            let delta = read_varint(&mut self.r)?;
            let id = if i == 0 { delta } else { prev + delta };
            ids.push(id as u32);
            prev = id;
        }
        self.next = Some((key, ids));
        Ok(())
    }
}

fn read_varint(r: &mut impl Read) -> io::Result<u64> {
    let mut v = 0u64;
    let mut shift = 0u32;
    loop {
        let mut b = [0u8; 1];
        r.read_exact(&mut b)?;
        if shift >= 64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "overlong varint in shard",
            ));
        }
        v |= u64::from(b[0] & 0x7f) << shift;
        if b[0] & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
    }
}

struct MergeCursors {
    cursors: Vec<ShardCursor>,
}

impl MergeCursors {
    fn next_merged(&mut self) -> io::Result<Option<(u32, Vec<u32>)>> {
        let Some(min) = self
            .cursors
            .iter()
            .filter_map(|c| c.next.as_ref().map(|(k, _)| *k))
            .min()
        else {
            return Ok(None);
        };
        let mut ids: Vec<u32> = Vec::new();
        for c in &mut self.cursors {
            if c.next.as_ref().is_some_and(|(k, _)| *k == min) {
                let (_, list) = c.next.take().unwrap();
                ids.extend(list);
                c.advance()?;
            }
        }
        ids.sort_unstable();
        ids.dedup();
        Ok(Some((min, ids)))
    }
}

/// Sorted (key, ids) stream fed to `format::write_index`.
enum PostingsIter {
    Mem(std::vec::IntoIter<(u32, Vec<u32>)>),
    Merge(MergeCursors),
}

impl Iterator for PostingsIter {
    type Item = io::Result<(u32, Vec<u32>)>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            PostingsIter::Mem(it) => it.next().map(Ok),
            PostingsIter::Merge(m) => m.next_merged().transpose(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tree(dir: &Path) {
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        for i in 0..24 {
            let body = format!(
                "file {i}\ncommon needle_alpha shared across files\nunique_token_{i:04}\n{}\n",
                "filler ".repeat(i % 10)
            );
            std::fs::write(dir.join(format!("f{i:02}.txt")), body).unwrap();
        }
        std::fs::write(
            sub.join("nested.rs"),
            "fn main() { println!(\"needle_alpha\"); }\n",
        )
        .unwrap();
        std::fs::write(dir.join("bin.dat"), b"\x00\x01\x02binary blob\x00").unwrap();
        std::fs::write(dir.join("huge.log"), "x".repeat(1000)).unwrap();
    }

    fn opts(spill_budget: usize) -> BuildOptions {
        BuildOptions {
            max_file_size: 512, // "huge.log" (1000 B) becomes FLAG_SCAN_ALWAYS
            threads: 4,
            spill_budget,
        }
    }

    #[test]
    fn spilled_build_is_byte_identical_to_in_memory() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("tree");
        std::fs::create_dir_all(&root).unwrap();
        write_tree(&root);
        let idx_mem = t.path().join("mem.gix");
        let idx_spill = t.path().join("spill.gix");

        let s1 = build(&root, &idx_mem, None, &opts(usize::MAX)).unwrap();
        let s2 = build(&root, &idx_spill, None, &opts(2048)).unwrap();

        assert_eq!(s1.files_indexed, s2.files_indexed);
        assert!(s1.files_indexed >= 25);
        assert_eq!(s1.files_binary, 1);
        assert_eq!(s1.files_scan_always, 1);
        let a = std::fs::read(&idx_mem).unwrap();
        let b = std::fs::read(&idx_spill).unwrap();
        assert_eq!(a, b, "spilled build must produce the identical index");

        // Both indexes open and agree on shape.
        let r = IndexReader::open(&idx_spill).unwrap();
        assert_eq!(r.file_count(), s1.files_total);

        // No shard temps left behind.
        let leftovers: Vec<String> = std::fs::read_dir(t.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("shard"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "shard temps not cleaned: {leftovers:?}"
        );
    }

    #[test]
    fn unchanged_tree_skips_rewrite() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("tree");
        std::fs::create_dir_all(&root).unwrap();
        write_tree(&root);
        let idx = t.path().join("noop.gix");
        build(&root, &idx, None, &opts(usize::MAX)).unwrap();

        let mtime_before = std::fs::metadata(&idx).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));

        let old = IndexReader::open(&idx).unwrap();
        let stats = build(&root, &idx, Some(&old), &opts(usize::MAX)).unwrap();
        assert_eq!(stats.files_extracted, 0);
        assert_eq!(stats.files_reused, stats.files_total);
        assert_eq!(
            std::fs::metadata(&idx).unwrap().modified().unwrap(),
            mtime_before,
            "an unchanged tree must not rewrite the index file"
        );

        // A subsequent real change still lands.
        std::fs::write(root.join("f00.txt"), "changed body needle_alpha\n").unwrap();
        let old = IndexReader::open(&idx).unwrap();
        let stats = build(&root, &idx, Some(&old), &opts(usize::MAX)).unwrap();
        assert_eq!(stats.files_extracted, 1);
        assert!(std::fs::metadata(&idx).unwrap().modified().unwrap() > mtime_before);
    }

    #[test]
    fn incremental_spill_matches_fresh() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("tree");
        std::fs::create_dir_all(&root).unwrap();
        write_tree(&root);
        let idx = t.path().join("inc.gix");
        build(&root, &idx, None, &opts(2048)).unwrap();

        // Change one file and add one (size changes, so both are detected
        // without waiting on mtime granularity).
        std::fs::write(
            root.join("f03.txt"),
            "completely new body with needle_alpha and more text\n",
        )
        .unwrap();
        std::fs::write(
            root.join("f25_new.txt"),
            "a brand new file mentioning needle_alpha\n",
        )
        .unwrap();

        let old = IndexReader::open(&idx).unwrap();
        let stats = build(&root, &idx, Some(&old), &opts(2048)).unwrap();
        drop(old);
        assert!(stats.files_reused > 0, "reuse path must be exercised");

        let idx_fresh = t.path().join("fresh.gix");
        build(&root, &idx_fresh, None, &opts(usize::MAX)).unwrap();
        assert_eq!(
            std::fs::read(&idx).unwrap(),
            std::fs::read(&idx_fresh).unwrap(),
            "incremental spilled rebuild must equal a fresh in-memory build"
        );
    }
}
