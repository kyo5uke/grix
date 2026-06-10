//! Index construction: full builds and incremental updates.
//!
//! Build pipeline:
//! 1. Walk the tree (gitignore-aware) and collect candidate files, sorted by
//!    relative path so file ids are deterministic.
//! 2. For an incremental update, files whose (size, mtime) match the old
//!    index are *reused*: their postings are recovered from the old index in
//!    one linear pass (old id -> new id remap) without touching the files.
//! 3. Changed/new files are read and trigram-extracted in parallel.
//! 4. Posting lists are sorted, deduplicated and written atomically.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use super::format::{self, FileRecord, IndexReader, FLAG_BINARY, FLAG_SCAN_ALWAYS};
use crate::trigram;

#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// Files larger than this are not indexed; they are recorded and always
    /// scanned at search time so results stay complete.
    pub max_file_size: u64,
    pub threads: usize,
}

impl Default for BuildOptions {
    fn default() -> Self {
        BuildOptions {
            max_file_size: 16 << 20,
            threads: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
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

/// Read + classify + extract one file.
fn extract_file(abs: &Path) -> io::Result<Extracted> {
    let data = std::fs::read(abs)?;
    if trigram::looks_binary(&data) {
        return Ok(Extracted::Binary);
    }
    Ok(Extracted::Indexed(trigram::extract_sorted(&data)))
}

/// Build (or incrementally rebuild) the index for `root` into `index_path`.
pub fn build(
    root: &Path,
    index_path: &Path,
    old: Option<&IndexReader>,
    opts: &BuildOptions,
) -> io::Result<BuildStats> {
    let mut stats = BuildStats::default();
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

    // Postings, recovered from the old index in one linear pass.
    let mut postings: HashMap<u32, Vec<u32>> = HashMap::new();
    if let Some(old) = old {
        for item in old.iter_postings() {
            let (key, ids) = match item {
                Ok(kv) => kv,
                Err(_) => {
                    // Corrupt old index: fall back to a full rebuild.
                    postings.clear();
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
                postings.insert(key, mapped);
            }
        }
    }

    // Parallel extraction of changed/new files.
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(u32, Extracted, u64)>> =
        Mutex::new(Vec::with_capacity(to_extract.len()));
    let nthreads = opts.threads.max(1).min(to_extract.len().max(1));
    std::thread::scope(|s| {
        for _ in 0..nthreads {
            s.spawn(|| {
                let mut local: Vec<(u32, Extracted, u64)> = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((new_id, rel)) = to_extract.get(i) else {
                        break;
                    };
                    let abs = root.join(rel);
                    match extract_file(&abs) {
                        Ok(ex) => {
                            let bytes = match &ex {
                                Extracted::Indexed(_) => records[*new_id as usize].size,
                                Extracted::Binary => 0,
                            };
                            local.push((*new_id, ex, bytes));
                        }
                        Err(_) => {
                            // File vanished or unreadable: mark binary so it
                            // is excluded from search rather than half-indexed.
                            local.push((*new_id, Extracted::Binary, 0));
                        }
                    }
                }
                results.lock().unwrap().extend(local);
            });
        }
    });

    for (new_id, ex, bytes) in results.into_inner().unwrap() {
        stats.bytes_read += bytes;
        match ex {
            Extracted::Binary => {
                records[new_id as usize].flags = FLAG_BINARY;
            }
            Extracted::Indexed(tris) => {
                for t in tris {
                    postings.entry(t).or_default().push(new_id);
                }
            }
        }
    }
    stats.files_indexed = records.iter().filter(|r| r.flags == 0).count();
    stats.files_binary = records
        .iter()
        .filter(|r| r.flags & FLAG_BINARY != 0)
        .count();
    stats.files_scan_always = records
        .iter()
        .filter(|r| r.flags & FLAG_SCAN_ALWAYS != 0)
        .count();

    // Sort ids per trigram (extraction order is interleaved across threads).
    let mut keys: Vec<u32> = postings.keys().copied().collect();
    keys.sort_unstable();
    let mut ordered: Vec<(u32, Vec<u32>)> = Vec::with_capacity(keys.len());
    for k in keys {
        let mut ids = postings.remove(&k).unwrap();
        ids.sort_unstable();
        ids.dedup();
        ordered.push((k, ids));
    }

    let root_str = root.to_string_lossy().replace('\\', "/");
    format::write_index(index_path, &root_str, &records, ordered.into_iter())?;
    Ok(stats)
}
