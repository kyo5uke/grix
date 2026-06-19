# grix

English | [日本語](README.md)

grix is a grep that uses a trigram index.

It indexes a directory tree once, then uses that index to narrow searches to a
small set of candidate files.
It runs the real regex on those candidates, so within the features it supports
it returns the same lines as ripgrep.

[![ci](https://github.com/kyo5uke/grix/actions/workflows/ci.yml/badge.svg)](https://github.com/kyo5uke/grix/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

![ripgrep 1.57s, grix 16.9ms on the linux kernel tree (92,823 files); identical matches](docs/bench-kernel.png)

## Why

ripgrep is fast, but it reads the files again on every search.
That is fine on a small repository, but on a large tree a single search can take
seconds.
With a cold cache or a network drive you wait even longer.

In real work you tend to search the same tree many times.
Refactoring, code review, and AI coding agents can run grep tens or hundreds of
times in one session.
Instead of reading every file each time, grix looks the candidates up in the
index.

It is not approximate search.
There is no embedding or semantic matching; the final step always runs the real
regex against the actual file contents.
If grix and ripgrep ever return different lines for a supported search, that is a
bug in grix.
There is a property test that checks indexed search and a full scan return the
same results, and the benchmark script stops if the two tools' match counts
differ.

## Usage

```bash
cargo install grix

cd your-repo
grix 'fn main'            # first run builds the index; every run refreshes it
grix 'fn main' src/       # limit the search to a directory or file
grix -C2 'fn main'        # show 2 lines of context around each match
grix -t rust 'fn main'    # filter by file type (or -g '*.rs')
grix --no-auto-index 'fn main'  # use the index as-is, no refresh (fastest, may be stale)
```

Each search refreshes the index first, so results are always up to date.
Unchanged files are not re-read (matched by size and mtime), so a refresh is
mostly just a directory walk.
For the fastest path that uses the existing index as-is, pass `--no-auto-index`.

No daemon, no config file.
Nothing to download, no model.

The index is stored under your cache directory.

* Windows: `%LOCALAPPDATA%\grix`
* Linux/macOS: `~/.cache/grix`

It never writes anything inside the repository.

## Benchmarks

Measured on the Linux kernel source v6.12.
The tree is 92,823 files, about 1.4GB.

The machine is Windows 11, NVMe, ripgrep 15.1.0.
Reproduce with [`bench/run.sh`](bench/run.sh).
Every pattern is checked for identical match counts between ripgrep and grix
before it is timed.
grix is timed with `--no-auto-index` (using the index as-is, without the
pre-search refresh). That is the query speed itself, and is also what a normal
search costs when the tree hasn't changed.

| pattern | matched lines | ripgrep | grix | speedup |
| --- | ---: | ---: | ---: | ---: |
| `PageTransHuge` (rare literal) | 5 | 2.31 s | 97 ms | 23.7× |
| `EXPORT_SYMBOL` (common literal) | 38,267 | 2.29 s | 195 ms | 11.7× |
| `static\s+int\s+\w+_probe` (regex) | 10,081 | 2.10 s | 288 ms | 7.3× |
| `spinlock` (`-i`) | 17,086 | 2.23 s | 223 ms | 10.0× |
| `zzqqxx_does_not_exist` (no match) | 0 | 2.09 s | 41 ms | 50.5× |

The index is 162 MiB.
The first build took about 26 seconds.
With a cold filesystem cache it took about 90 seconds.

An unchanged `grix index` takes about 2.4 seconds.
In that case it does not re-read any file contents.

There are Linux numbers too.
These are from a stock GitHub Actions runner with 4 cores.
The log is [public](https://github.com/kyo5uke/grix/actions/runs/27286573555).

| pattern | ripgrep | grix | speedup |
| --- | ---: | ---: | ---: |
| `PageTransHuge` (rare literal) | 338 ms | 7.6 ms | 44.6× |
| `EXPORT_SYMBOL` (common literal) | 355 ms | 63 ms | 5.6× |
| `static\s+int\s+\w+_probe` (regex) | 390 ms | 99 ms | 4.0× |
| `spinlock` (`-i`) | 409 ms | 71 ms | 5.8× |
| `zzqqxx_does_not_exist` (no match) | 335 ms | 7.6 ms | 44.0× |

Directory walks cost more on Windows, so the gap varies by environment.
Either way it helps a lot when you search a large tree repeatedly.

## How it works

A trigram is three consecutive bytes.
A file that contains `hello` must contain `hel`, `ell`, and `llo`.

So if you have an index from trigrams to the files that contain them, you can
narrow the candidates by intersecting a few sorted lists instead of reading a
gigabyte of files every time.

grix extracts trigram constraints from the regex pattern.

* `abc.*def` becomes `abc` AND `def`
* `abc|xyz` becomes `abc` OR `xyz`

This is based on Russ Cox's trigram planner for Google Code Search.

After narrowing with the index, grix runs the real regex against the current
contents of those files.
So an out-of-date index never shows a line that is not there.
It can only miss lines added since the last index update.

`grix --explain` shows the trigram plan for any pattern.
There is more detail in [ARCHITECTURE.md](ARCHITECTURE.md).

## ripgrep compatibility

The output format, exit codes, gitignore handling, binary detection, and line
semantics follow ripgrep.

The main supported forms are:

* default output: `path:line:text`
* heading style on a tty
* exit codes: 0 / 1 / 2
* `--json`
* `--color`

The flag set is still small.

| supported | not yet |
| --- | --- |
| `-i`, `-F`, `-l`, `-c`, `-m`, `-A`, `-B`, `-C`, `-g`, `-t`, `-T`, `--json`, `--no-heading`, `--color` | `-U`, `--replace` |

If grix and ripgrep disagree on matched lines within the supported set, please
open an issue.

## Using it with AI coding agents

AI coding agents run grep against the same tree over and over.
Pointing them at grix instead of reading every file each time cuts the search
wait noticeably.

`--json` returns one JSON object per match line.
`--stats` and `--explain` show the search cost and the trigram plan.

You can put something like this in the agent's instructions:

```text
Use `grix <pattern>` instead of grep / rg for code search.
```

Basic usage is close to ripgrep, so simple searches drop in directly.

## Prior art

* [Google Code Search](https://github.com/google/codesearch): Russ Cox's trigram planner is the basis for this project. Essay: [Regular Expression Matching with a Trigram Index](https://swtch.com/~rsc/regexp/regexp4.html)
* [zoekt](https://github.com/sourcegraph/zoekt): a server-side trigram search engine. grix aims at the local, zero-setup version of the idea.
* [qgrep](https://github.com/zeux/qgrep), [ugrep-indexer](https://github.com/Genivia/ugrep-indexer): earlier indexed grep CLIs, with different designs (archive-based, batch indexing).
* [ripgrep](https://github.com/BurntSushi/ripgrep): the search tool grix matches its output and behavior against.

## License

MIT
