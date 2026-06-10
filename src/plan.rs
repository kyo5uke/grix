//! Regex -> trigram query planning.
//!
//! This is a clean-room Rust adaptation of the algorithm Russ Cox described
//! for Google Code Search ("Regular Expression Matching with a Trigram
//! Index", swtch.com/~rsc/regexp/regexp4.html), built on top of
//! regex-syntax's HIR instead of Go's syntax tree.
//!
//! For every regex node we track:
//! - `can_empty`: can it match the empty string?
//! - `exact`: the *complete* set of strings it can match (when small enough)
//! - `prefix` / `suffix`: sets of possible match prefixes/suffixes otherwise
//! - `query`: trigrams that any match *must* contain (the index constraint)
//!
//! INVARIANT (correctness): `query` may only require trigrams that are
//! guaranteed to appear in any string matching the regex. When in doubt the
//! planner degrades to `Query::All` (scan everything); the confirming
//! scan keeps results exact. Over-constraining would silently drop matches,
//! so every transformation below errs on the side of `All`.

use std::collections::BTreeSet;

use regex_syntax::hir::{Class, Hir, HirKind, Look};

use crate::trigram;

const MAX_EXACT: usize = 7;
const MAX_SET: usize = 20;
/// Char classes bigger than this stop being enumerated and become "any char".
const MAX_CLASS: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// No index constraint: every indexed file is a candidate.
    All,
    /// Cannot match (e.g. empty alternation).
    None,
    /// Candidate files must contain this trigram.
    Tri(u32),
    And(Vec<Query>),
    Or(Vec<Query>),
}

impl Query {
    pub fn and(self, other: Query) -> Query {
        match (self, other) {
            (Query::None, _) | (_, Query::None) => Query::None,
            (Query::All, q) | (q, Query::All) => q,
            (a, b) => combine(a, b, true),
        }
    }

    pub fn or(self, other: Query) -> Query {
        match (self, other) {
            (Query::All, _) | (_, Query::All) => Query::All,
            (Query::None, q) | (q, Query::None) => q,
            (a, b) => combine(a, b, false),
        }
    }

    /// Human-readable form, normalized (children sorted) for tests/debugging.
    pub fn display(&self) -> String {
        match self {
            Query::All => "ALL".into(),
            Query::None => "NONE".into(),
            Query::Tri(t) => format!("\"{}\"", trigram::unpack_display(*t)),
            Query::And(subs) => {
                let mut parts: Vec<String> = subs.iter().map(|q| q.display()).collect();
                parts.sort();
                parts.join(" ")
            }
            Query::Or(subs) => {
                let mut parts: Vec<String> = subs.iter().map(|q| q.display()).collect();
                parts.sort();
                format!("({})", parts.join("|"))
            }
        }
    }
}

fn combine(a: Query, b: Query, is_and: bool) -> Query {
    // Flatten same-op children into one level.
    let mut parts: Vec<Query> = Vec::new();
    for q in [a, b] {
        match (q, is_and) {
            (Query::And(subs), true) | (Query::Or(subs), false) => parts.extend(subs),
            (q, _) => parts.push(q),
        }
    }
    // Dedup exact duplicates.
    let mut deduped: Vec<Query> = Vec::with_capacity(parts.len());
    for q in parts {
        if !deduped.contains(&q) {
            deduped.push(q);
        }
    }
    prune_subsumed(&mut deduped, is_and);
    if deduped.len() == 1 {
        return deduped.pop().unwrap();
    }
    if is_and {
        Query::And(deduped)
    } else {
        Query::Or(deduped)
    }
}

/// Drop branches implied by other branches.
///
/// A branch representable as a pure trigram conjunction is compared by set:
/// in an OR, a branch whose requirement set is a superset of another's is
/// stronger and therefore redundant (the weaker one already admits its
/// files); in an AND, a branch whose set is a subset of another's is implied
/// by it. Both prunings preserve query semantics exactly.
fn prune_subsumed(parts: &mut Vec<Query>, is_and: bool) {
    if parts.len() < 2 {
        return;
    }
    let sets: Vec<Option<BTreeSet<u32>>> = parts.iter().map(as_tri_set).collect();
    let mut keep = vec![true; parts.len()];
    for i in 0..parts.len() {
        let Some(si) = &sets[i] else { continue };
        for j in 0..parts.len() {
            if i == j || !keep[j] || !keep[i] {
                continue;
            }
            let Some(sj) = &sets[j] else { continue };
            let redundant = if is_and {
                si.is_subset(sj)
            } else {
                si.is_superset(sj)
            };
            // For set-equal branches keep the earliest one.
            if redundant && !(si == sj && i < j) {
                keep[i] = false;
            }
        }
    }
    let mut it = keep.iter();
    parts.retain(|_| *it.next().unwrap());
}

/// The trigram set of a pure conjunction (Tri or And-of-Tri), else None.
fn as_tri_set(q: &Query) -> Option<BTreeSet<u32>> {
    match q {
        Query::Tri(t) => Some(std::iter::once(*t).collect()),
        Query::And(subs) => {
            let mut set = BTreeSet::new();
            for s in subs {
                match s {
                    Query::Tri(t) => {
                        set.insert(*t);
                    }
                    _ => return None,
                }
            }
            Some(set)
        }
        _ => None,
    }
}

type StrSet = BTreeSet<Vec<u8>>;

fn set_of(items: &[&[u8]]) -> StrSet {
    items.iter().map(|s| s.to_vec()).collect()
}

fn min_len(s: &StrSet) -> usize {
    s.iter().map(|x| x.len()).min().unwrap_or(0)
}

fn cross(a: &StrSet, b: &StrSet) -> StrSet {
    let mut out = StrSet::new();
    for x in a {
        for y in b {
            let mut s = x.clone();
            s.extend_from_slice(y);
            out.insert(s);
        }
    }
    out
}

fn union(a: &StrSet, b: &StrSet) -> StrSet {
    a.union(b).cloned().collect()
}

/// OR over the strings of `set`, each contributing the AND of its trigrams.
/// If any string is shorter than 3 bytes it guarantees nothing -> All.
fn or_trigrams(set: &StrSet) -> Query {
    if set.is_empty() {
        return Query::None;
    }
    if min_len(set) < 3 {
        return Query::All;
    }
    let mut q = Query::None;
    for s in set {
        let mut conj = Query::All;
        for t in trigram::of_str(s) {
            conj = conj.and(Query::Tri(t));
        }
        q = q.or(conj);
    }
    q
}

#[derive(Debug, Clone)]
struct Info {
    can_empty: bool,
    exact: Option<StrSet>,
    prefix: StrSet,
    suffix: StrSet,
    query: Query,
}

fn empty_string() -> Info {
    Info {
        can_empty: true,
        exact: Some(set_of(&[b""])),
        prefix: StrSet::new(),
        suffix: StrSet::new(),
        query: Query::All,
    }
}

fn no_match() -> Info {
    Info {
        can_empty: false,
        exact: Some(StrSet::new()),
        prefix: StrSet::new(),
        suffix: StrSet::new(),
        query: Query::None,
    }
}

fn any_char() -> Info {
    Info {
        can_empty: false,
        exact: None,
        prefix: set_of(&[b""]),
        suffix: set_of(&[b""]),
        query: Query::All,
    }
}

fn any_match() -> Info {
    Info {
        can_empty: true,
        exact: None,
        prefix: set_of(&[b""]),
        suffix: set_of(&[b""]),
        query: Query::All,
    }
}

fn literal(bytes: &[u8]) -> Info {
    if bytes.is_empty() {
        return empty_string();
    }
    let mut set = StrSet::new();
    set.insert(bytes.to_vec());
    let mut info = Info {
        can_empty: false,
        exact: Some(set),
        prefix: StrSet::new(),
        suffix: StrSet::new(),
        query: Query::All,
    };
    info.simplify(false);
    info
}

impl Info {
    /// Fold the exact set's trigrams into the query.
    fn add_exact(&mut self) {
        if let Some(exact) = &self.exact {
            self.query = std::mem::replace(&mut self.query, Query::All).and(or_trigrams(exact));
        }
    }

    fn simplify(&mut self, force: bool) {
        if let Some(exact) = &self.exact {
            let ml = min_len(exact);
            if !exact.is_empty()
                && (exact.len() > MAX_EXACT || (ml >= 3 && force) || ml >= 4)
            {
                self.add_exact();
                let exact = self.exact.take().unwrap();
                for s in exact {
                    let n = s.len();
                    if n < 3 {
                        self.prefix.insert(s.clone());
                        self.suffix.insert(s);
                    } else {
                        self.prefix.insert(s[..2].to_vec());
                        self.suffix.insert(s[n - 2..].to_vec());
                    }
                }
            }
        }
        if self.exact.is_none() {
            self.simplify_set(false);
            self.simplify_set(true);
        }
    }

    /// Capture the trigram information of prefix/suffix, then shorten the
    /// set so later compositions stay bounded.
    fn simplify_set(&mut self, is_suffix: bool) {
        let set = if is_suffix { &self.suffix } else { &self.prefix };
        self.query = std::mem::replace(&mut self.query, Query::All).and(or_trigrams(set));

        let set = if is_suffix {
            &mut self.suffix
        } else {
            &mut self.prefix
        };
        let mut n = 3usize;
        loop {
            let mut next = StrSet::new();
            for s in set.iter() {
                if s.len() >= n {
                    if is_suffix {
                        next.insert(s[s.len() - (n - 1)..].to_vec());
                    } else {
                        next.insert(s[..n - 1].to_vec());
                    }
                } else {
                    next.insert(s.clone());
                }
            }
            *set = next;
            if set.len() <= MAX_SET || n <= 1 {
                break;
            }
            n -= 1;
        }
    }
}

fn concat(x: Info, y: Info) -> Info {
    let mut out = no_match();
    if let (Some(xe), Some(ye)) = (x.exact.as_ref(), y.exact.as_ref()) {
        out.exact = Some(cross(xe, ye));
    } else {
        out.exact = None;
        if let Some(xe) = &x.exact {
            out.prefix = cross(xe, &y.prefix);
        } else {
            out.prefix = x.prefix.clone();
            if x.can_empty {
                out.prefix = union(&out.prefix, &y.prefix);
            }
        }
        if let Some(ye) = &y.exact {
            out.suffix = cross(&x.suffix, ye);
        } else {
            out.suffix = y.suffix.clone();
            if y.can_empty {
                out.suffix = union(&out.suffix, &x.suffix);
            }
        }
    }

    // When neither side is exact, matches straddle the boundary: the strings
    // in x.suffix × y.prefix are guaranteed substrings of any match, and
    // their trigrams are not yet accounted for in x.query / y.query.
    let mut boundary = Query::All;
    if x.exact.is_none()
        && y.exact.is_none()
        && x.suffix.len() <= MAX_SET
        && y.prefix.len() <= MAX_SET
        && min_len(&x.suffix) + min_len(&y.prefix) >= 3
    {
        boundary = or_trigrams(&cross(&x.suffix, &y.prefix));
    }

    out.can_empty = x.can_empty && y.can_empty;
    out.query = x.query.and(y.query).and(boundary);
    out.simplify(false);
    out
}

fn alternate(x: Info, y: Info) -> Info {
    let mut x = x;
    let mut y = y;
    let mut out = no_match();
    match (x.exact.clone(), y.exact.clone()) {
        (Some(xe), Some(ye)) => {
            out.exact = Some(union(&xe, &ye));
        }
        (Some(xe), None) => {
            out.exact = None;
            out.prefix = union(&xe, &y.prefix);
            out.suffix = union(&xe, &y.suffix);
            x.add_exact();
        }
        (None, Some(ye)) => {
            out.exact = None;
            out.prefix = union(&x.prefix, &ye);
            out.suffix = union(&x.suffix, &ye);
            y.add_exact();
        }
        (None, None) => {
            out.exact = None;
            out.prefix = union(&x.prefix, &y.prefix);
            out.suffix = union(&x.suffix, &y.suffix);
        }
    }
    out.can_empty = x.can_empty || y.can_empty;
    out.query = x.query.or(y.query);
    out.simplify(false);
    out
}

fn analyze(hir: &Hir) -> Info {
    match hir.kind() {
        HirKind::Empty => empty_string(),
        HirKind::Literal(lit) => literal(&lit.0),
        HirKind::Class(class) => analyze_class(class),
        HirKind::Look(look) => match look {
            // Anchors and boundaries match the empty string; they constrain
            // *where*, not *what*, so they contribute nothing to trigrams.
            Look::Start
            | Look::End
            | Look::StartLF
            | Look::EndLF
            | Look::StartCRLF
            | Look::EndCRLF
            | Look::WordAscii
            | Look::WordAsciiNegate
            | Look::WordUnicode
            | Look::WordUnicodeNegate
            | Look::WordStartAscii
            | Look::WordEndAscii
            | Look::WordStartUnicode
            | Look::WordEndUnicode
            | Look::WordStartHalfAscii
            | Look::WordEndHalfAscii
            | Look::WordStartHalfUnicode
            | Look::WordEndHalfUnicode => empty_string(),
        },
        HirKind::Capture(cap) => analyze(&cap.sub),
        HirKind::Repetition(rep) => {
            if rep.min == 0 {
                if rep.max == Some(1) {
                    // x? = x | ""
                    alternate(analyze(&rep.sub), empty_string())
                } else {
                    // x* (or x{0,n}): zero copies allowed, nothing guaranteed.
                    any_match()
                }
            } else {
                // x+ / x{n,}: at least one copy, so x's trigrams must appear,
                // but matches are no longer exact strings.
                let mut info = analyze(&rep.sub);
                if let Some(exact) = info.exact.take() {
                    info.prefix = exact.clone();
                    info.suffix = exact;
                }
                info.simplify(false);
                info
            }
        }
        HirKind::Concat(subs) => {
            let mut iter = subs.iter();
            let first = match iter.next() {
                Some(h) => analyze(h),
                None => return empty_string(),
            };
            iter.fold(first, |acc, h| concat(acc, analyze(h)))
        }
        HirKind::Alternation(subs) => {
            let mut iter = subs.iter();
            let first = match iter.next() {
                Some(h) => analyze(h),
                None => return no_match(),
            };
            iter.fold(first, |acc, h| alternate(acc, analyze(h)))
        }
    }
}

fn analyze_class(class: &Class) -> Info {
    // Enumerate small classes into an exact set; large ones are "any char".
    let mut set = StrSet::new();
    match class {
        Class::Unicode(cls) => {
            let mut count: usize = 0;
            for range in cls.iter() {
                count += (u32::from(range.end()) - u32::from(range.start()) + 1) as usize;
                if count > MAX_CLASS {
                    return any_char();
                }
            }
            for range in cls.iter() {
                let (s, e) = (u32::from(range.start()), u32::from(range.end()));
                for cp in s..=e {
                    if let Some(c) = char::from_u32(cp) {
                        let mut buf = [0u8; 4];
                        set.insert(c.encode_utf8(&mut buf).as_bytes().to_vec());
                    }
                }
            }
        }
        Class::Bytes(cls) => {
            let mut count: usize = 0;
            for range in cls.iter() {
                count += (range.end() - range.start() + 1) as usize;
                if count > MAX_CLASS {
                    return any_char();
                }
            }
            for range in cls.iter() {
                for b in range.start()..=range.end() {
                    set.insert(vec![b]);
                }
            }
        }
    }
    if set.is_empty() {
        // An empty class matches nothing.
        return no_match();
    }
    let mut info = Info {
        can_empty: false,
        exact: Some(set),
        prefix: StrSet::new(),
        suffix: StrSet::new(),
        query: Query::All,
    };
    info.simplify(false);
    info
}

/// Plan the index query for `pattern`. Errors only on invalid regex syntax.
pub fn plan(pattern: &str, case_insensitive: bool) -> Result<Query, Box<regex_syntax::Error>> {
    let hir = regex_syntax::ParserBuilder::new()
        .case_insensitive(case_insensitive)
        .multi_line(true)
        .build()
        .parse(pattern)
        .map_err(Box::new)?;
    let mut info = analyze(&hir);
    info.simplify(true);
    info.add_exact();
    Ok(info.query)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(pattern: &str) -> String {
        plan(pattern, false).unwrap().display()
    }

    fn qi(pattern: &str) -> String {
        plan(pattern, true).unwrap().display()
    }

    #[test]
    fn literals() {
        assert_eq!(q("abc"), r#""abc""#);
        assert_eq!(q("Abcdef"), r#""Abc" "bcd" "cde" "def""#);
        // Too short to constrain.
        assert_eq!(q("ab"), "ALL");
        assert_eq!(q("a"), "ALL");
        assert_eq!(q(""), "ALL");
    }

    #[test]
    fn concat_groups() {
        assert_eq!(q("(abc)(def)"), r#""abc" "bcd" "cde" "def""#);
        assert_eq!(q("abc.*def"), r#""abc" "def""#);
        assert_eq!(q("abc.def"), r#""abc" "def""#);
    }

    #[test]
    fn alternations() {
        assert_eq!(q("abc|def"), r#"("abc"|"def")"#);
        assert_eq!(q("abcdef|ghijkl"), r#"("abc" "bcd" "cde" "def"|"ghi" "hij" "ijk" "jkl")"#);
        // One branch unconstrained poisons the whole alternation -> ALL.
        assert_eq!(q("abc|a"), "ALL");
    }

    #[test]
    fn repetitions() {
        assert_eq!(q("(abc)*"), "ALL");
        assert_eq!(q("(abcd)+"), r#""abc" "bcd""#);
        assert_eq!(q("(abc)+"), r#""abc""#);
        assert_eq!(q("(abc)?def"), r#""def""#);
        // x{2,} behaves like x+
        assert_eq!(q("(abcd){2,}"), r#""abc" "bcd""#);
    }

    #[test]
    fn anchors_and_boundaries() {
        assert_eq!(q("^abc$"), r#""abc""#);
        assert_eq!(q(r"\babcd\b"), r#""abc" "bcd""#);
    }

    #[test]
    fn classes() {
        // [ab]cde -> acd|bcd plus shared tail
        assert_eq!(q("[ab]cde"), r#"("acd" "cde"|"bcd" "cde")"#);
        // Big classes give up on the class but keep neighbors.
        assert_eq!(q("[a-z]abcd"), r#""abc" "bcd""#);
        assert_eq!(q(r"\wabcd"), r#""abc" "bcd""#);
        assert_eq!(q(r"\d+foobar"), r#""bar" "foo" "oba" "oob""#);
    }

    #[test]
    fn case_insensitive() {
        // (?i)abcd explodes into case variants; all still constrain.
        let plan = qi("abcd");
        assert!(plan.contains("abc") || plan.contains("ABC"), "{plan}");
        assert_ne!(plan, "ALL");
        // Long case-insensitive literals must not blow up.
        let plan = qi("intermediate_representation");
        assert_ne!(plan, "ALL");
    }

    #[test]
    fn pathological_degrade_to_all() {
        assert_eq!(q(".*"), "ALL");
        assert_eq!(q("a.*b"), "ALL");
        assert_eq!(q(r"\w+"), "ALL");
        assert_eq!(q("[^a]xyz[^b]"), r#""xyz""#);
    }

    #[test]
    fn never_none_for_matchable() {
        // A planner bug that yields NONE would silently hide results.
        for p in ["abc", "a|b", "x{0,3}", "(a)(b)(c)", "^$", "[0-9]+"] {
            let query = plan(p, false).unwrap();
            assert_ne!(query, Query::None, "pattern {p} planned to NONE");
        }
    }
}
