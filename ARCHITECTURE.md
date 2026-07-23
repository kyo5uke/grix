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

One base file per indexed root, in your cache directory
(`%LOCALAPPDATA%\grix` or `~/.cache/grix`) — repositories are never
touched. Little-endian, mmap-friendly (format `GRIXIDX3`):

```
[magic][header][root path][paths blob][file table][scan-always ids]
[tombstones][trigram table][postings]
```

- **file table**: fixed-width entries (path, size, mtime, flags) — the
  flags mark binary files (excluded) and oversized files (always scanned).
- **scan-always ids**: the (usually tiny) list of oversized-file ids as its
  own section, so a query only ever touches its candidates' metadata — no
  per-search walk over the whole file table.
- **trigram table**: sorted fixed-width entries; a posting list is found by
  binary search and decoded lazily. Each list gets the cheapest of three
  encodings: delta + LEB128 varints for rare trigrams; a file-count-wide
  **bitmap** once more than 1/8 of all files contain the trigram (smaller
  than varints from there on, and byte-for-byte the same information — no
  narrowing power lost); and **dense** (document count only) once more
  than 3/4 contain it — a list covering nearly everything narrows nothing,
  so dropping its ids strictly weakens the constraint and can never drop
  results. Together this is ~20% smaller than varints alone.
- **postings**: per-trigram sorted file ids in the encodings above.
  The linux kernel source (92,823 files, ~1.4 GB) indexes to ~129 MiB.

The same format serves a second role: a small sidecar **overlay**
(`.gixo`) holding only what changed since the base was built, plus the
base ids it supersedes (the tombstones section). Base and overlay are tied
together by a build id in the header, so a stale overlay can never be
applied to the wrong base. Searches evaluate `(base − tombstones) ∪
overlay` through one view; ids stay disjoint by offsetting overlay ids
past the base's.

Older index versions are rejected on open with a version error; searches
then answer from a full scan while a rebuild runs in the background.

Every read is bounds-checked; a corrupted index produces an error (and a
rebuild hint), never undefined behavior.

## The build pipeline

Building is a sort, not a hash join:

1. The tree is walked **in parallel** (gitignore-aware); candidates are
   sorted by path so file ids are deterministic.
2. Worker threads read files and find each file's distinct trigrams with a
   reusable 2^24-bit bitmap — O(bytes), no per-file sorting — and emit
   packed `(trigram, file id)` u64 pairs in large batches.
3. Pairs accumulate under a memory budget; over budget the buffer is sorted
   once and spilled as a raw sorted run.
4. At write time the in-memory run, the spilled runs and (for incremental
   builds) the old index — streamed as one pre-sorted, id-remapped run —
   are k-way merged straight into the on-disk postings encoder.

No hash map or per-trigram allocation exists anywhere on this path, and
peak memory stays bounded no matter how large the tree is.

## Incremental updates

`grix index` on an already-indexed tree:

1. Walk the tree (parallel), collect (path, size, mtime), sort by path.
2. If nothing changed at all, stop — nothing is written.
3. Otherwise only the **overlay** is rewritten: changed/new files are read
   and extracted (in parallel), unchanged overlay entries stream out of the
   old overlay id-remapped, and superseded base ids become tombstones. The
   base index — usually >99% of the data — is not touched, so a refresh
   costs the walk plus the churn since the base was built, not the tree
   size (measured: ~0.5 s for a one-file change on the kernel tree, vs
   rewriting a 130 MiB index).
4. When the overlay outgrows its budget (`max(64, files/8)` entries or a
   comparable tombstone count), everything folds into a fresh base in one
   full build — which itself reuses postings from both old files — and the
   overlay is dropped. A compacted base is byte-identical to a
   from-scratch build (tested).

## First contact

A search with no index (or an old-format one) does not wait for a build:
it answers immediately from a parallel full scan — the same results, just
at grep speed — and spawns a detached `grix index` child. The child holds
the watch heartbeat while it builds, so concurrent searches neither
self-refresh nor spawn duplicate builders; the spawn happens after the
answer is printed so the scan's warmed file cache benefits the builder
instead of competing with it.

## The confirming scan

Candidates are scanned with [`regex`](https://docs.rs/regex)'s `bytes` API
across a work-stealing thread pool. The pool is deliberately oversubscribed
(4× cores): on Windows the dominant cost is opening files, which blocks its
thread, so extra threads overlap that syscall latency. Files are opened with
a sequential-access hint and read into buffers pre-sized from the index's
own metadata. Match offsets are mapped to line numbers in a single forward
pass (the newline counter doubles as the line-start anchor, so even
pathological empty-match patterns stay linear). Files over 8 MiB are mmap'd
instead of read.

Output mirrors ripgrep: headings on a tty, `path:line:text` when piped,
`--json` for machines, exit codes 0/1/2 (match/no match/error).

## Watch mode

By default a search refreshes the index first (a directory walk + stat; only
changed files are re-read). On a large tree that walk is the dominant cost, so
the `--no-auto-index` numbers above represent pure query speed but can be
stale. `grix watch` removes that tension:

1. It subscribes to filesystem events for the tree
   ([`notify`](https://docs.rs/notify): inotify / FSEvents / ReadDirectoryChangesW).
2. Events are filtered (gitignore + `.git`) so build churn like `target/`
   doesn't trigger work, then **debounced** — after ~400 ms of quiet it runs
   the same incremental build, re-reading only what changed.
3. It writes a heartbeat to a sidecar file next to the index. A normal
   `grix <pattern>` checks that heartbeat: if a watcher is alive it **skips its
   own refresh** and trusts the live index — instant *and* fresh.

The heartbeat is a freshness timestamp, not a lock: if the watcher crashes the
heartbeat goes stale within seconds and searches transparently resume
refreshing themselves. Nothing in the repository is touched.

## What grix does not do (yet)

- **Sub-file granularity**: posting lists reference whole files; very large
  uniform corpora would benefit from chunk-level postings.
- **Incremental on-disk index**: a watch reindex rewrites the whole index
  file. Cheap for typical repos; a true incremental format would help on
  giant trees.
- **Multiline patterns** (`-U`) and replacements.
