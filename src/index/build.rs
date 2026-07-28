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

use super::format::{
    self, FileRecord, IndexReader, PostList, PostingsSource, FLAG_BINARY, FLAG_HIDDEN,
    FLAG_SCAN_ALWAYS, FLAG_UNINDEXED,
};
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
    /// Dotted path component, or the hidden attribute on Windows.
    pub hidden: bool,
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

#[cfg(windows)]
fn attr_hidden(md: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    md.file_attributes() & 0x2 != 0 // FILE_ATTRIBUTE_HIDDEN
}

#[cfg(not(windows))]
fn attr_hidden(_md: &std::fs::Metadata) -> bool {
    false
}

/// Walk `root` in parallel and return indexable candidates sorted by
/// relative path (ids stay deterministic regardless of traversal order).
/// Directory walks are syscall-bound — on Windows especially — so the
/// parallel walker is the single biggest lever for refresh latency.
///
/// Hidden files are *included* and marked, so the index covers them and a
/// search decides (--hidden) without a rebuild. `.git` is always pruned.
/// `no_ignore` drops the gitignore/.ignore rules (rg -u) — used only for
/// walk-scans; the index itself always respects ignore rules.
pub fn collect_candidates(
    root: &Path,
    threads: usize,
    no_ignore: bool,
) -> io::Result<Vec<Candidate>> {
    let (tx, rx) = mpsc::channel::<Candidate>();
    let mut builder = ignore::WalkBuilder::new(root);
    builder.threads(threads.max(1)).hidden(false);
    if no_ignore {
        builder
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .ignore(false)
            .parents(false);
    }
    let walker = builder.build_parallel();
    walker.run(|| {
        let tx = tx.clone();
        let root = root.to_path_buf();
        Box::new(move |entry| {
            // Unreadable entries are skipped, not fatal.
            let Ok(entry) = entry else {
                return ignore::WalkState::Continue;
            };
            let ft = entry.file_type();
            if ft.as_ref().is_some_and(|t| t.is_dir()) {
                // Never descend into .git, hidden or not.
                if entry.file_name() == ".git" {
                    return ignore::WalkState::Skip;
                }
                return ignore::WalkState::Continue;
            }
            if !ft.is_some_and(|t| t.is_file()) {
                return ignore::WalkState::Continue;
            }
            let Ok(md) = entry.metadata() else {
                return ignore::WalkState::Continue;
            };
            let Ok(rel) = entry.path().strip_prefix(&root) else {
                return ignore::WalkState::Continue;
            };
            let dotted = rel
                .components()
                .any(|c| c.as_os_str().to_string_lossy().starts_with('.'));
            let rel_path = rel_string(rel);
            if !rel_path.is_empty() {
                let _ = tx.send(Candidate {
                    rel_path,
                    size: md.len(),
                    mtime: mtime_nanos(&md),
                    hidden: dotted || attr_hidden(&md),
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

/// Where a candidate's current contents already live, if anywhere.
enum Class {
    /// Unchanged since the base was built: base id.
    Base(u32),
    /// Unchanged since the old overlay was written: old overlay id.
    Over(u32),
    /// New or changed: must be read.
    New,
}

fn new_build_id() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // Never 0: 0 means "is a base" in parent_id.
    (t ^ (u64::from(std::process::id()) << 32) ^ SEQ.fetch_add(1, Ordering::Relaxed)) | 1
}

/// Build or refresh the index for `root`.
///
/// With a usable base index, changes accumulate in a small overlay next to
/// it — the refresh cost tracks the churn since the base was built, not the
/// tree size. Past a threshold (or without a base) everything is folded
/// into a fresh base and the overlay is dropped.
pub fn build(root: &Path, index_path: &Path, opts: &BuildOptions) -> io::Result<BuildStats> {
    match build_attempt(root, index_path, opts, true) {
        // A corrupt existing index surfaces as InvalidData while its
        // postings stream through the merge; retry once from scratch.
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            build_attempt(root, index_path, opts, false)
        }
        r => r,
    }
}

fn build_attempt(
    root: &Path,
    index_path: &Path,
    opts: &BuildOptions,
    use_existing: bool,
) -> io::Result<BuildStats> {
    let mut stats = BuildStats::default();
    let overlay_path = format::overlay_path(index_path);
    // Reclaim temps a crashed build may have left next to the index (their
    // names are unique per attempt, so nothing else ever overwrites them).
    format::sweep_stale_temps(index_path);
    format::sweep_stale_temps(&overlay_path);
    let candidates = collect_candidates(root, opts.threads, false)?;
    stats.files_total = candidates.len();

    let base = if use_existing {
        IndexReader::open(index_path).ok()
    } else {
        None
    };
    // An overlay only counts if it belongs to this exact base.
    let old_over = base.as_ref().and_then(|b| {
        IndexReader::open(&overlay_path)
            .ok()
            .filter(|o| o.index_ids().parent_id == b.index_ids().build_id)
    });

    // Prior state by path, newest representation wins at classification.
    let mut base_by_path: HashMap<&str, (u32, u64, u64, u32)> = HashMap::new();
    if let Some(b) = &base {
        for id in 0..b.file_count() as u32 {
            if let Ok(m) = b.file(id) {
                base_by_path.insert(m.rel_path, (id, m.size, m.mtime, m.flags));
            }
        }
    }
    let mut over_by_path: HashMap<&str, (u32, u64, u64, u32)> = HashMap::new();
    if let Some(o) = &old_over {
        for id in 0..o.file_count() as u32 {
            if let Ok(m) = o.file(id) {
                over_by_path.insert(m.rel_path, (id, m.size, m.mtime, m.flags));
            }
        }
    }

    // Classification: where does each candidate's content already live?
    let b_count = base.as_ref().map_or(0, |b| b.file_count());
    let mut class: Vec<Class> = Vec::with_capacity(candidates.len());
    let mut flags: Vec<u32> = Vec::with_capacity(candidates.len());
    let mut base_alive = vec![false; b_count];
    let mut over_kept = 0usize;
    let mut new_files = 0usize;
    for cand in &candidates {
        let too_large = cand.size > opts.max_file_size;
        // Unchanged (size+mtime) entries keep their stored classification
        // (indexed/binary/large/hidden) unless the size cap or hiddenness
        // moved them across a boundary. Base match is preferred so reverted
        // files migrate back out of the overlay.
        if let Some(&(bid, sz, mt, bf)) = base_by_path.get(cand.rel_path.as_str()) {
            if sz == cand.size
                && mt == cand.mtime
                && (bf & FLAG_SCAN_ALWAYS != 0) == too_large
                && (bf & FLAG_HIDDEN != 0) == cand.hidden
            {
                class.push(Class::Base(bid));
                flags.push(bf);
                base_alive[bid as usize] = true;
                continue;
            }
        }
        if let Some(&(oid, sz, mt, of)) = over_by_path.get(cand.rel_path.as_str()) {
            if sz == cand.size
                && mt == cand.mtime
                && (of & FLAG_SCAN_ALWAYS != 0) == too_large
                && (of & FLAG_HIDDEN != 0) == cand.hidden
            {
                class.push(Class::Over(oid));
                flags.push(of);
                over_kept += 1;
                continue;
            }
        }
        class.push(Class::New);
        let mut f = if too_large { FLAG_SCAN_ALWAYS } else { 0 };
        if cand.hidden {
            f |= FLAG_HIDDEN;
        }
        flags.push(f);
        new_files += 1;
    }
    let base_kept = base_alive.iter().filter(|&&a| a).count();
    stats.files_reused = base_kept + over_kept;

    // Every base id without a surviving candidate is superseded: deleted,
    // changed (now represented in the overlay), or reclassified.
    let tombstones: Vec<u32> = (0..b_count as u32)
        .filter(|&id| !base_alive[id as usize])
        .collect();

    // Mode: fold into a fresh base when there is none, or when the overlay
    // would grow past its budget (searches pay for overlay size on every
    // query, and the fold amortizes).
    let over_files = over_kept + new_files;
    let cap = (b_count / 8).max(64);
    let full = base.is_none() || over_files > cap || tombstones.len() > cap;

    if !full {
        let same_tomb = match &old_over {
            Some(o) => o.tombstones().eq(tombstones.iter().copied()),
            None => tombstones.is_empty(),
        };
        let kept_all_over = old_over
            .as_ref()
            .map_or(over_kept == 0, |o| over_kept == o.file_count());
        if new_files == 0 && kept_all_over && same_tomb {
            // Nothing changed relative to base+overlay: write nothing.
            fill_flag_stats(&mut stats, &flags);
            return Ok(stats);
        }
        if over_files == 0 && tombstones.is_empty() {
            // Everything reverted to the base: the overlay is obsolete.
            let _ = std::fs::remove_file(&overlay_path);
            fill_flag_stats(&mut stats, &flags);
            return Ok(stats);
        }
    }

    // Materialize the write set for the chosen mode. `rec2cand` maps record
    // ids back to candidate indexes so late binary discoveries update the
    // stats flags too.
    let mut records: Vec<FileRecord> = Vec::new();
    let mut rec2cand: Vec<usize> = Vec::new();
    let mut to_extract: Vec<(u32, String, u64)> = Vec::new();
    let mut remap_base: Vec<u32> = Vec::new();
    let mut remap_over: Vec<u32> = vec![u32::MAX; old_over.as_ref().map_or(0, |o| o.file_count())];
    let (target_path, tombs_out, out_ids);
    if full {
        remap_base = vec![u32::MAX; b_count];
        for (i, cand) in candidates.iter().enumerate() {
            let id = records.len() as u32;
            match class[i] {
                Class::Base(bid) => {
                    if flags[i] & FLAG_UNINDEXED == 0 {
                        remap_base[bid as usize] = id;
                    }
                }
                Class::Over(oid) => {
                    if flags[i] & FLAG_UNINDEXED == 0 {
                        remap_over[oid as usize] = id;
                    }
                }
                Class::New => {
                    if flags[i] & FLAG_SCAN_ALWAYS == 0 {
                        to_extract.push((id, cand.rel_path.clone(), cand.size));
                    }
                }
            }
            records.push(FileRecord {
                rel_path: cand.rel_path.clone(),
                size: cand.size,
                mtime: cand.mtime,
                flags: flags[i],
            });
            rec2cand.push(i);
        }
        target_path = index_path;
        tombs_out = Vec::new();
        out_ids = format::IndexIds::base(new_build_id());
    } else {
        for (i, cand) in candidates.iter().enumerate() {
            match class[i] {
                Class::Base(_) => continue,
                Class::Over(oid) => {
                    if flags[i] & FLAG_UNINDEXED == 0 {
                        remap_over[oid as usize] = records.len() as u32;
                    }
                }
                Class::New => {
                    if flags[i] & FLAG_SCAN_ALWAYS == 0 {
                        to_extract.push((records.len() as u32, cand.rel_path.clone(), cand.size));
                    }
                }
            }
            records.push(FileRecord {
                rel_path: cand.rel_path.clone(),
                size: cand.size,
                mtime: cand.mtime,
                flags: flags[i],
            });
            rec2cand.push(i);
        }
        target_path = &overlay_path;
        tombs_out = tombstones;
        out_ids = format::IndexIds {
            build_id: new_build_id(),
            parent_id: base.as_ref().map_or(0, |b| b.index_ids().build_id),
        };
    }
    stats.files_extracted = to_extract.len();

    // Parallel extraction of changed/new files. Workers emit batched pair
    // messages; this thread accumulates them under the spill budget. The
    // bounded channel provides backpressure while a spill is in progress.
    let mut acc = PairAcc::new(target_path, opts.spill_budget);
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
                    if msg.pairs.len() >= FLUSH_PAIRS && tx.send(std::mem::take(&mut msg)).is_err()
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
                records[id as usize].flags |= FLAG_BINARY;
                flags[rec2cand[id as usize]] |= FLAG_BINARY;
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

    fill_flag_stats(&mut stats, &flags);

    // Merge the in-memory run, spilled runs and the reused-index runs into
    // one sorted (key, list) stream. The guard deletes shard files after
    // the write completes.
    let mut old_runs: Vec<OldRun> = Vec::new();
    if full {
        if let Some(b) = &base {
            old_runs.push(OldRun::new(b, &remap_base)?);
        }
    }
    if let Some(o) = &old_over {
        old_runs.push(OldRun::new(o, &remap_over)?);
    }
    let (merged, _guard) = acc.into_source(old_runs)?;
    let root_str = root.to_string_lossy().replace('\\', "/");
    format::write_index(
        target_path,
        &root_str,
        &records,
        merged,
        &tombs_out,
        out_ids,
    )?;
    if full {
        // Any overlay now describes a dead base.
        let _ = std::fs::remove_file(&overlay_path);
    }
    Ok(stats)
}

fn fill_flag_stats(stats: &mut BuildStats, flags: &[u32]) {
    stats.files_indexed = flags.iter().filter(|&&f| f & FLAG_UNINDEXED == 0).count();
    stats.files_binary = flags.iter().filter(|&&f| f & FLAG_BINARY != 0).count();
    stats.files_scan_always = flags.iter().filter(|&&f| f & FLAG_SCAN_ALWAYS != 0).count();
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

    /// Consume into the merged (key, list) stream plus a guard that deletes
    /// the shard files once the stream has been consumed and dropped.
    fn into_source(mut self, old: Vec<OldRun<'_>>) -> io::Result<(Merged<'_>, ShardGuard)> {
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

type OldPostings<'a> =
    Box<dyn Iterator<Item = Result<(u32, format::Postings), format::IndexError>> + 'a>;

/// One reused group from an existing index.
enum OldGroup {
    /// Remapped, non-empty, ascending ids.
    Ids(Vec<u32>),
    /// Dense trigram: the ids were never stored; passes through as dense.
    Dense(u64),
}

/// An existing index streamed as a pre-sorted run: keys ascending, ids
/// remapped to new ids (survivors keep their relative order, so lists stay
/// sorted). Decode errors surface as `InvalidData`, which `build` turns
/// into a from-scratch rebuild.
struct OldRun<'a> {
    it: OldPostings<'a>,
    remap: &'a [u32],
    next: Option<(u32, OldGroup)>,
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
            let (key, list) =
                item.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            match list {
                format::Postings::Dense(df) => {
                    self.next = Some((key, OldGroup::Dense(df)));
                    return Ok(());
                }
                format::Postings::Ids(ids) => {
                    let mapped: Vec<u32> = ids
                        .into_iter()
                        .filter_map(|oid| {
                            let nid = remap[oid as usize];
                            (nid != u32::MAX).then_some(nid)
                        })
                        .collect();
                    if !mapped.is_empty() {
                        self.next = Some((key, OldGroup::Ids(mapped)));
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }
}

/// K-way merge of the in-memory run, spilled runs and the reused-index
/// runs into the final sorted (key, list) stream. One reusable ids buffer
/// serves every key. A dense group swallows the key: the merged entry
/// stays dense (its ids are unknowable), which only weakens the constraint.
struct Merged<'a> {
    mem: Vec<u64>,
    mem_pos: usize,
    shards: Vec<ShardCursor>,
    old: Vec<OldRun<'a>>,
    ids: Vec<u32>,
}

impl PostingsSource for Merged<'_> {
    fn next(&mut self) -> io::Result<Option<(u32, PostList<'_>)>> {
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
        for o in &self.old {
            if let Some((k, _)) = &o.next {
                key = key.min(*k);
                any = true;
            }
        }
        if !any {
            return Ok(None);
        }

        // Consume this key's contribution from every run (all cursors must
        // advance past it even when the result ends up dense). Each run's
        // ids are already ascending; with more than one contributing run
        // the concatenation needs one sort.
        self.ids.clear();
        let mut sources = 0u32;
        let mut dense: Option<u64> = None;
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
        for o in &mut self.old {
            if o.next.as_ref().is_some_and(|(k, _)| *k == key) {
                let (_, group) = o.next.take().unwrap();
                match group {
                    OldGroup::Ids(mut ids) => {
                        sources += 1;
                        self.ids.append(&mut ids);
                    }
                    OldGroup::Dense(df) => {
                        dense = Some(dense.map_or(df, |d: u64| d.max(df)));
                    }
                }
                o.advance()?;
            }
        }
        if let Some(df) = dense {
            return Ok(Some((key, PostList::Dense(df))));
        }
        if sources > 1 {
            self.ids.sort_unstable();
            self.ids.dedup();
        }
        Ok(Some((key, PostList::Ids(&self.ids))))
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

    /// Index bytes with the random build/parent ids masked out, so two
    /// logically identical builds compare equal.
    fn read_masked(p: &Path) -> Vec<u8> {
        let mut v = std::fs::read(p).unwrap();
        v[128..144].fill(0);
        v
    }

    #[test]
    fn spilled_build_is_byte_identical_to_in_memory() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("tree");
        std::fs::create_dir_all(&root).unwrap();
        write_tree(&root);
        let idx_mem = t.path().join("mem.gix");
        let idx_spill = t.path().join("spill.gix");

        let s1 = build(&root, &idx_mem, &opts(usize::MAX)).unwrap();
        let s2 = build(&root, &idx_spill, &opts(2048)).unwrap();

        assert_eq!(s1.files_indexed, s2.files_indexed);
        assert!(s1.files_indexed >= 25);
        assert_eq!(s1.files_binary, 1);
        assert_eq!(s1.files_scan_always, 1);
        assert_eq!(
            read_masked(&idx_mem),
            read_masked(&idx_spill),
            "spilled build must produce the identical index"
        );

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
    fn unchanged_tree_writes_nothing_changes_go_to_overlay() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("tree");
        std::fs::create_dir_all(&root).unwrap();
        write_tree(&root);
        let idx = t.path().join("noop.gix");
        let over = format::overlay_path(&idx);
        build(&root, &idx, &opts(usize::MAX)).unwrap();

        let base_mtime = std::fs::metadata(&idx).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));

        // No-op refresh: neither base nor overlay appears/changes.
        let stats = build(&root, &idx, &opts(usize::MAX)).unwrap();
        assert_eq!(stats.files_extracted, 0);
        assert_eq!(stats.files_reused, stats.files_total);
        assert_eq!(
            std::fs::metadata(&idx).unwrap().modified().unwrap(),
            base_mtime
        );
        assert!(!over.exists(), "no-op refresh must not create an overlay");

        // A change lands in the overlay; the base is untouched.
        std::fs::write(root.join("f00.txt"), "changed body needle_alpha\n").unwrap();
        let stats = build(&root, &idx, &opts(usize::MAX)).unwrap();
        assert_eq!(stats.files_extracted, 1);
        assert_eq!(
            std::fs::metadata(&idx).unwrap().modified().unwrap(),
            base_mtime,
            "a small change must not rewrite the base index"
        );
        let o = IndexReader::open(&over).unwrap();
        assert_eq!(o.file_count(), 1);
        assert_eq!(o.file(0).unwrap().rel_path, "f00.txt");
        assert_eq!(o.tombstones().count(), 1); // the old f00 in the base
        let base = IndexReader::open(&idx).unwrap();
        assert_eq!(o.index_ids().parent_id, base.index_ids().build_id);

        // No-op refresh on top of an overlay: overlay not rewritten either.
        let over_mtime = std::fs::metadata(&over).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        let stats = build(&root, &idx, &opts(usize::MAX)).unwrap();
        assert_eq!(stats.files_extracted, 0);
        assert_eq!(
            std::fs::metadata(&over).unwrap().modified().unwrap(),
            over_mtime
        );
    }

    #[test]
    fn overlay_tracks_delete_add_and_shrinks() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("tree");
        std::fs::create_dir_all(&root).unwrap();
        write_tree(&root);
        let idx = t.path().join("od.gix");
        let over = format::overlay_path(&idx);
        build(&root, &idx, &opts(usize::MAX)).unwrap();

        std::fs::remove_file(root.join("f01.txt")).unwrap();
        std::fs::write(root.join("f02.txt"), "different needle_alpha body\n").unwrap();
        std::fs::write(root.join("added.txt"), "brand new needle_alpha\n").unwrap();
        build(&root, &idx, &opts(usize::MAX)).unwrap();

        let o = IndexReader::open(&over).unwrap();
        // f02 (changed) + added.txt live in the overlay; f01 + f02 are
        // superseded base ids.
        assert_eq!(o.file_count(), 2);
        assert_eq!(o.tombstones().count(), 2);

        // Dropping the added file shrinks the overlay on the next refresh.
        std::fs::remove_file(root.join("added.txt")).unwrap();
        build(&root, &idx, &opts(usize::MAX)).unwrap();
        let o = IndexReader::open(&over).unwrap();
        assert_eq!(o.file_count(), 1);
        assert_eq!(o.tombstones().count(), 2);
    }

    #[test]
    fn compaction_folds_overlay_and_matches_fresh() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("tree");
        std::fs::create_dir_all(&root).unwrap();
        // Enough files that changing most of them crosses the overlay cap
        // (max(64, files/8)).
        for i in 0..90 {
            std::fs::write(
                root.join(format!("m{i:03}.txt")),
                format!("file {i} needle_alpha unique_token_{i:04}\n"),
            )
            .unwrap();
        }
        let idx = t.path().join("cf.gix");
        let over = format::overlay_path(&idx);
        build(&root, &idx, &opts(usize::MAX)).unwrap();
        let base_mtime = std::fs::metadata(&idx).unwrap().modified().unwrap();

        // Small change first: overlay route.
        std::fs::write(root.join("m000.txt"), "changed needle_alpha zero\n").unwrap();
        build(&root, &idx, &opts(usize::MAX)).unwrap();
        assert!(over.exists());

        // Mass change: crosses the cap, folds into a fresh base.
        std::thread::sleep(std::time::Duration::from_millis(30));
        for i in 0..70 {
            std::fs::write(
                root.join(format!("m{i:03}.txt")),
                format!("rewritten {i} needle_alpha beta_token_{i:04}\n"),
            )
            .unwrap();
        }
        let stats = build(&root, &idx, &opts(2048)).unwrap();
        assert!(stats.files_reused > 0, "unchanged files still reused");
        assert!(
            std::fs::metadata(&idx).unwrap().modified().unwrap() > base_mtime,
            "compaction must rewrite the base"
        );
        assert!(!over.exists(), "compaction must drop the overlay");

        // The folded base equals a from-scratch build byte for byte.
        let idx_fresh = t.path().join("cf-fresh.gix");
        build(&root, &idx_fresh, &opts(usize::MAX)).unwrap();
        assert_eq!(
            read_masked(&idx),
            read_masked(&idx_fresh),
            "compacted rebuild must equal a fresh build"
        );
    }
}
