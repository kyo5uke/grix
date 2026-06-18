//! Query evaluation + confirming scan.
//!
//! The index narrows the search to candidate files; the scan then runs the
//! real regex over the *current* content of those files. Results therefore
//! never contain stale lines — an out-of-date index can only miss files
//! whose content changed after indexing (see `grix index`).

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use crate::index::format::{IndexReader, FLAG_BINARY, FLAG_SCAN_ALWAYS};
use crate::plan::{self, Query};
use crate::trigram;

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub case_insensitive: bool,
    pub fixed_string: bool,
    /// Stop scanning a file at the first match (-l / -c file listings).
    pub matches_only: bool,
    pub threads: usize,
    /// Restrict to files under these scopes. Each scope is a `/`-separated
    /// path relative to the index root; a file matches a scope when it equals
    /// it (a file argument) or sits under it (a directory argument). Empty =
    /// no restriction.
    pub path_scopes: Vec<String>,
    pub max_count: Option<u64>,
}

/// True when `rel` is in scope: no scopes means everything, otherwise the
/// path must equal one scope (file) or be nested under one (directory).
pub fn in_scope(rel: &str, scopes: &[String]) -> bool {
    if scopes.is_empty() {
        return true;
    }
    scopes.iter().any(|s| {
        rel == s
            || (rel.len() > s.len()
                && rel.as_bytes()[s.len()] == b'/'
                && rel.starts_with(s.as_str()))
    })
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions {
            case_insensitive: false,
            fixed_string: false,
            matches_only: false,
            threads: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
            path_scopes: Vec::new(),
            max_count: None,
        }
    }
}

#[derive(Debug)]
pub struct MatchLine {
    pub line_number: u64,
    /// Byte ranges of matches within `line`.
    pub spans: Vec<(usize, usize)>,
    pub line: Vec<u8>,
}

#[derive(Debug)]
pub struct FileResult {
    pub rel_path: String,
    pub lines: Vec<MatchLine>,
}

#[derive(Debug, Default)]
pub struct SearchStats {
    pub query_display: String,
    pub files_in_index: usize,
    pub candidates: usize,
    pub files_scanned: usize,
    pub files_matched: usize,
    pub lines_matched: usize,
    pub plan_micros: u128,
    pub lookup_micros: u128,
    pub scan_micros: u128,
}

#[derive(Debug)]
pub enum SearchError {
    BadPattern(String),
    Io(std::io::Error),
    Index(crate::index::format::IndexError),
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SearchError::BadPattern(e) => write!(f, "invalid pattern: {e}"),
            SearchError::Io(e) => write!(f, "io error: {e}"),
            SearchError::Index(e) => write!(f, "{e}"),
        }
    }
}

impl From<std::io::Error> for SearchError {
    fn from(e: std::io::Error) -> Self {
        SearchError::Io(e)
    }
}

pub struct Matcher {
    pub regex: regex::bytes::Regex,
    pub query: Query,
}

/// Compile the pattern into both the index query and the confirming regex.
pub fn compile(pattern: &str, opts: &SearchOptions) -> Result<Matcher, SearchError> {
    let pattern_owned;
    let pattern = if opts.fixed_string {
        pattern_owned = regex_syntax::escape(pattern);
        &pattern_owned
    } else {
        pattern
    };
    if pattern.is_empty() {
        return Err(SearchError::BadPattern("empty pattern".into()));
    }
    let query = plan::plan(pattern, opts.case_insensitive)
        .map_err(|e| SearchError::BadPattern(e.to_string()))?;
    let regex = regex::bytes::RegexBuilder::new(pattern)
        .case_insensitive(opts.case_insensitive)
        .multi_line(true)
        .build()
        .map_err(|e| SearchError::BadPattern(e.to_string()))?;
    Ok(Matcher { regex, query })
}

fn intersect(a: Vec<u32>, b: Vec<u32>) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

fn union(a: Vec<u32>, b: Vec<u32>) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if j >= b.len() || (i < a.len() && a[i] < b[j]) {
            out.push(a[i]);
            i += 1;
        } else if i >= a.len() || b[j] < a[i] {
            out.push(b[j]);
            j += 1;
        } else {
            out.push(a[i]);
            i += 1;
            j += 1;
        }
    }
    out
}

/// Evaluate the trigram query into a sorted candidate id list.
fn eval(q: &Query, r: &IndexReader) -> Result<Vec<u32>, SearchError> {
    Ok(match q {
        Query::None => Vec::new(),
        Query::All => (0..r.file_count() as u32).collect(),
        Query::Tri(t) => r.postings(*t).map_err(SearchError::Index)?,
        Query::And(subs) => {
            // Cheapest lists first: evaluate all, sort by length, intersect.
            let mut lists = Vec::with_capacity(subs.len());
            for s in subs {
                lists.push(eval(s, r)?);
            }
            lists.sort_by_key(|l| l.len());
            let mut it = lists.into_iter();
            let mut acc = it.next().unwrap_or_default();
            for l in it {
                if acc.is_empty() {
                    break;
                }
                acc = intersect(acc, l);
            }
            acc
        }
        Query::Or(subs) => {
            let mut acc = Vec::new();
            for s in subs {
                acc = union(acc, eval(s, r)?);
            }
            acc
        }
    })
}

enum FileData {
    Owned(Vec<u8>),
    Mapped(memmap2::Mmap),
}

impl std::ops::Deref for FileData {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            FileData::Owned(v) => v,
            FileData::Mapped(m) => m,
        }
    }
}

fn load(path: &Path, size_hint: u64) -> std::io::Result<FileData> {
    if size_hint > 8 << 20 {
        let f = std::fs::File::open(path)?;
        // Safety: bytes are only read; see note in index/format.rs.
        let m = unsafe { memmap2::Mmap::map(&f)? };
        Ok(FileData::Mapped(m))
    } else {
        Ok(FileData::Owned(std::fs::read(path)?))
    }
}

/// Scan one buffer, collecting matched lines.
pub fn scan_buffer(
    re: &regex::bytes::Regex,
    data: &[u8],
    matches_only: bool,
    max_count: Option<u64>,
) -> Vec<MatchLine> {
    let mut lines: Vec<MatchLine> = Vec::new();
    let mut line_no: u64 = 1;
    let mut counted_to: usize = 0; // newlines counted up to this offset
    let mut line_anchor: usize = 0; // start offset of the line containing counted_to
    let mut cur_line: Option<(usize, usize)> = None; // (start, end) of last line emitted

    for m in re.find_iter(data) {
        // grep/ripgrep search line by line: a match can never span a
        // newline. Enforce the same semantics for output parity (e.g. \s+
        // would otherwise bridge lines).
        if memchr::memchr(b'\n', &data[m.start()..m.end()]).is_some() {
            continue;
        }
        if matches_only {
            // Existence is all the caller needs.
            lines.push(MatchLine {
                line_number: 0,
                spans: Vec::new(),
                line: Vec::new(),
            });
            return lines;
        }
        let start = m.start();
        // Count newlines up to the match, tracking the last one so the line
        // start needs no backwards scan (keeps pathological empty-match
        // patterns linear instead of quadratic).
        for p in memchr::memchr_iter(b'\n', &data[counted_to..start]) {
            line_no += 1;
            line_anchor = counted_to + p + 1;
        }
        counted_to = start;

        let line_start = line_anchor;
        let line_end = memchr::memchr(b'\n', &data[start..]).map_or(data.len(), |p| start + p);

        if cur_line == Some((line_start, line_end)) {
            // Another match on the same line: extend spans.
            let last = lines.last_mut().unwrap();
            let s = m.start().max(line_start) - line_start;
            let e = m.end().min(line_end) - line_start;
            if e > s {
                last.spans.push((s, e));
            }
        } else {
            if let Some(max) = max_count {
                if lines.len() as u64 >= max {
                    break;
                }
            }
            let s = m.start() - line_start;
            let e = m.end().min(line_end) - line_start;
            lines.push(MatchLine {
                line_number: line_no,
                spans: if e > s { vec![(s, e)] } else { Vec::new() },
                line: data[line_start..line_end].to_vec(),
            });
            cur_line = Some((line_start, line_end));
        }
    }
    lines
}

/// Search using an index. Returns per-file results sorted by path.
pub fn search_index(
    reader: &IndexReader,
    root: &Path,
    matcher: &Matcher,
    opts: &SearchOptions,
) -> Result<(Vec<FileResult>, SearchStats), SearchError> {
    let mut stats = SearchStats {
        query_display: matcher.query.display(),
        files_in_index: reader.file_count(),
        ..Default::default()
    };

    let t0 = Instant::now();
    let ids = eval(&matcher.query, reader)?;
    stats.lookup_micros = t0.elapsed().as_micros();

    // Candidates = query hits + always-scan files − binary, path-filtered.
    let mut targets: Vec<(u32, String, u64)> = Vec::with_capacity(ids.len());
    let mut push_target = |id: u32| -> Result<(), SearchError> {
        let meta = reader.file(id).map_err(SearchError::Index)?;
        if meta.flags & FLAG_BINARY != 0 {
            return Ok(());
        }
        if !in_scope(meta.rel_path, &opts.path_scopes) {
            return Ok(());
        }
        targets.push((id, meta.rel_path.to_string(), meta.size));
        Ok(())
    };
    let mut seen_scan_always = Vec::new();
    for id in 0..reader.file_count() as u32 {
        let meta = reader.file(id).map_err(SearchError::Index)?;
        if meta.flags & FLAG_SCAN_ALWAYS != 0 {
            seen_scan_always.push(id);
        }
    }
    for &id in ids.iter() {
        let meta = reader.file(id).map_err(SearchError::Index)?;
        if meta.flags & FLAG_SCAN_ALWAYS != 0 {
            continue; // added below regardless of query
        }
        push_target(id)?;
    }
    for id in seen_scan_always {
        push_target(id)?;
    }
    stats.candidates = targets.len();

    // Parallel confirming scan.
    let t1 = Instant::now();
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<FileResult>> = Mutex::new(Vec::new());
    let scanned = AtomicUsize::new(0);
    let nthreads = opts.threads.max(1).min(targets.len().max(1));
    std::thread::scope(|s| {
        for _ in 0..nthreads {
            s.spawn(|| {
                let mut local: Vec<FileResult> = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((_, rel, size)) = targets.get(i) else {
                        break;
                    };
                    let abs = root.join(rel);
                    let Ok(data) = load(&abs, *size) else {
                        continue; // vanished since indexing
                    };
                    scanned.fetch_add(1, Ordering::Relaxed);
                    if trigram::looks_binary(&data) {
                        continue;
                    }
                    let lines =
                        scan_buffer(&matcher.regex, &data, opts.matches_only, opts.max_count);
                    if !lines.is_empty() {
                        local.push(FileResult {
                            rel_path: rel.clone(),
                            lines,
                        });
                    }
                }
                results.lock().unwrap().extend(local);
            });
        }
    });
    stats.scan_micros = t1.elapsed().as_micros();
    stats.files_scanned = scanned.load(Ordering::Relaxed);

    let mut results = results.into_inner().unwrap();
    results.sort_unstable_by(|a, b| a.rel_path.cmp(&b.rel_path));
    stats.files_matched = results.len();
    stats.lines_matched = results.iter().map(|r| r.lines.len()).sum();
    Ok((results, stats))
}

/// Index-less fallback: walk the tree and scan everything.
pub fn search_walk(
    root: &Path,
    matcher: &Matcher,
    opts: &SearchOptions,
) -> Result<(Vec<FileResult>, SearchStats), SearchError> {
    let mut stats = SearchStats {
        query_display: format!("{} (no index: full scan)", matcher.query.display()),
        ..Default::default()
    };

    let mut targets: Vec<(u32, String, u64)> = Vec::new();
    let walker = ignore::WalkBuilder::new(root).build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let rel_path = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if !in_scope(&rel_path, &opts.path_scopes) {
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        targets.push((0, rel_path, size));
    }
    stats.candidates = targets.len();

    let t1 = Instant::now();
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<FileResult>> = Mutex::new(Vec::new());
    let scanned = AtomicUsize::new(0);
    let nthreads = opts.threads.max(1).min(targets.len().max(1));
    std::thread::scope(|s| {
        for _ in 0..nthreads {
            s.spawn(|| {
                let mut local: Vec<FileResult> = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((_, rel, size)) = targets.get(i) else {
                        break;
                    };
                    let abs = root.join(rel);
                    let Ok(data) = load(&abs, *size) else {
                        continue;
                    };
                    scanned.fetch_add(1, Ordering::Relaxed);
                    if trigram::looks_binary(&data) {
                        continue;
                    }
                    let lines =
                        scan_buffer(&matcher.regex, &data, opts.matches_only, opts.max_count);
                    if !lines.is_empty() {
                        local.push(FileResult {
                            rel_path: rel.clone(),
                            lines,
                        });
                    }
                }
                results.lock().unwrap().extend(local);
            });
        }
    });
    stats.scan_micros = t1.elapsed().as_micros();
    stats.files_scanned = scanned.load(Ordering::Relaxed);

    let mut results = results.into_inner().unwrap();
    results.sort_unstable_by(|a, b| a.rel_path.cmp(&b.rel_path));
    stats.files_matched = results.len();
    stats.lines_matched = results.iter().map(|r| r.lines.len()).sum();
    Ok((results, stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn re(p: &str) -> regex::bytes::Regex {
        regex::bytes::RegexBuilder::new(p)
            .multi_line(true)
            .build()
            .unwrap()
    }

    #[test]
    fn scan_lines_and_spans() {
        let data = b"foo bar\nbaz foo foo\nqux\n";
        let lines = scan_buffer(&re("foo"), data, false, None);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].line_number, 1);
        assert_eq!(lines[0].spans, vec![(0, 3)]);
        assert_eq!(lines[1].line_number, 2);
        assert_eq!(lines[1].spans, vec![(4, 7), (8, 11)]);
        assert_eq!(lines[1].line, b"baz foo foo");
    }

    #[test]
    fn scan_no_trailing_newline() {
        let data = b"alpha\nbeta";
        let lines = scan_buffer(&re("beta"), data, false, None);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].line_number, 2);
        assert_eq!(lines[0].line, b"beta");
    }

    #[test]
    fn scan_anchors() {
        let data = b"x\nabc\nyabc\n";
        let lines = scan_buffer(&re("^abc$"), data, false, None);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].line_number, 2);
    }

    #[test]
    fn matches_never_span_lines() {
        // ripgrep semantics: \s+ must not bridge a newline.
        let data = b"static\nint x;\nstatic int y;\n";
        let lines = scan_buffer(&re(r"static\s+int"), data, false, None);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].line_number, 3);
        // Explicit \n in the pattern can match nothing, ever.
        assert!(scan_buffer(&re("static\\nint"), data, false, None).is_empty());
        assert!(scan_buffer(&re("static\\nint"), data, true, None).is_empty());
    }

    #[test]
    fn scope_matching() {
        let none: Vec<String> = vec![];
        assert!(in_scope("anything/x.rs", &none)); // no scopes => all

        let dir = vec!["src".to_string()];
        assert!(in_scope("src/main.rs", &dir));
        assert!(in_scope("src/a/b.rs", &dir));
        assert!(in_scope("src", &dir)); // exact path (a file named src) still matches
        assert!(!in_scope("src2/x.rs", &dir)); // prefix must end at a '/'
        assert!(!in_scope("srcfile.rs", &dir));
        assert!(!in_scope("tests/x.rs", &dir));

        let file = vec!["deobf/clean/app-core.clean.jsx".to_string()];
        assert!(in_scope("deobf/clean/app-core.clean.jsx", &file));
        assert!(!in_scope("deobf/clean/app-core.clean.jsx.map", &file));
        assert!(!in_scope("deobf/clean", &file));

        let multi = vec!["src".to_string(), "docs/guide.md".to_string()];
        assert!(in_scope("src/x.rs", &multi));
        assert!(in_scope("docs/guide.md", &multi));
        assert!(!in_scope("docs/other.md", &multi));
    }

    #[test]
    fn merge_set_ops() {
        assert_eq!(intersect(vec![1, 3, 5], vec![2, 3, 5, 7]), vec![3, 5]);
        assert_eq!(union(vec![1, 5], vec![2, 5, 9]), vec![1, 2, 5, 9]);
        assert_eq!(intersect(vec![], vec![1]), Vec::<u32>::new());
        assert_eq!(union(vec![], vec![1]), vec![1]);
    }
}
