# How grix works

grix answers `grep`-style queries in milliseconds by consulting a trigram
index instead of reading every file. This document explains the moving
parts. It assumes nothing beyond knowing what a regular expression is.

## The big picture

```
grix index            grix <pattern>
     │                      │
     ▼                      ▼
┌─────────┐   ┌──────────────────────────┐
│ walker   │   │ planner: regex → trigram │
│ extract  │   │ query ("abc" AND "bcd")  │
│ postings │   └──────────┬───────────────┘
└────┬─────┘              ▼
     ▼            ┌──────────────┐    ┌─────────────────┐
  index file ───▶ │ posting list │ ─▶ │ confirming scan │ ─▶ results
  (mmap'd)        │ intersection │    │ (real regex, on │
                  └──────────────┘    │ current files)  │
                                      └─────────────────┘
```

Two phases at query time:

1. **Planning** narrows the candidate set using the index (microseconds).
2. **Confirming scan** runs the actual regex over the candidates' *current*
   content (milliseconds, parallel).

Because the scan always reads live files, grix never reports a line that is
not really there. A stale index can only *miss* very recent edits — run
`grix index` (incremental, typically ~1s) to catch up.

## Trigrams

A trigram is any 3 consecutive bytes. `hello` contains `hel`, `ell`, `llo`.
The index maps every trigram that occurs in the tree to the sorted list of
files containing it (a *posting list*), delta- and varint-encoded.

If you search for `hello`, a matching file **must** contain all three
trigrams. Intersecting three posting lists is enormously cheaper than
reading every file: that is the entire trick, and it is the same one behind
Google Code Search (2006).

## The planner: regex → trigram query

The interesting part is doing this for arbitrary regexes, not just literals.
`src/plan.rs` is a clean-room Rust implementation of the algorithm Russ Cox
described in [Regular Expression Matching with a Trigram
Index](https://swtch.com/~rsc/regexp/regexp4.html), adapted to operate on
[`regex-syntax`](https://docs.rs/regex-syntax)'s HIR.

For every regex node it computes:

| field      | meaning                                                     |
|------------|-------------------------------------------------------------|
| `can_empty`| can this subexpression match the empty string?              |
| `exact`    | the complete set of strings it can match (if small enough)  |
| `prefix`   | possible match prefixes (when `exact` overflows)            |
| `suffix`   | possible match suffixes                                     |
| `query`    | trigrams any match must contain (AND/OR tree)               |

Composition rules do the work. Sketches:

- **Concat**: `exact(xy) = exact(x) × exact(y)` (bounded cross product).
  When the product overflows, the boundary `suffix(x) × prefix(y)` still
  yields guaranteed substrings — their trigrams are ANDed into the query.
- **Alternation**: union of sets, OR of queries.
- **`x+`**: at least one copy, so `x`'s trigrams still hold; exact strings
  demote to prefixes/suffixes (a match may continue).
- **`x*`, `x?`**: may match empty — contributes nothing (degrades toward
  match-all rather than risk a wrong constraint).
- **Classes**: `[ab]` enumerates into the exact set; big classes (`\w`)
  become "any char".

Examples (`grix --explain` shows these):

```
Abcdef        →  "Abc" "bcd" "cde" "def"
abc.*def      →  "abc" "def"
abc|def       →  ("abc"|"def")
(abc)?def     →  "def"
a[0-9]z       →  ("a0z"|"a1z"|…|"a9z")
\w+           →  ALL          (scan everything — still correct)
```

The single invariant the planner must uphold: **it may only require
trigrams guaranteed to appear in every match.** Whenever the analysis
cannot guarantee anything it degrades to `ALL`, which means "scan every
file" — slower, never wrong. An over-constraining planner bug would
silently hide results, which is why the test suite's core property test
asserts `search-with-index ≡ full-scan` across every pattern shape the
planner handles, and why query minimization (subsumption pruning: in an OR,
a branch implied by a weaker branch is dropped) is implemented as pure set
logic that provably preserves semantics.

## The index file

One file per indexed root, in your cache directory (`%LOCALAPPDATA%\grix`
or `~/.cache/grix`) — repositories are never touched. Little-endian,
mmap-friendly:

```
[magic][header][root path][paths blob][file table][trigram table][postings]
```

- **file table**: fixed-width entries (path, size, mtime, flags) — the
  flags mark binary files (excluded) and oversized files (always scanned).
- **trigram table**: sorted fixed-width entries; a posting list is found by
  binary search and decoded lazily.
- **postings**: per-trigram sorted file ids, delta + LEB128 encoded.
  The linux kernel source (92,823 files, ~1.4 GB) indexes to 162 MiB with
  this scheme.

Every read is bounds-checked; a corrupted index produces an error (and a
rebuild hint), never undefined behavior.

## Incremental updates

`grix index` on an already-indexed tree:

1. Walk the tree, collect (path, size, mtime), sort by path.
2. Files whose (size, mtime) match the old index are **reused**: one linear
   pass over the old posting lists remaps their ids into the new file table
   — their bytes are never read again. The remap preserves sort order, so
   posting lists stay sorted by construction.
3. Only new/changed files are read and extracted (in parallel).

In practice this makes a refresh ~20× faster than the initial build on a
typical working tree.

## The confirming scan

Candidates are scanned with [`regex`](https://docs.rs/regex)'s `bytes` API
across a work-stealing thread pool. Match offsets are mapped to line
numbers in a single forward pass (the newline counter doubles as the
line-start anchor, so even pathological empty-match patterns stay linear).
Files over 8 MiB are mmap'd instead of read.

Output mirrors ripgrep: headings on a tty, `path:line:text` when piped,
`--json` for machines, exit codes 0/1/2 (match/no match/error).

## What grix does not do (yet)

- **Watch mode**: a daemon updating the index on file events, closing the
  freshness gap entirely.
- **Sub-file granularity**: posting lists reference whole files; very large
  uniform corpora would benefit from chunk-level postings.
- **Multiline patterns** (`-U`), context lines (`-A/-B/-C`), replacements.
