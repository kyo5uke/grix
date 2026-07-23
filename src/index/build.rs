//! Index construction: full builds and incremental updates.
//!
//! Build pipeline (the "pair pipeline"):
//! 1. Walk the tree in parallel (gitignore-aware) and collect candidates,
//!    sorted by relative path so file ids are deterministic.
//! 2. For an incremental update, files whose (size, mtime) match the old
//!    index are *reused*: their postings stream out of the old index as one
//!    pre-sorted run (old id -> new id remap) without touching the files.
//! 3. Changed/new files are read in parallel; each file's distinct trigrams
//!    are found with a reusable bitmap (no per-file sort) and emitted as
//!    packed `(trigram << 32) | file_id` u64 pairs, batched to the collector.
//! 4. Pairs accumulate under `BuildOptions::spill_budget`; over budget the
//!    buffer is sorted once and written as a raw sorted run. At write time
//!    the in-memory run, the spilled runs and the old-index run are k-way
//!    merged into the final (key, ids) stream — no hash map anywhere.
//! 5. The merged stream is written atomically (see `format::write_index`).

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::UNIX_EPOCH;

use super::format::{self, FileRecord, IndexReader, PostingsSource, FLAG_BINARY, FLAG_SCAN_ALWAYS};
use crate::trigram::{self, TriSet};

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
            // 64 MiB: generous enough that virtually everything in a source
            // tree is indexed. Every scan-always file is re-scanned by every
            // query, so an unindexed 20 MB header would put a floor under
            // all search latencies (measured: ~30 ms on the kernel tree).
            max_file_size: 64 << 20,
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

pub struct Candidate {
    pub rel_path: String,
    pub size: u64,
    pub mtime: u64,
}

fn mtime_nanos(md: &std::fs::Metadata) -> u64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// `/`-separated relative path in one allocation.
fn rel_string(rel: &Path) -> String {
    let mut out = String::with_capacity(rel.as_os_str().len());
    for c in rel.components() {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&c.as_os_str().to_string_lossy());
    }
    out
}

/// Walk `root` in parallel and return indexable candidates sorted by
/// relative path (ids stay deterministic regardless of traversal order).
/// Directory walks are syscall-bound — on Windows especially — so the
/// parallel walker is the single biggest lever for refresh latency.
pub fn collect_candidates(root: &Path, threads: usize) -> io::Result<Vec<Candidate>> {
    let (tx, rx) = mpsc::channel::<Candidate>();
    let walker = ignore::WalkBuilder::new(root)
        .threads(threads.max(1))
        .build_parallel();
    walker.run(|| {
        let tx = tx.clone();
        let root = root.to_path_buf();
        Box::new(move |entry| {
            // Unreadable entries are skipped, not fatal.
            let Ok(entry) = entry else {
                return ignore::WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return ignore::WalkState::Continue;
            }
            let Ok(md) = entry.metadata() else {
                return ignore::WalkState::Continue;
            };
            let Ok(rel) = entry.path().strip_prefix(&root) else {
                return ignore::WalkState::Continue;
            };
            let rel_path = rel_string(rel);
            if !rel_path.is_empty() {
                let _ = tx.send(Candidate {
                    rel_path,
                    size: md.len(),
                    mtime: mtime_nanos(&md),
                });
            }
            ignore::WalkState::Continue
        })
    });
    drop(tx);
    let mut out: Vec<Candidate> = rx.into_iter().collect();
    out.sort_unstable_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(out)
}

/// One batch of extraction results, sent worker -> collector. Batching keeps
/// channel traffic to a few messages per MiB instead of one per file.
#[derive(Default)]
struct WorkMsg {
    /// Packed `(trigram << 32) | file_id` pairs.
    pairs: Vec<u64>,
    /// Files that turned out binary (or vanished mid-build).
    binaries: Vec<u32>,
    bytes: u64,
}

/// Pairs per message: 512K pairs = 4 MiB.
const FLUSH_PAIRS: usize = 1 << 19;

#[inline]
fn pack_pair(t: u32, id: u32) -> u64 {
    (u64::from(t) << 32) | u64::from(id)
}

#[inline]
fn pair_key(p: u64) -> u32 {
    (p >> 32) as u32
}

/// Read a whole file with a pre-sized buffer (the walk already knows the
/// size) and a sequential-access hint.
fn read_file(abs: &Path, size_hint: u64) -> io::Result<Vec<u8>> {
    let mut f = format::open_sequential(abs)?;
    let mut v = Vec::with_capacity(size_hint as usize + 16);
    f.read_to_end(&mut v)?;
    Ok(v)
}

/// Emit one file's distinct trigrams as pairs. O(bytes), no sorting: the
/// global run sort orders everything at once later.
fn extract_pairs(data: &[u8], id: u32, tri: &mut TriSet, out: &mut Vec<u64>) {
    if data.len() < 3 {
        return;
    }
    let mut t = (u32::from(data[0]) << 8) | u32::from(data[1]);
    for &b in &data[2..] {
        t = ((t << 8) | u32::from(b)) & 0x00ff_ffff;
        if tri.insert(t) {
            out.push(pack_pair(t, id));
        }
    }
    tri.clear();
}

/// Build (or incrementally rebuild) the index for `root` into `index_path`.
pub fn build(
    root: &Path,
    index_path: &Path,
    old: Option<&IndexReader>,
    opts: &BuildOptions,
) -> io::Result<BuildStats> {
    match build_inner(root, index_path, old, opts) {
        // A corrupt old index surfaces as InvalidData while its postings
        // stream through the merge; retry once from scratch (the same
        // recovery semantics as an upfront validation, without paying a
        // full decode on every healthy build).
        Err(e) if old.is_some() && e.kind() == io::ErrorKind::InvalidData => {
            build_inner(root, index_path, None, opts)
        }
        r => r,
    }
}

fn build_inner(
    root: &Path,
    index_path: &Path,
    old: Option<&IndexReader>,
    opts: &BuildOptions,
) -> io::Result<BuildStats> {
    let mut stats = BuildStats::default();
    // Reclaim temps a crashed build may have left next to the index (their
    // names are unique per attempt, so nothing else ever overwrites them).
    format::sweep_stale_temps(index_path);
    let candidates = collect_candidates(root, opts.threads)?;
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
    // (new_id, rel_path, size) pending extraction
    let mut to_extract: Vec<(u32, String, u64)> = Vec::new();

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
            None => to_extract.push((new_id, cand.rel_path.clone(), cand.size)),
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

    // Corrupt-old-index detection needs the classification to be redone from
    // scratch (see `build`), so nothing below may mutate `remap`.
    let remap = remap;

    // Parallel extraction of changed/new files. Workers emit batched pair
    // messages; this thread accumulates them under the spill budget. The
    // bounded channel provides backpressure while a spill is in progress.
    stats.files_extracted = to_extract.len();
    let mut acc = PairAcc::new(index_path, opts.spill_budget);
    let next = AtomicUsize::new(0);
    let nthreads = opts.threads.max(1).min(to_extract.len().max(1));
    let (tx, rx) = mpsc::sync_channel::<WorkMsg>(nthreads);
    let mut drain_err: Option<io::Error> = None;
    std::thread::scope(|s| {
        for _ in 0..nthreads {
            let tx = tx.clone();
            let next = &next;
            let to_extract = &to_extract;
            s.spawn(move || {
                let mut tri = TriSet::new();
                let mut msg = WorkMsg::default();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((new_id, rel, size)) = to_extract.get(i) else {
                        break;
                    };
                    let abs = root.join(rel);
                    match read_file(&abs, *size) {
                        Ok(data) if !trigram::looks_binary(&data) => {
                            msg.bytes += data.len() as u64;
                            extract_pairs(&data, *new_id, &mut tri, &mut msg.pairs);
                        }
                        // Binary, vanished or unreadable: exclude from
                        // search rather than half-index it.
                        _ => msg.binaries.push(*new_id),
                    }
                    if msg.pairs.len() >= FLUSH_PAIRS
                        && tx.send(std::mem::take(&mut msg)).is_err()
                    {
                        return; // receiver bailed on an io error
                    }
                }
                if !msg.pairs.is_empty() || !msg.binaries.is_empty() || msg.bytes > 0 {
                    let _ = tx.send(msg);
                }
            });
        }
        drop(tx);
        for msg in rx {
            stats.bytes_read += msg.bytes;
            for id in msg.binaries {
                records[id as usize].flags = FLAG_BINARY;
            }
            if let Err(e) = acc.extend(msg.pairs) {
                drain_err = Some(e);
                break;
            }
        }
    });
    if let Some(e) = drain_err {
        return Err(e);
    }

    fill_flag_stats(&mut stats, &records);

    // Merge the in-memory run, spilled runs and the old-index run into one
    // sorted (key, ids) stream. The guard deletes shard files after the
    // write completes.
    let old_run = match old {
        Some(o) => Some(OldRun::new(o, &remap)?),
        None => None,
    };
    let (merged, _guard) = acc.into_source(old_run)?;
    let root_str = root.to_string_lossy().replace('\\', "/");
    format::write_index(index_path, &root_str, &records, merged)?;
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

/// Bounded pair accumulator. Batches of packed `(trigram << 32) | id` pairs
/// arrive in completion order; over budget the buffer is sorted once and
/// written out as a raw little-endian u64 run, so peak memory stays near
/// `budget` however many text files the tree has.
struct PairAcc {
    pairs: Vec<u64>,
    /// Spill threshold in pairs (budget bytes / 8).
    cap: usize,
    index_path: PathBuf,
    /// Unique per build attempt so concurrent builds never share shard files.
    tag: String,
    shards: Vec<PathBuf>,
}

impl PairAcc {
    fn new(index_path: &Path, budget: usize) -> Self {
        PairAcc {
            pairs: Vec::new(),
            cap: (budget / 8).max(1),
            index_path: index_path.to_path_buf(),
            tag: format::temp_tag(),
            shards: Vec::new(),
        }
    }

    fn extend(&mut self, mut batch: Vec<u64>) -> io::Result<()> {
        self.pairs.append(&mut batch);
        if self.pairs.len() >= self.cap {
            self.spill()?;
        }
        Ok(())
    }

    fn shard_path(&self, n: usize) -> PathBuf {
        format::temp_sibling(&self.index_path, &format!("shard{n}"), &self.tag)
    }

    /// Sort the buffer and write it as one raw sorted u64 run.
    fn spill(&mut self) -> io::Result<()> {
        if self.pairs.is_empty() {
            return Ok(());
        }
        self.pairs.sort_unstable();
        let path = self.shard_path(self.shards.len());
        let f = File::create(&path)?;
        let mut w = BufWriter::with_capacity(1 << 20, f);
        for &p in &self.pairs {
            w.write_all(&p.to_le_bytes())?;
        }
        w.flush()?;
        // Keep the capacity: it is the accounted budget and will refill.
        self.pairs.clear();
        self.shards.push(path);
        Ok(())
    }

    /// Consume into the merged (key, ids) stream plus a guard that deletes
    /// the shard files once the stream has been consumed and dropped.
    fn into_source<'a>(mut self, old: Option<OldRun<'a>>) -> io::Result<(Merged<'a>, ShardGuard)> {
        // The in-memory remainder merges directly; no need to round-trip it
        // through disk.
        self.pairs.sort_unstable();
        let mut cursors = Vec::with_capacity(self.shards.len());
        for p in &self.shards {
            let mut c = ShardCursor {
                r: BufReader::with_capacity(1 << 20, File::open(p)?),
                next: None,
            };
            c.advance()?;
            cursors.push(c);
        }
        let guard = ShardGuard(std::mem::take(&mut self.shards));
        let merged = Merged {
            mem: std::mem::take(&mut self.pairs),
            mem_pos: 0,
            shards: cursors,
            old,
            ids: Vec::new(),
        };
        Ok((merged, guard))
    }
}

impl Drop for PairAcc {
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

/// Buffered cursor over one raw sorted u64 run.
struct ShardCursor {
    r: BufReader<File>,
    next: Option<u64>,
}

impl ShardCursor {
    fn advance(&mut self) -> io::Result<()> {
        let mut b = [0u8; 8];
        match self.r.read_exact(&mut b) {
            Ok(()) => {
                self.next = Some(u64::from_le_bytes(b));
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                self.next = None;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

type OldPostings<'a> = Box<dyn Iterator<Item = Result<(u32, Vec<u32>), format::IndexError>> + 'a>;

/// The old index streamed as a pre-sorted run: keys ascending, ids remapped
/// to new ids (survivors keep their relative order, so lists stay sorted).
/// Decode errors surface as `InvalidData`, which `build` turns into a
/// from-scratch rebuild.
struct OldRun<'a> {
    it: OldPostings<'a>,
    remap: &'a [u32],
    /// Next remapped, non-empty group.
    next: Option<(u32, Vec<u32>)>,
}

impl<'a> OldRun<'a> {
    fn new(old: &'a IndexReader, remap: &'a [u32]) -> io::Result<Self> {
        let mut run = OldRun {
            it: Box::new(old.iter_postings()),
            remap,
            next: None,
        };
        run.advance()?;
        Ok(run)
    }

    fn advance(&mut self) -> io::Result<()> {
        self.next = None;
        let remap = self.remap;
        for item in self.it.by_ref() {
            let (key, ids) =
                item.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            let mapped: Vec<u32> = ids
                .into_iter()
                .filter_map(|oid| {
                    let nid = remap[oid as usize];
                    (nid != u32::MAX).then_some(nid)
                })
                .collect();
            if !mapped.is_empty() {
                self.next = Some((key, mapped));
                return Ok(());
            }
        }
        Ok(())
    }
}

/// K-way merge of the in-memory run, spilled runs and the old-index run
/// into the final sorted (key, ids) stream. One reusable ids buffer serves
/// every key.
struct Merged<'a> {
    mem: Vec<u64>,
    mem_pos: usize,
    shards: Vec<ShardCursor>,
    old: Option<OldRun<'a>>,
    ids: Vec<u32>,
}

impl PostingsSource for Merged<'_> {
    fn next(&mut self) -> io::Result<Option<(u32, &[u32])>> {
        // Minimum key across all runs.
        let mut key = u32::MAX;
        let mut any = false;
        if let Some(&p) = self.mem.get(self.mem_pos) {
            key = key.min(pair_key(p));
            any = true;
        }
        for c in &self.shards {
            if let Some(p) = c.next {
                key = key.min(pair_key(p));
                any = true;
            }
        }
        if let Some(o) = &self.old {
            if let Some((k, _)) = &o.next {
                key = key.min(*k);
                any = true;
            }
        }
        if !any {
            return Ok(None);
        }

        // Collect this key's ids from every run. Each run's ids are already
        // ascending; with more than one contributing run the concatenation
        // needs one sort.
        self.ids.clear();
        let mut sources = 0u32;
        if self
            .mem
            .get(self.mem_pos)
            .is_some_and(|&p| pair_key(p) == key)
        {
            sources += 1;
            while let Some(&p) = self.mem.get(self.mem_pos) {
                if pair_key(p) != key {
                    break;
                }
                self.ids.push(p as u32);
                self.mem_pos += 1;
            }
        }
        for c in &mut self.shards {
            if c.next.is_some_and(|p| pair_key(p) == key) {
                sources += 1;
                while let Some(p) = c.next {
                    if pair_key(p) != key {
                        break;
                    }
                    self.ids.push(p as u32);
                    c.advance()?;
                }
            }
        }
        if let Some(o) = &mut self.old {
            if o.next.as_ref().is_some_and(|(k, _)| *k == key) {
                sources += 1;
                let (_, mut ids) = o.next.take().unwrap();
                self.ids.append(&mut ids);
                o.advance()?;
            }
        }
        if sources > 1 {
            self.ids.sort_unstable();
            self.ids.dedup();
        }
        Ok(Some((key, &self.ids)))
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
