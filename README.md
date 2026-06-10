# grix

[![ci](https://github.com/kyo5uke/grix/actions/workflows/ci.yml/badge.svg)](https://github.com/kyo5uke/grix/actions/workflows/ci.yml)

grep with an index. grix builds a trigram index of your tree once, keeps it
fresh incrementally, and then answers regex searches in milliseconds where
a full scan takes seconds — with output that matches ripgrep line for line.

[日本語版 README](README.ja.md)

```
$ grix 'static\s+int\s+\w+_probe' .          # linux kernel source, 92k files
drivers/gpu/drm/bridge/sii902x.c
1101:static int sii902x_audio_codec_probe(struct platform_device *pdev)
...

$ grix 'static\s+int\s+\w+_probe' . --stats
query plan:  "_pr" "pro" "rob" "obe" ...
index:       92,823 files; candidates after planning: 8,489 (9.1%)
timing:      postings 2ms · scan 120ms · total 130ms
```

## Why

- **Big trees make grep slow.** ripgrep is extraordinary, but it must read
  every file every time. On a monorepo, every search costs seconds; on a
  cold cache or a network drive, much more. An index pays that cost once.
- **Repeated searches dominate real workflows.** Refactoring, code review,
  and especially AI coding agents — an agent session can easily run
  hundreds of greps over the same unchanged tree. grix turns each of those
  from a full scan into a few posting-list lookups.
- **Exact, not approximate.** This is not a semantic/embedding search:
  results are exactly what grep would print — same lines, same count —
  just faster. There is a test suite property enforcing that
  `search-with-index ≡ full-scan`, and the benchmark script refuses to time
  anything until both tools' outputs match.

## Quick start

```
cargo install grix

cd your-repo
grix 'fn main'            # first run builds the index automatically
grix 'fn main'            # subsequent runs answer in milliseconds
grix index                # refresh after pulling (incremental, ~seconds)
```

No daemon, no configuration, no model downloads. One binary. Indexes live
under your cache directory (`%LOCALAPPDATA%\grix`, `~/.cache/grix`) — your
repositories are never touched.

## Benchmarks

Linux kernel source v6.12 (92,823 files, ~1.4 GB), Windows 11, NVMe.
ripgrep 15.1.0. Reproduce with [`bench/run.sh`](bench/run.sh); every
pattern is parity-checked (identical matched-line counts) before timing.

| pattern | matched lines | ripgrep | grix | speedup |
|---|---:|---:|---:|---:|
| `PageTransHuge` (rare literal) | 5 | 2.31 s | 97 ms | 23.7× |
| `EXPORT_SYMBOL` (common literal) | 38,267 | 2.29 s | 195 ms | 11.7× |
| `static\s+int\s+\w+_probe` (regex) | 10,081 | 2.10 s | 288 ms | 7.3× |
| `spinlock` (`-i`) | 17,086 | 2.23 s | 223 ms | 10.0× |
| `zzqqxx_does_not_exist` (no match) | 0 | 2.09 s | 41 ms | 50.5× |

Index: 162 MiB, built once in ~26 s (≈90 s with a cold filesystem cache);
a refresh when nothing changed takes ~2.4 s and re-reads no file contents.
Note these timings are from Windows, where directory walks are expensive —
on Linux ripgrep's full scans are faster, so expect a smaller (but still
large) gap.

## How it works

A trigram is 3 consecutive bytes. `hello` cannot appear in a file that
lacks `hel`, `ell` or `llo` — so an index from trigrams to files lets grix
intersect a few sorted lists instead of reading a gigabyte. The interesting
part is doing this for *regexes*: grix analyzes the pattern into trigram
constraints (`abc.*def` → must contain `abc` AND `def`; `abc|xyz` → `abc`
OR `xyz`), following the algorithm Russ Cox described for Google Code
Search. Candidates are then confirmed by running the real regex over the
files' current contents.

That last point gives grix a useful guarantee: **results are never stale.**
Matches are read from the live files, so an out-of-date index can only
*miss* lines added since the last `grix index` — it can never show you a
line that is not there. `grix --explain` prints the trigram plan for any
pattern; [ARCHITECTURE.md](ARCHITECTURE.md) has the full story.

## ripgrep compatibility

Output format (`path:line:text`, headings on a tty), exit codes (0/1/2),
gitignore handling, binary detection and line semantics follow ripgrep.
The flag surface is intentionally small for now:

| supported | not yet |
|---|---|
| `-i`, `-F`, `-l`, `-c`, `-m`, `--json`, `--no-heading`, `--color` | `-A/-B/-C` context, `-U` multiline, `-g` globs, `-t` types, `--replace` |

If grix and ripgrep ever disagree on matched lines for a supported
pattern, that is a bug — please open an issue.

## For AI agents

Agents grep constantly, and they grep the same tree hundreds of times per
session. Point them at grix and each lookup is milliseconds:

- `--json` emits one JSON object per match line.
- `--stats`/`--explain` are useful when debugging what a pattern costs.
- A note in your agent instructions like *"use `grix <pattern>` instead of
  grep/rg for code search"* is enough — the CLI is argument-compatible for
  basic usage.

## Prior art

- [Google Code Search](https://github.com/google/codesearch) — Russ Cox's
  trigram planner is the foundation here ([essay](https://swtch.com/~rsc/regexp/regexp4.html)).
- [zoekt](https://github.com/sourcegraph/zoekt) — trigram search as a
  server; grix is the local, zero-setup take.
- [qgrep](https://github.com/zeux/qgrep), [ugrep-indexer](https://github.com/Genivia/ugrep-indexer)
  — earlier indexed-grep CLIs, different tradeoffs (archives / batch
  indexing).
- [ripgrep](https://github.com/BurntSushi/ripgrep) — the bar for what a
  search tool should feel like, and the scan engine grix has to agree with.

## License

MIT
