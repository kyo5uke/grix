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

use crate::index::format::{
    FileMeta, IndexError, IndexReader, Postings, FLAG_BINARY, FLAG_SCAN_ALWAYS,
};
use crate::plan::{self, Query};
use crate::trigram;

/// Search-time composition of a base index and its (optional) overlay:
/// base ids stay `[0, B)`, overlay ids live at `[B, B+O)`, and base ids the
/// overlay superseded (tombstones) are dead. All the merge logic lives
/// here; the readers stay dumb mmaps.
pub struct View<'a> {
    base: &'a IndexReader,
    overlay: Option<&'a IndexReader>,
    /// Bitset over base ids: superseded by the overlay.
    dead: Vec<u64>,
    alive_base: usize,
}

/// A candidate id set during query evaluation. `All` (every alive file)
/// propagates lazily so dense trigrams and `.*`-ish queries never
/// materialize 90k-element vectors mid-expression.
enum IdSet {
    All,
    Ids(Vec<u32>),
}

impl<'a> View<'a> {
    pub fn new(base: &'a IndexReader, overlay: Option<&'a IndexReader>) -> Self {
        // Callers validate the pairing; be defensive anyway.
        let overlay = overlay.filter(|o| o.index_ids().parent_id == base.index_ids().build_id);
        let mut dead = vec![0u64; base.file_count().div_ceil(64)];
        let mut dead_count = 0usize;
        if let Some(o) = overlay {
            for id in o.tombstones() {
                let id = id as usize;
                if id < base.file_count() {
                    let (w, m) = (id / 64, 1u64 << (id % 64));
                    if dead[w] & m == 0 {
                        dead[w] |= m;
                        dead_count += 1;
                    }
                }
            }
        }
        View {
            base,
            overlay,
            dead,
            alive_base: base.file_count() - dead_count,
        }
    }

    #[inline]
    fn base_count(&self) -> u32 {
        self.base.file_count() as u32
    }

    #[inline]
    fn is_dead(&self, base_id: u32) -> bool {
        self.dead[(base_id / 64) as usize] & (1u64 << (base_id % 64)) != 0
    }

    /// Alive files across base and overlay.
    pub fn file_count(&self) -> usize {
        self.alive_base + self.overlay.map_or(0, |o| o.file_count())
    }

    pub fn file(&self, id: u32) -> Result<FileMeta<'_>, IndexError> {
        let b = self.base_count();
        if id < b {
            self.base.file(id)
        } else {
            self.overlay
                .ok_or(IndexError::Corrupt("overlay id without overlay"))?
                .file(id - b)
        }
    }

    /// Candidate ids for one trigram, in ascending order (base then
    /// offset overlay ids — already sorted by construction).
    fn postings(&self, t: u32) -> Result<IdSet, IndexError> {
        let base_ids = match self.base.postings(t)? {
            Postings::Dense(_) => return Ok(IdSet::All),
            Postings::Ids(v) => v,
        };
        let mut ids: Vec<u32> = base_ids.into_iter().filter(|&i| !self.is_dead(i)).collect();
        if let Some(o) = self.overlay {
            match o.postings(t)? {
                Postings::Dense(_) => return Ok(IdSet::All),
                Postings::Ids(ov) => {
                    let off = self.base_count();
                    ids.extend(ov.into_iter().map(|i| i + off));
                }
            }
        }
        Ok(IdSet::Ids(ids))
    }

    /// Every alive id (the top-level `ALL` materialization).
    fn all_ids(&self) -> Vec<u32> {
        let b = self.base_count();
        let mut v: Vec<u32> = (0..b).filter(|&i| !self.is_dead(i)).collect();
        if let Some(o) = self.overlay {
            v.extend((0..o.file_count() as u32).map(|i| i + b));
        }
        v
    }

    /// Alive scan-always ids across base and overlay.
    fn scan_always(&self) -> Vec<u32> {
        let b = self.base_count();
        let mut v: Vec<u32> = self
            .base
            .scan_always_ids()
            .filter(|&i| !self.is_dead(i))
            .collect();
        if let Some(o) = self.overlay {
            v.extend(o.scan_always_ids().map(|i| i + b));
        }
        v
    }
}

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
    /// Context lines to show before / after each matching line (-B / -A).
    pub before: usize,
    pub after: usize,
    /// Matches may span line boundaries (-U). Output then covers every line
    /// the match touches, and -c / -m count matches instead of lines.
    pub multiline: bool,
    /// Display-time replacement for matched text (-r), with `$group` refs.
    /// Files are never modified.
    pub replace: Option<Vec<u8>>,
    /// Only count matches with non-word (or edge) neighbours (-w). Like
    /// ripgrep this is a *surrounded-by* test, not `\b` rewriting: `-w '::'`
    /// matches in `x :: y` but not in `a::b`.
    pub word: bool,
    /// Print lines that do NOT match (-v). The index cannot narrow this —
    /// a file with zero candidate trigrams matches everywhere — so every
    /// file is scanned.
    pub invert: bool,
    /// Print each match on its own line instead of the whole line (-o).
    pub only_matching: bool,
    /// Case-insensitive iff the pattern has no uppercase literal (-S).
    /// An explicit `case_insensitive` wins.
    pub smart_case: bool,
    /// Glob filters (-g). A leading `!` excludes; otherwise, the presence of
    /// any positive glob restricts results to files that match one.
    pub globs: Vec<String>,
    /// File types to include (-t) and exclude (-T), by name (e.g. "rust").
    pub types_select: Vec<String>,
    pub types_negate: Vec<String>,
}

/// Filename-based filtering (-g / -t / -T), built from `SearchOptions` once
/// per search. Reuses the same glob and type machinery as ripgrep.
pub struct FileFilter {
    overrides: Option<ignore::overrides::Override>,
    types: Option<ignore::types::Types>,
}

impl FileFilter {
    pub fn build(opts: &SearchOptions) -> Result<Self, SearchError> {
        let overrides = if opts.globs.is_empty() {
            None
        } else {
            let mut b = ignore::overrides::OverrideBuilder::new(".");
            for g in &opts.globs {
                b.add(g)
                    .map_err(|e| SearchError::BadPattern(format!("bad glob {g:?}: {e}")))?;
            }
            Some(
                b.build()
                    .map_err(|e| SearchError::BadPattern(format!("bad glob: {e}")))?,
            )
        };
        let types = if opts.types_select.is_empty() && opts.types_negate.is_empty() {
            None
        } else {
            let mut b = ignore::types::TypesBuilder::new();
            b.add_defaults();
            for t in &opts.types_select {
                b.select(t);
            }
            for t in &opts.types_negate {
                b.negate(t);
            }
            Some(
                b.build()
                    .map_err(|e| SearchError::BadPattern(format!("unknown file type: {e}")))?,
            )
        };
        Ok(FileFilter { overrides, types })
    }

    /// True when a file at `rel` (a `/`-separated relative path) passes the
    /// glob and type filters.
    pub fn accept(&self, rel: &str) -> bool {
        if let Some(ov) = &self.overrides {
            if ov.matched(rel, false).is_ignore() {
                return false;
            }
        }
        if let Some(ty) = &self.types {
            if ty.matched(rel, false).is_ignore() {
                return false;
            }
        }
        true
    }
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
            before: 0,
            after: 0,
            multiline: false,
            replace: None,
            word: false,
            invert: false,
            only_matching: false,
            smart_case: false,
            globs: Vec::new(),
            types_select: Vec::new(),
            types_negate: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct MatchLine {
    pub line_number: u64,
    /// Byte ranges of matches within `line` (empty for context lines).
    pub spans: Vec<(usize, usize)>,
    pub line: Vec<u8>,
    /// True for a line the pattern actually matched, false for a context line
    /// pulled in by -A/-B/-C.
    pub is_match: bool,
    /// Number of matches *starting* on this line. Continuation lines of a
    /// multiline match and context lines carry 0. (`-c` counts matched
    /// lines normally but whole matches under `-U`, like ripgrep.)
    pub starts: u32,
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

/// True when the pattern contains an uppercase *literal* (the -S test).
/// Class names like `\S` or `\W` do not count, matching ripgrep.
pub fn pattern_has_uppercase(pattern: &str, fixed: bool) -> bool {
    if fixed {
        return pattern.chars().any(char::is_uppercase);
    }
    use regex_syntax::ast::{self, Ast};
    struct Upper {
        found: bool,
    }
    impl ast::Visitor for Upper {
        type Output = bool;
        type Err = ();
        fn finish(self) -> Result<bool, ()> {
            Ok(self.found)
        }
        fn visit_pre(&mut self, a: &Ast) -> Result<(), ()> {
            if let Ast::Literal(l) = a {
                if l.c.is_uppercase() {
                    self.found = true;
                }
            }
            Ok(())
        }
        fn visit_class_set_item_pre(&mut self, item: &ast::ClassSetItem) -> Result<(), ()> {
            match item {
                ast::ClassSetItem::Literal(l) if l.c.is_uppercase() => self.found = true,
                ast::ClassSetItem::Range(r)
                    if r.start.c.is_uppercase() || r.end.c.is_uppercase() =>
                {
                    self.found = true
                }
                _ => {}
            }
            Ok(())
        }
    }
    match ast::parse::Parser::new().parse(pattern) {
        Ok(a) => ast::visit(&a, Upper { found: false }).unwrap_or(true),
        // Invalid pattern: stay case-sensitive; compile reports the error.
        Err(_) => true,
    }
}

/// Compile the pattern into both the index query and the confirming regex.
pub fn compile(pattern: &str, opts: &SearchOptions) -> Result<Matcher, SearchError> {
    let case_insensitive = opts.case_insensitive
        || (opts.smart_case && !pattern_has_uppercase(pattern, opts.fixed_string));
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
    let query = plan::plan(pattern, case_insensitive)
        .map_err(|e| SearchError::BadPattern(e.to_string()))?;
    let regex = regex::bytes::RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
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

/// Evaluate the trigram query into a candidate id set. `All` propagates
/// lazily: a dense trigram is a constraint that constrains nothing, so in
/// an AND it simply drops out and in an OR it absorbs the whole branch.
fn eval(q: &Query, v: &View) -> Result<IdSet, SearchError> {
    Ok(match q {
        Query::None => IdSet::Ids(Vec::new()),
        Query::All => IdSet::All,
        Query::Tri(t) => v.postings(*t).map_err(SearchError::Index)?,
        Query::And(subs) => {
            // Cheapest lists first: evaluate all, sort by length, intersect.
            let mut lists = Vec::with_capacity(subs.len());
            for s in subs {
                match eval(s, v)? {
                    IdSet::All => {} // X ∧ ALL = X
                    IdSet::Ids(l) => lists.push(l),
                }
            }
            if lists.is_empty() {
                return Ok(IdSet::All);
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
            IdSet::Ids(acc)
        }
        Query::Or(subs) => {
            let mut acc = Vec::new();
            for s in subs {
                match eval(s, v)? {
                    IdSet::All => return Ok(IdSet::All), // X ∨ ALL = ALL
                    IdSet::Ids(l) => acc = union(acc, l),
                }
            }
            IdSet::Ids(acc)
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
    let mut f = crate::index::format::open_sequential(path)?;
    if size_hint > 8 << 20 {
        // Safety: bytes are only read; see note in index/format.rs.
        let m = unsafe { memmap2::Mmap::map(&f)? };
        Ok(FileData::Mapped(m))
    } else {
        // The index already knows the size: pre-size the buffer and skip
        // the extra stat that std::fs::read would do.
        use std::io::Read;
        let mut v = Vec::with_capacity(size_hint as usize + 16);
        f.read_to_end(&mut v)?;
        Ok(FileData::Owned(v))
    }
}

/// Per-scan options: the scanner modes share one signature.
#[derive(Default)]
pub struct ScanOpts<'a> {
    pub matches_only: bool,
    pub max_count: Option<u64>,
    pub before: usize,
    pub after: usize,
    pub multiline: bool,
    pub replace: Option<&'a [u8]>,
    pub word: bool,
    pub invert: bool,
    pub only: bool,
}

#[inline]
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Decode the character ending at byte `i` (raw byte when not UTF-8).
fn char_before(data: &[u8], i: usize) -> Option<char> {
    for k in 1..=4usize.min(i) {
        if let Ok(s) = std::str::from_utf8(&data[i - k..i]) {
            return s.chars().next();
        }
    }
    if i > 0 {
        Some(data[i - 1] as char)
    } else {
        None
    }
}

/// Decode the character starting at byte `i` (raw byte when not UTF-8).
fn char_after(data: &[u8], i: usize) -> Option<char> {
    if i >= data.len() {
        return None;
    }
    for k in 1..=4usize.min(data.len() - i) {
        if let Ok(s) = std::str::from_utf8(&data[i..i + k]) {
            return s.chars().next();
        }
    }
    Some(data[i] as char)
}

/// rg's -w test: the match is surrounded by non-word characters or edges.
fn word_bounded(data: &[u8], s: usize, e: usize) -> bool {
    !char_before(data, s).is_some_and(is_word_char)
        && !char_after(data, e).is_some_and(is_word_char)
}

/// Match enumeration shared by every scan mode. With `word`, a match whose
/// neighbour is a word character is rejected and the search resumes one
/// byte after the *start* of the rejected match — not after its end — so an
/// overlapping bounded match further in is still found (e.g. `a:a` in
/// `za:a:a` matches at the second position like it does for ripgrep).
struct MatchIter<'r, 'd> {
    re: &'r regex::bytes::Regex,
    data: &'d [u8],
    pos: usize,
    word: bool,
}

impl<'r, 'd> MatchIter<'r, 'd> {
    fn new(re: &'r regex::bytes::Regex, data: &'d [u8], word: bool) -> Self {
        MatchIter {
            re,
            data,
            pos: 0,
            word,
        }
    }

    fn next_span(&mut self) -> Option<(usize, usize)> {
        while self.pos <= self.data.len() {
            let m = self.re.find_at(self.data, self.pos)?;
            if self.word && !word_bounded(self.data, m.start(), m.end()) {
                self.pos = m.start() + 1;
                continue;
            }
            self.pos = if m.end() > m.start() {
                m.end()
            } else {
                m.end() + 1
            };
            return Some((m.start(), m.end()));
        }
        None
    }

    fn next_caps(&mut self) -> Option<regex::bytes::Captures<'d>> {
        while self.pos <= self.data.len() {
            let caps = self.re.captures_at(self.data, self.pos)?;
            let m = caps.get(0).unwrap();
            let (ms, me) = (m.start(), m.end());
            if self.word && !word_bounded(self.data, ms, me) {
                self.pos = ms + 1;
                continue;
            }
            self.pos = if me > ms { me } else { me + 1 };
            return Some(caps);
        }
        None
    }
}

/// Scan one buffer, collecting matched lines.
pub fn scan_buffer(
    re: &regex::bytes::Regex,
    data: &[u8],
    matches_only: bool,
    max_count: Option<u64>,
) -> Vec<MatchLine> {
    scan_buffer_opts(
        re,
        data,
        &ScanOpts {
            matches_only,
            max_count,
            ..Default::default()
        },
    )
}

/// Scan one buffer, collecting matched lines plus `before`/`after` context.
pub fn scan_buffer_ctx(
    re: &regex::bytes::Regex,
    data: &[u8],
    matches_only: bool,
    max_count: Option<u64>,
    before: usize,
    after: usize,
) -> Vec<MatchLine> {
    scan_buffer_opts(
        re,
        data,
        &ScanOpts {
            matches_only,
            max_count,
            before,
            after,
            ..Default::default()
        },
    )
}

/// Full scanner entry point: dispatches to the inverted, line-by-line,
/// replacing or multiline scan, applies -o, then expands -A/-B/-C context.
pub fn scan_buffer_opts(re: &regex::bytes::Regex, data: &[u8], o: &ScanOpts) -> Vec<MatchLine> {
    let lines = if o.invert {
        scan_invert(re, data, o)
    } else if o.multiline {
        scan_multiline(re, data, o)
    } else if o.replace.is_some() {
        scan_single_replace(re, data, o)
    } else {
        scan_single(re, data, o)
    };
    let lines = if o.only && !o.invert {
        explode_only_matching(lines)
    } else {
        lines
    };
    if o.only || (o.before == 0 && o.after == 0) || lines.is_empty() {
        return lines;
    }
    expand_context(data, lines, o.before, o.after)
}

/// -o: each span becomes its own output row (one line per match, like rg).
/// `starts` rides on the first row of each source line so -c still counts
/// matches correctly.
fn explode_only_matching(lines: Vec<MatchLine>) -> Vec<MatchLine> {
    let mut out = Vec::new();
    for l in lines {
        let mut starts = l.starts;
        for &(s, e) in &l.spans {
            out.push(MatchLine {
                line_number: l.line_number,
                spans: vec![(0, e - s)],
                line: l.line[s..e].to_vec(),
                is_match: true,
                starts,
            });
            starts = 0;
        }
    }
    out
}

/// -v: emit the lines that do NOT match. With -U, the lines not touched by
/// any (word-checked) match; otherwise a per-line test. The index cannot
/// narrow inverted searches, so callers hand us every file.
fn scan_invert(re: &regex::bytes::Regex, data: &[u8], o: &ScanOpts) -> Vec<MatchLine> {
    // For -U, precollect matched line ranges over the whole buffer.
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    if o.multiline {
        let mut line_no = 1u64;
        let mut counted_to = 0usize;
        let mut it = MatchIter::new(re, data, o.word);
        while let Some((ms, me)) = it.next_span() {
            for _ in memchr::memchr_iter(b'\n', &data[counted_to..ms]) {
                line_no += 1;
            }
            counted_to = ms;
            let last = if me > ms { me - 1 } else { ms };
            let nl = memchr::memchr_iter(b'\n', &data[ms..last.min(data.len())]).count() as u64;
            let (lo, hi) = (line_no, line_no + nl);
            match ranges.last_mut() {
                // Merge only true overlaps: a clean line *between* two
                // matches is unmatched and must stay eligible.
                Some(r) if lo <= r.1 => r.1 = r.1.max(hi),
                _ => ranges.push((lo, hi)),
            }
        }
    }

    let mut out: Vec<MatchLine> = Vec::new();
    let mut start = 0usize;
    let mut line_no = 1u64;
    let mut ri = 0usize;
    while start < data.len() {
        let le = memchr::memchr(b'\n', &data[start..]).map_or(data.len(), |p| start + p);
        let matched = if o.multiline {
            while ri < ranges.len() && ranges[ri].1 < line_no {
                ri += 1;
            }
            ri < ranges.len() && ranges[ri].0 <= line_no
        } else {
            let slice = &data[start..le];
            if o.word {
                MatchIter::new(re, slice, true).next_span().is_some()
            } else {
                re.is_match(slice)
            }
        };
        if !matched {
            if o.matches_only {
                return existence();
            }
            if let Some(max) = o.max_count {
                if out.len() as u64 >= max {
                    break;
                }
            }
            out.push(MatchLine {
                line_number: line_no,
                spans: Vec::new(),
                line: data[start..le].to_vec(),
                is_match: true,
                starts: 1,
            });
        }
        if le == data.len() {
            break;
        }
        start = le + 1;
        line_no += 1;
    }
    out
}

/// The one MatchLine that says "this file matches" (-l / recon calls).
fn existence() -> Vec<MatchLine> {
    vec![MatchLine {
        line_number: 0,
        spans: Vec::new(),
        line: Vec::new(),
        is_match: true,
        starts: 1,
    }]
}

fn scan_single(re: &regex::bytes::Regex, data: &[u8], o: &ScanOpts) -> Vec<MatchLine> {
    let mut lines: Vec<MatchLine> = Vec::new();
    let mut line_no: u64 = 1;
    let mut counted_to: usize = 0; // newlines counted up to this offset
    let mut line_anchor: usize = 0; // start offset of the line containing counted_to
    let mut cur_line: Option<(usize, usize)> = None; // (start, end) of last line emitted

    let mut it = MatchIter::new(re, data, o.word);
    while let Some((mstart, mend)) = it.next_span() {
        // grep/ripgrep search line by line: a match can never span a
        // newline. Enforce the same semantics for output parity (e.g. \s+
        // would otherwise bridge lines).
        if memchr::memchr(b'\n', &data[mstart..mend]).is_some() {
            continue;
        }
        if o.matches_only {
            return existence();
        }
        let start = mstart;
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
            let s = mstart.max(line_start) - line_start;
            let e = mend.min(line_end) - line_start;
            if e > s {
                last.spans.push((s, e));
            }
            last.starts += 1;
        } else {
            if let Some(max) = o.max_count {
                if lines.len() as u64 >= max {
                    break;
                }
            }
            let s = mstart - line_start;
            let e = mend.min(line_end) - line_start;
            lines.push(MatchLine {
                line_number: line_no,
                spans: if e > s { vec![(s, e)] } else { Vec::new() },
                line: data[line_start..line_end].to_vec(),
                is_match: true,
                starts: 1,
            });
            cur_line = Some((line_start, line_end));
        }
    }
    lines
}

/// Line-by-line scan with -r: each matched line is rebuilt with every match
/// replaced by its `$group` expansion, and spans point at the replacements.
fn scan_single_replace(re: &regex::bytes::Regex, data: &[u8], o: &ScanOpts) -> Vec<MatchLine> {
    let rep = o.replace.unwrap_or(b"");
    let mut lines: Vec<MatchLine> = Vec::new();
    let mut line_no: u64 = 1;
    let mut counted_to: usize = 0;
    let mut line_anchor: usize = 0;
    // The line currently being rebuilt.
    let mut cur: Option<(usize, usize)> = None;
    let mut cur_no: u64 = 0;
    let mut rebuilt: Vec<u8> = Vec::new();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut starts: u32 = 0;
    let mut src = 0usize; // absolute position in `data` copied so far

    let mut it = MatchIter::new(re, data, o.word);
    while let Some(caps) = it.next_caps() {
        let m = caps.get(0).unwrap();
        if memchr::memchr(b'\n', &data[m.start()..m.end()]).is_some() {
            continue;
        }
        if o.matches_only {
            return existence();
        }
        for p in memchr::memchr_iter(b'\n', &data[counted_to..m.start()]) {
            line_no += 1;
            line_anchor = counted_to + p + 1;
        }
        counted_to = m.start();
        let ls = line_anchor;
        let le = memchr::memchr(b'\n', &data[m.start()..]).map_or(data.len(), |p| m.start() + p);

        if cur != Some((ls, le)) {
            if let Some((_, pe)) = cur.take() {
                rebuilt.extend_from_slice(&data[src..pe]);
                lines.push(MatchLine {
                    line_number: cur_no,
                    spans: std::mem::take(&mut spans),
                    line: std::mem::take(&mut rebuilt),
                    is_match: true,
                    starts,
                });
                starts = 0;
            }
            if let Some(max) = o.max_count {
                if lines.len() as u64 >= max {
                    return lines;
                }
            }
            cur = Some((ls, le));
            cur_no = line_no;
            src = ls;
        }
        rebuilt.extend_from_slice(&data[src..m.start()]);
        let s0 = rebuilt.len();
        caps.expand(rep, &mut rebuilt);
        if rebuilt.len() > s0 {
            spans.push((s0, rebuilt.len()));
        }
        starts += 1;
        src = m.end();
    }
    if let Some((_, pe)) = cur {
        rebuilt.extend_from_slice(&data[src..pe]);
        lines.push(MatchLine {
            line_number: cur_no,
            spans,
            line: rebuilt,
            is_match: true,
            starts,
        });
    }
    lines
}

/// -U scan: a match may span newlines; every line it touches is emitted
/// (spans clipped per line). With -r the whole touched region is rebuilt
/// around the expansion and re-split into lines, like ripgrep. -m counts
/// matches here, not lines.
fn scan_multiline(re: &regex::bytes::Regex, data: &[u8], o: &ScanOpts) -> Vec<MatchLine> {
    let mut lines: Vec<MatchLine> = Vec::new();
    let mut line_no: u64 = 1;
    let mut counted_to: usize = 0;
    let mut line_anchor: usize = 0;
    let mut matches_seen: u64 = 0;

    if let Some(rep) = o.replace {
        let mut it = MatchIter::new(re, data, o.word);
        while let Some(caps) = it.next_caps() {
            let m = caps.get(0).unwrap();
            if o.matches_only {
                return existence();
            }
            if let Some(max) = o.max_count {
                if matches_seen >= max {
                    break;
                }
            }
            matches_seen += 1;
            for p in memchr::memchr_iter(b'\n', &data[counted_to..m.start()]) {
                line_no += 1;
                line_anchor = counted_to + p + 1;
            }
            counted_to = m.start();
            let ls = line_anchor;
            // End of the line containing the match's last byte (a match
            // ending in '\n' does not touch the following line).
            let last = if m.end() > m.start() {
                m.end() - 1
            } else {
                m.start().min(data.len().saturating_sub(1))
            };
            let region_end = memchr::memchr(b'\n', &data[last.min(data.len())..])
                .map_or(data.len(), |p| last + p);

            let mut rebuilt: Vec<u8> = data[ls..m.start()].to_vec();
            let s0 = rebuilt.len();
            caps.expand(rep, &mut rebuilt);
            let s1 = rebuilt.len();
            if region_end > m.end() {
                rebuilt.extend_from_slice(&data[m.end()..region_end]);
            }
            // Re-split the rebuilt region; line numbers run on from the
            // match's first line.
            let mut off = 0usize;
            let mut ln = line_no;
            let mut first = true;
            loop {
                let nl = memchr::memchr(b'\n', &rebuilt[off..]);
                let end = nl.map_or(rebuilt.len(), |p| off + p);
                let (cs, ce) = (s0.max(off), s1.min(end));
                lines.push(MatchLine {
                    line_number: ln,
                    spans: if ce > cs {
                        vec![(cs - off, ce - off)]
                    } else {
                        Vec::new()
                    },
                    line: rebuilt[off..end].to_vec(),
                    is_match: true,
                    starts: u32::from(first),
                });
                first = false;
                match nl {
                    None => break,
                    Some(p) => {
                        off = off + p + 1;
                        ln += 1;
                    }
                }
            }
        }
        return lines;
    }

    let mut it = MatchIter::new(re, data, o.word);
    while let Some((mstart, mend)) = it.next_span() {
        if o.matches_only {
            return existence();
        }
        if let Some(max) = o.max_count {
            if matches_seen >= max {
                break;
            }
        }
        matches_seen += 1;
        for p in memchr::memchr_iter(b'\n', &data[counted_to..mstart]) {
            line_no += 1;
            line_anchor = counted_to + p + 1;
        }
        counted_to = mstart;

        let last = if mend > mstart {
            mend - 1
        } else {
            mstart.min(data.len().saturating_sub(1))
        };
        let mut ls = line_anchor;
        let mut ln = line_no;
        loop {
            let le = memchr::memchr(b'\n', &data[ls..]).map_or(data.len(), |p| ls + p);
            let s = mstart.max(ls);
            let e = mend.min(le);
            let is_first = ln == line_no;
            // The previous match may already have emitted this line; merge
            // instead of duplicating. Only the tail of `lines` can qualify.
            let mut merged = false;
            for l in lines.iter_mut().rev() {
                if l.line_number < ln {
                    break;
                }
                if l.line_number == ln {
                    if e > s {
                        l.spans.push((s - ls, e - ls));
                    }
                    l.starts += u32::from(is_first);
                    merged = true;
                    break;
                }
            }
            if !merged {
                lines.push(MatchLine {
                    line_number: ln,
                    spans: if e > s {
                        vec![(s - ls, e - ls)]
                    } else {
                        Vec::new()
                    },
                    line: data[ls..le].to_vec(),
                    is_match: true,
                    starts: u32::from(is_first),
                });
            }
            if le >= data.len() || last <= le {
                break;
            }
            ls = le + 1;
            ln += 1;
        }
    }
    lines
}

/// Given the matching lines, pull in `before`/`after` neighbour lines.
/// Returns lines in order, with context lines marked `is_match = false`.
/// Overlapping context windows merge naturally (one entry per line number).
/// Matched lines keep their own text (which may be rebuilt by -r) rather
/// than being re-read from `data`.
fn expand_context(
    data: &[u8],
    matches: Vec<MatchLine>,
    before: usize,
    after: usize,
) -> Vec<MatchLine> {
    use std::collections::BTreeMap;

    // Wanted inclusive line ranges, merged.
    let mut ranges: Vec<(u64, u64)> = Vec::with_capacity(matches.len());
    for m in &matches {
        let lo = m.line_number.saturating_sub(before as u64).max(1);
        let hi = m.line_number + after as u64;
        match ranges.last_mut() {
            Some(last) if lo <= last.1 + 1 => last.1 = last.1.max(hi),
            _ => ranges.push((lo, hi)),
        }
    }
    // line number -> the matched entry itself.
    let mut by_line: BTreeMap<u64, MatchLine> = BTreeMap::new();
    for m in matches {
        by_line.insert(m.line_number, m);
    }

    let mut out: Vec<MatchLine> = Vec::new();
    let mut start = 0usize;
    let mut line_no: u64 = 1;
    let mut ri = 0usize;
    while start < data.len() {
        let line_end = memchr::memchr(b'\n', &data[start..]).map_or(data.len(), |p| start + p);
        while ri < ranges.len() && ranges[ri].1 < line_no {
            ri += 1;
        }
        if ri < ranges.len() && ranges[ri].0 <= line_no {
            match by_line.remove(&line_no) {
                Some(m) => out.push(m),
                None => out.push(MatchLine {
                    line_number: line_no,
                    spans: Vec::new(),
                    line: data[start..line_end].to_vec(),
                    is_match: false,
                    starts: 0,
                }),
            }
        }
        if line_end == data.len() {
            break;
        }
        start = line_end + 1;
        line_no += 1;
    }
    out
}

/// Confirming scan of candidate files, in parallel over a work-stealing pool.
/// Returns matched results (sorted by path) and how many files were scanned.
/// Shared by the indexed and the index-less (walk) search paths.
fn scan_targets(
    root: &Path,
    targets: &[(u32, String, u64)],
    matcher: &Matcher,
    opts: &SearchOptions,
) -> (Vec<FileResult>, usize) {
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<FileResult>> = Mutex::new(Vec::new());
    let scanned = AtomicUsize::new(0);
    // File opens dominate the scan on Windows and block their thread, so run
    // more threads than cores to overlap that syscall latency; the shared
    // queue keeps CPU-bound regex work balanced anyway. (Measured on the
    // kernel tree: 2x/4x/8x are within noise of each other, 4x picked as
    // the portable middle.)
    let nthreads = (opts.threads * 4).clamp(1, 64).min(targets.len().max(1));
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
                    let lines = scan_buffer_opts(
                        &matcher.regex,
                        &data,
                        &ScanOpts {
                            matches_only: opts.matches_only,
                            max_count: opts.max_count,
                            before: opts.before,
                            after: opts.after,
                            multiline: opts.multiline,
                            replace: opts.replace.as_deref(),
                            word: opts.word,
                            invert: opts.invert,
                            only: opts.only_matching,
                        },
                    );
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
    let mut results = results.into_inner().unwrap();
    results.sort_unstable_by(|a, b| a.rel_path.cmp(&b.rel_path));
    (results, scanned.load(Ordering::Relaxed))
}

/// Search using an index view (base + optional overlay). Returns per-file
/// results sorted by path.
pub fn search_index(
    view: &View,
    root: &Path,
    matcher: &Matcher,
    opts: &SearchOptions,
) -> Result<(Vec<FileResult>, SearchStats), SearchError> {
    let mut stats = SearchStats {
        query_display: if opts.invert {
            format!(
                "{} (inverted: every file must be scanned)",
                matcher.query.display()
            )
        } else {
            matcher.query.display()
        },
        files_in_index: view.file_count(),
        ..Default::default()
    };

    let filter = FileFilter::build(opts)?;

    let t0 = Instant::now();
    // -v needs every file: zero trigram hits just means every line is a
    // non-match, i.e. output.
    let ids = if opts.invert {
        view.all_ids()
    } else {
        match eval(&matcher.query, view)? {
            IdSet::Ids(ids) => ids,
            IdSet::All => view.all_ids(),
        }
    };
    stats.lookup_micros = t0.elapsed().as_micros();

    // Candidates = query hits + always-scan files − binary, path/glob/type-filtered.
    // Only candidate metadata is touched — the scan-always list is its own
    // index section, so nothing here walks the whole file table.
    let mut targets: Vec<(u32, String, u64)> = Vec::with_capacity(ids.len());
    {
        let mut push_target = |id: u32, skip_scan_always: bool| -> Result<(), SearchError> {
            let meta = view.file(id).map_err(SearchError::Index)?;
            if meta.flags & FLAG_BINARY != 0 {
                return Ok(());
            }
            if skip_scan_always && meta.flags & FLAG_SCAN_ALWAYS != 0 {
                return Ok(()); // added from the scan-always section instead
            }
            if !in_scope(meta.rel_path, &opts.path_scopes) || !filter.accept(meta.rel_path) {
                return Ok(());
            }
            targets.push((id, meta.rel_path.to_string(), meta.size));
            Ok(())
        };
        for &id in ids.iter() {
            push_target(id, true)?;
        }
        for id in view.scan_always() {
            push_target(id, false)?;
        }
    }
    stats.candidates = targets.len();

    // Parallel confirming scan.
    let t1 = Instant::now();
    let (results, files_scanned) = scan_targets(root, &targets, matcher, opts);
    stats.scan_micros = t1.elapsed().as_micros();
    stats.files_scanned = files_scanned;
    stats.files_matched = results.len();
    stats.lines_matched = results
        .iter()
        .map(|r| r.lines.iter().filter(|l| l.is_match).count())
        .sum();
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

    let filter = FileFilter::build(opts)?;
    let mut targets: Vec<(u32, String, u64)> = Vec::new();
    for cand in crate::index::build::collect_candidates(root, opts.threads)? {
        if !in_scope(&cand.rel_path, &opts.path_scopes) || !filter.accept(&cand.rel_path) {
            continue;
        }
        targets.push((0, cand.rel_path, cand.size));
    }
    stats.candidates = targets.len();

    let t1 = Instant::now();
    let (results, files_scanned) = scan_targets(root, &targets, matcher, opts);
    stats.scan_micros = t1.elapsed().as_micros();
    stats.files_scanned = files_scanned;
    stats.files_matched = results.len();
    stats.lines_matched = results
        .iter()
        .map(|r| r.lines.iter().filter(|l| l.is_match).count())
        .sum();
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
    fn context_before_after() {
        let data = b"l1\nl2\nMATCH\nl4\nl5\n";
        // -C1: line 2, MATCH(3), line 4.
        let lines = scan_buffer_ctx(&re("MATCH"), data, false, None, 1, 1);
        let nums: Vec<u64> = lines.iter().map(|l| l.line_number).collect();
        assert_eq!(nums, vec![2, 3, 4]);
        assert!(!lines[0].is_match);
        assert!(lines[1].is_match);
        assert_eq!(lines[1].line, b"MATCH");
        assert!(!lines[2].is_match);
        // -B2 clamps at the top of the file.
        let lines = scan_buffer_ctx(&re("MATCH"), data, false, None, 2, 0);
        assert_eq!(
            lines.iter().map(|l| l.line_number).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn context_windows_merge() {
        // Two matches whose context windows touch should not duplicate lines.
        let data = b"a\nHIT\nb\nHIT\nc\n";
        let lines = scan_buffer_ctx(&re("HIT"), data, false, None, 1, 1);
        // lines 1..5 all covered exactly once, in order.
        assert_eq!(
            lines.iter().map(|l| l.line_number).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
        assert_eq!(lines.iter().filter(|l| l.is_match).count(), 2);
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

    fn filter(globs: &[&str], select: &[&str], negate: &[&str]) -> FileFilter {
        let opts = SearchOptions {
            globs: globs.iter().map(|s| s.to_string()).collect(),
            types_select: select.iter().map(|s| s.to_string()).collect(),
            types_negate: negate.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        FileFilter::build(&opts).unwrap()
    }

    #[test]
    fn glob_filter() {
        let f = filter(&["*.rs"], &[], &[]);
        assert!(f.accept("src/main.rs"));
        assert!(!f.accept("README.md"));
        assert!(!f.accept("src/data.json"));

        // exclusion glob
        let f = filter(&["!*.rs"], &[], &[]);
        assert!(!f.accept("src/main.rs"));
        assert!(f.accept("README.md"));

        // no globs => everything
        let f = filter(&[], &[], &[]);
        assert!(f.accept("anything.xyz"));
    }

    #[test]
    fn type_filter() {
        let f = filter(&[], &["rust"], &[]);
        assert!(f.accept("src/main.rs"));
        assert!(!f.accept("script.py"));

        // negate a type
        let f = filter(&[], &[], &["rust"]);
        assert!(!f.accept("src/main.rs"));
        assert!(f.accept("script.py"));
    }

    #[test]
    fn unknown_type_errors() {
        let opts = SearchOptions {
            types_select: vec!["definitely-not-a-type".into()],
            ..Default::default()
        };
        assert!(FileFilter::build(&opts).is_err());
    }

    #[test]
    fn replace_rebuilds_lines() {
        let data = b"alice@example\nbob@test plus carol@dev\n";
        let rx = re(r"(\w+)@(\w+)");
        let o = ScanOpts {
            replace: Some(b"[$2/$1]"),
            ..Default::default()
        };
        let lines = scan_buffer_opts(&rx, data, &o);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].line, b"[example/alice]");
        assert_eq!(lines[0].spans, vec![(0, 15)]);
        assert_eq!(lines[1].line, b"[test/bob] plus [dev/carol]");
        assert_eq!(lines[1].spans, vec![(0, 10), (16, 27)]);
        assert_eq!(lines[1].starts, 2);

        // $$ is a literal dollar; a missing group expands to nothing.
        let o = ScanOpts {
            replace: Some(b"$$9"),
            ..Default::default()
        };
        let lines = scan_buffer_opts(&re("alice"), data, &o);
        assert_eq!(lines[0].line, b"$9@example");
        let o = ScanOpts {
            replace: Some(b"<$5>"),
            ..Default::default()
        };
        let lines = scan_buffer_opts(&re("alice"), data, &o);
        assert_eq!(lines[0].line, b"<>@example");
    }

    #[test]
    fn replace_with_context_keeps_rebuilt_line() {
        let data = b"l1\nfoo here\nl3\n";
        let o = ScanOpts {
            replace: Some(b"BAR"),
            before: 1,
            after: 1,
            ..Default::default()
        };
        let lines = scan_buffer_opts(&re("foo"), data, &o);
        let nums: Vec<u64> = lines.iter().map(|l| l.line_number).collect();
        assert_eq!(nums, vec![1, 2, 3]);
        assert_eq!(lines[1].line, b"BAR here");
        assert!(!lines[0].is_match && lines[1].is_match && !lines[2].is_match);
    }

    #[test]
    fn multiline_emits_every_touched_line() {
        let data = b"one\nfoo\nbar\nfoo\nbaz\n";
        let rx = re(r"foo\nba.");
        let o = ScanOpts {
            multiline: true,
            ..Default::default()
        };
        let lines = scan_buffer_opts(&rx, data, &o);
        let nums: Vec<u64> = lines.iter().map(|l| l.line_number).collect();
        assert_eq!(nums, vec![2, 3, 4, 5]);
        assert!(lines.iter().all(|l| l.is_match));
        let starts: Vec<u32> = lines.iter().map(|l| l.starts).collect();
        assert_eq!(starts, vec![1, 0, 1, 0], "-c counts matches, not lines");
        assert_eq!(lines[0].spans, vec![(0, 3)]);
        assert_eq!(lines[1].spans, vec![(0, 3)]);

        // -m 1 keeps only the first match (both of its lines).
        let o = ScanOpts {
            multiline: true,
            max_count: Some(1),
            ..Default::default()
        };
        let lines = scan_buffer_opts(&rx, data, &o);
        let nums: Vec<u64> = lines.iter().map(|l| l.line_number).collect();
        assert_eq!(nums, vec![2, 3]);

        // A match ending in '\n' does not touch the following line.
        let o = ScanOpts {
            multiline: true,
            ..Default::default()
        };
        let lines = scan_buffer_opts(&re(r"foo\n"), data, &o);
        let nums: Vec<u64> = lines.iter().map(|l| l.line_number).collect();
        assert_eq!(nums, vec![2, 4]);
    }

    #[test]
    fn multiline_adjacent_matches_share_a_line() {
        let data = b"one two\nthree four\n";
        let o = ScanOpts {
            multiline: true,
            ..Default::default()
        };
        let lines = scan_buffer_opts(&re(r"e two\nthr|ur"), data, &o);
        let nums: Vec<u64> = lines.iter().map(|l| l.line_number).collect();
        assert_eq!(nums, vec![1, 2], "shared line must not be duplicated");
        assert_eq!(lines[1].spans, vec![(0, 3), (8, 10)]);
        assert_eq!(lines[0].starts + lines[1].starts, 2);
    }

    #[test]
    fn multiline_replace_collapses_region() {
        let data = b"a\nfoo\nbar\nz\n";
        let o = ScanOpts {
            multiline: true,
            replace: Some(b"XX"),
            ..Default::default()
        };
        let lines = scan_buffer_opts(&re(r"foo\nbar"), data, &o);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].line_number, 2);
        assert_eq!(lines[0].line, b"XX");
        assert_eq!(lines[0].spans, vec![(0, 2)]);
    }

    #[test]
    fn word_is_surrounded_not_bounded() {
        // rg semantics: `-w '::'` matches in " x :: y" (non-word neighbours)
        // but NOT in "a::b" (word neighbour) — the opposite of \b-wrapping.
        let data = b"a::b\n x :: y\n";
        let o = ScanOpts {
            word: true,
            ..Default::default()
        };
        let lines = scan_buffer_opts(&re("::"), data, &o);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].line_number, 2);

        // Identifier case: "foo" not inside "foofoo".
        let lines = scan_buffer_opts(&re("foo"), b"foo foofoo\n", &o);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans, vec![(0, 3)]);

        // Rejected match must not swallow an overlapping bounded one:
        // 'a:a' in "za:a:a" is rejected at 1 but accepted at 3.
        let lines = scan_buffer_opts(&re("a:a"), b"za:a:a\n", &o);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans, vec![(3, 6)]);
    }

    #[test]
    fn word_composes_with_replace() {
        let o = ScanOpts {
            word: true,
            replace: Some(b"X"),
            ..Default::default()
        };
        let lines = scan_buffer_opts(&re("foo"), b"foo foofoo\n", &o);
        assert_eq!(lines[0].line, b"X foofoo");
    }

    #[test]
    fn invert_emits_non_matching_lines() {
        let data = b"foo\nbar\nfoo baz\nqux\n";
        let o = ScanOpts {
            invert: true,
            ..Default::default()
        };
        let lines = scan_buffer_opts(&re("foo"), data, &o);
        let nums: Vec<u64> = lines.iter().map(|l| l.line_number).collect();
        assert_eq!(nums, vec![2, 4]);
        assert!(lines.iter().all(|l| l.is_match && l.starts == 1));

        // -m limits emitted lines; -l short-circuits.
        let o = ScanOpts {
            invert: true,
            max_count: Some(1),
            ..Default::default()
        };
        assert_eq!(scan_buffer_opts(&re("foo"), data, &o).len(), 1);
        let o = ScanOpts {
            invert: true,
            matches_only: true,
            ..Default::default()
        };
        assert_eq!(scan_buffer_opts(&re("foo"), data, &o).len(), 1);
        // Every line matches -> nothing to invert.
        let o = ScanOpts {
            invert: true,
            ..Default::default()
        };
        assert!(scan_buffer_opts(&re("."), data, &o).is_empty());
    }

    #[test]
    fn invert_multiline_excludes_touched_lines() {
        let data = b"one\nfoo\nbar\nmid\nfoo\nbaz\n";
        let o = ScanOpts {
            invert: true,
            multiline: true,
            ..Default::default()
        };
        let lines = scan_buffer_opts(&re(r"foo\nba."), data, &o);
        let nums: Vec<u64> = lines.iter().map(|l| l.line_number).collect();
        assert_eq!(nums, vec![1, 4]);
    }

    #[test]
    fn only_matching_rows() {
        let data = b"foo bar foo\nnope\nfoo\n";
        let o = ScanOpts {
            only: true,
            ..Default::default()
        };
        let lines = scan_buffer_opts(&re("foo"), data, &o);
        let rows: Vec<(u64, &[u8])> = lines
            .iter()
            .map(|l| (l.line_number, l.line.as_slice()))
            .collect();
        assert_eq!(rows, vec![(1, b"foo".as_slice()), (1, b"foo"), (3, b"foo")]);
        // -c under -o counts matches: starts sum to 3.
        assert_eq!(lines.iter().map(|l| l.starts).sum::<u32>(), 3);

        // -o with -r prints the expansion.
        let o = ScanOpts {
            only: true,
            replace: Some(b"<$1>"),
            ..Default::default()
        };
        let lines = scan_buffer_opts(&re("f(o)o"), data, &o);
        assert!(lines.iter().all(|l| l.line == b"<o>"));

        // -o with -U prints per-line fragments (rg: "1:AA", "2:BB").
        let o = ScanOpts {
            only: true,
            multiline: true,
            ..Default::default()
        };
        let lines = scan_buffer_opts(&re(r"AA\nBB"), b"AA\nBB\nCC\n", &o);
        let rows: Vec<(u64, &[u8])> = lines
            .iter()
            .map(|l| (l.line_number, l.line.as_slice()))
            .collect();
        assert_eq!(rows, vec![(1, b"AA".as_slice()), (2, b"BB")]);
        assert_eq!(lines.iter().map(|l| l.starts).sum::<u32>(), 1);
    }

    #[test]
    fn smart_case_checks_literals_only() {
        // Class names like \S are not uppercase literals.
        assert!(!pattern_has_uppercase(r"\Sfoo", false));
        assert!(pattern_has_uppercase("Sfoo", false));
        assert!(pattern_has_uppercase("[A-Z]x", false));
        assert!(!pattern_has_uppercase(r"foo\w+", false));
        assert!(pattern_has_uppercase("Foo", true));
        assert!(!pattern_has_uppercase("foo", true));

        // End to end: -S lowers the case only for lowercase patterns.
        let data = b"xfoo\nXFOO\n";
        let opts = SearchOptions {
            smart_case: true,
            ..Default::default()
        };
        let m = compile(r"\Sfoo", &opts).unwrap();
        assert_eq!(scan_buffer(&m.regex, data, false, None).len(), 2);
        let m = compile("Sfoo", &opts).unwrap();
        assert_eq!(scan_buffer(&m.regex, data, false, None).len(), 0);
    }

    #[test]
    fn merge_set_ops() {
        assert_eq!(intersect(vec![1, 3, 5], vec![2, 3, 5, 7]), vec![3, 5]);
        assert_eq!(union(vec![1, 5], vec![2, 5, 9]), vec![1, 2, 5, 9]);
        assert_eq!(intersect(vec![], vec![1]), Vec::<u32>::new());
        assert_eq!(union(vec![], vec![1]), vec![1]);
    }
}
