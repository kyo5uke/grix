use grix::{index, search, store};

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use index::build::{self, BuildOptions};
use index::format::IndexReader;
use search::{FileResult, Matcher, SearchOptions};

const USAGE: &str = "\
grix - grep with an index

USAGE:
    grix [OPTIONS] <PATTERN> [PATH...]  search (auto-indexes on first run);
                                        PATH... limits the search to those
                                        files/directories
    grix index [PATH]                  build or refresh the index
    grix status [PATH]                 show index info
    grix forget [PATH]                 delete the index

OPTIONS:
    -i              case-insensitive search
    -F              treat the pattern as a literal string
    -l              list matching files only
    -c              print per-file match counts
    -m <N>          stop after N matching lines per file
    -A <N>          show N lines of context after each match
    -B <N>          show N lines of context before each match
    -C <N>          show N lines of context before and after
    -g <GLOB>       only search files matching the glob (!GLOB to exclude)
    -t <TYPE>       only search files of TYPE (e.g. rust, py, js)
    -T <TYPE>       exclude files of TYPE
    --json          machine-readable output (one JSON object per line)
    --stats         print planner/index statistics after searching
    --explain       print the trigram query plan and exit
    --no-index      scan without using or building an index
    --no-auto-index fail instead of building a missing index
    --no-heading    grep-style path:line:text output
    --color <WHEN>  always | never | auto (default: auto)
    -h, --help      show this help
    -V, --version   show version
";

struct Cli {
    pattern: Option<String>,
    path: Option<PathBuf>,
    /// Extra path arguments for `search`: files/dirs to scope the search to.
    paths: Vec<PathBuf>,
    command: Cmd,
    case_insensitive: bool,
    fixed: bool,
    files_only: bool,
    counts: bool,
    max_count: Option<u64>,
    before: usize,
    after: usize,
    globs: Vec<String>,
    types_select: Vec<String>,
    types_negate: Vec<String>,
    json: bool,
    stats: bool,
    explain: bool,
    no_index: bool,
    no_auto_index: bool,
    no_heading: bool,
    color: ColorChoice,
}

#[derive(PartialEq)]
enum Cmd {
    Search,
    Index,
    Status,
    Forget,
}

#[derive(PartialEq, Clone, Copy)]
enum ColorChoice {
    Auto,
    Always,
    Never,
}

fn parse_args() -> Result<Cli, String> {
    let mut cli = Cli {
        pattern: None,
        path: None,
        paths: Vec::new(),
        command: Cmd::Search,
        case_insensitive: false,
        fixed: false,
        files_only: false,
        counts: false,
        max_count: None,
        before: 0,
        after: 0,
        globs: Vec::new(),
        types_select: Vec::new(),
        types_negate: Vec::new(),
        json: false,
        stats: false,
        explain: false,
        no_index: false,
        no_auto_index: false,
        no_heading: false,
        color: ColorChoice::Auto,
    };
    let mut args = std::env::args().skip(1).peekable();
    let mut positionals: Vec<String> = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("grix {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-i" => cli.case_insensitive = true,
            "-F" => cli.fixed = true,
            "-l" => cli.files_only = true,
            "-c" => cli.counts = true,
            "-m" => {
                let v = args.next().ok_or("-m needs a number")?;
                cli.max_count = Some(v.parse().map_err(|_| format!("bad -m value: {v}"))?);
            }
            "-A" | "-B" | "-C" => {
                let v = args.next().ok_or(format!("{arg} needs a number"))?;
                let n: usize = v.parse().map_err(|_| format!("bad {arg} value: {v}"))?;
                match arg.as_str() {
                    "-A" => cli.after = n,
                    "-B" => cli.before = n,
                    _ => {
                        cli.before = n;
                        cli.after = n;
                    }
                }
            }
            // grep-style attached form: -A3 / -B2 / -C1
            s if s.starts_with("-A") || s.starts_with("-B") || s.starts_with("-C") => {
                let n: usize = s[2..]
                    .parse()
                    .map_err(|_| format!("bad {} value: {}", &s[..2], &s[2..]))?;
                match &s[..2] {
                    "-A" => cli.after = n,
                    "-B" => cli.before = n,
                    _ => {
                        cli.before = n;
                        cli.after = n;
                    }
                }
            }
            "-g" => {
                let v = args.next().ok_or("-g needs a glob")?;
                cli.globs.push(v);
            }
            "-t" => {
                let v = args.next().ok_or("-t needs a type name")?;
                cli.types_select.push(v);
            }
            "-T" => {
                let v = args.next().ok_or("-T needs a type name")?;
                cli.types_negate.push(v);
            }
            "--json" => cli.json = true,
            "--stats" => cli.stats = true,
            "--explain" => cli.explain = true,
            "--no-index" => cli.no_index = true,
            "--no-auto-index" => cli.no_auto_index = true,
            "--no-heading" => cli.no_heading = true,
            "--color" => {
                let v = args.next().ok_or("--color needs always|never|auto")?;
                cli.color = match v.as_str() {
                    "always" => ColorChoice::Always,
                    "never" => ColorChoice::Never,
                    "auto" => ColorChoice::Auto,
                    other => return Err(format!("bad --color value: {other}")),
                };
            }
            s if s.starts_with('-') && s.len() > 1 && !positionals.is_empty() => {
                return Err(format!("unknown option: {s}"));
            }
            s if s.starts_with('-') && s.len() > 1 => {
                return Err(format!("unknown option: {s}"));
            }
            _ => positionals.push(arg),
        }
    }

    match positionals.first().map(String::as_str) {
        Some("index") => {
            cli.command = Cmd::Index;
            cli.path = positionals.get(1).map(PathBuf::from);
        }
        Some("status") => {
            cli.command = Cmd::Status;
            cli.path = positionals.get(1).map(PathBuf::from);
        }
        Some("forget") => {
            cli.command = Cmd::Forget;
            cli.path = positionals.get(1).map(PathBuf::from);
        }
        Some(_) => {
            cli.pattern = Some(positionals.remove(0));
            cli.paths = positionals.iter().map(PathBuf::from).collect();
        }
        None => return Err("missing pattern (try --help)".into()),
    }
    Ok(cli)
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn human_count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn cmd_index(path: Option<&Path>) -> Result<(), String> {
    let root = store::canonical_root(path.unwrap_or(Path::new(".")))
        .map_err(|e| format!("cannot resolve path: {e}"))?;
    let idx = store::index_path(&root).map_err(|e| e.to_string())?;
    if let Some(parent) = idx.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let old = IndexReader::open(&idx).ok();
    let t0 = Instant::now();
    let stats = build::build(&root, &idx, old.as_ref(), &BuildOptions::default())
        .map_err(|e| format!("index build failed: {e}"))?;
    let elapsed = t0.elapsed();
    let size = std::fs::metadata(&idx).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "indexed {} ({} files: {} indexed, {} reused, {} binary, {} too large) in {:.2}s -> {}",
        root.display(),
        human_count(stats.files_total),
        human_count(stats.files_indexed),
        human_count(stats.files_reused),
        human_count(stats.files_binary),
        human_count(stats.files_scan_always),
        elapsed.as_secs_f64(),
        human_bytes(size),
    );
    Ok(())
}

fn cmd_status(path: Option<&Path>) -> Result<(), String> {
    let start = path.unwrap_or(Path::new("."));
    match store::find_index_upward(start) {
        Some((idx, root)) => {
            let reader = IndexReader::open(&idx).map_err(|e| e.to_string())?;
            let size = std::fs::metadata(&idx).map(|m| m.len()).unwrap_or(0);
            println!("root:     {}", root.display());
            println!("index:    {}", idx.display());
            println!("files:    {}", human_count(reader.file_count()));
            println!("trigrams: {}", human_count(reader.trigram_count()));
            println!("size:     {}", human_bytes(size));
            Ok(())
        }
        None => {
            println!("no index found for {} (run: grix index)", start.display());
            Ok(())
        }
    }
}

fn cmd_forget(path: Option<&Path>) -> Result<(), String> {
    let start = path.unwrap_or(Path::new("."));
    match store::find_index_upward(start) {
        Some((idx, root)) => {
            std::fs::remove_file(&idx).map_err(|e| e.to_string())?;
            eprintln!("removed index for {}", root.display());
            Ok(())
        }
        None => {
            eprintln!("no index found for {}", start.display());
            Ok(())
        }
    }
}

struct Printer {
    color: bool,
    heading: bool,
    json: bool,
    files_only: bool,
    counts: bool,
    /// Context (-A/-B/-C) is active; groups get "--" dividers like grep.
    context: bool,
}

impl Printer {
    fn print(&self, results: &[FileResult]) -> std::io::Result<u64> {
        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());
        let mut total: u64 = 0;
        let mut first = true;
        for fr in results {
            if self.files_only {
                writeln!(out, "{}", fr.rel_path)?;
                total += 1;
                continue;
            }
            if self.counts {
                let n = fr.lines.iter().filter(|l| l.is_match).count();
                writeln!(out, "{}:{}", fr.rel_path, n)?;
                total += n as u64;
                continue;
            }
            if self.json {
                for line in fr.lines.iter().filter(|l| l.is_match) {
                    total += 1;
                    let text = String::from_utf8_lossy(&line.line);
                    write!(
                        out,
                        "{{\"path\":{},\"line\":{},\"text\":{},\"spans\":[",
                        json_str(&fr.rel_path),
                        line.line_number,
                        json_str(&text),
                    )?;
                    for (i, (s, e)) in line.spans.iter().enumerate() {
                        if i > 0 {
                            write!(out, ",")?;
                        }
                        write!(out, "[{s},{e}]")?;
                    }
                    writeln!(out, "]}}")?;
                }
                continue;
            }
            if self.heading {
                if !first {
                    writeln!(out)?;
                }
                if self.color {
                    writeln!(out, "\x1b[35m{}\x1b[0m", fr.rel_path)?;
                } else {
                    writeln!(out, "{}", fr.rel_path)?;
                }
            }
            // In no-heading mode grep divides every context group with "--",
            // including across files. In heading mode files are already
            // separated by a blank line + heading, so only intra-file gaps
            // get a divider.
            if self.context && !self.heading && !first {
                writeln!(out, "--")?;
            }
            first = false;
            let mut prev_line: Option<u64> = None;
            for line in &fr.lines {
                // With context on, a gap between emitted line numbers means a
                // separate group: print grep's "--" divider. Without context,
                // grep prints no divider between non-adjacent matches.
                if self.context {
                    if let Some(p) = prev_line {
                        if line.line_number > p + 1 {
                            writeln!(out, "--")?;
                        }
                    }
                }
                prev_line = Some(line.line_number);
                if line.is_match {
                    total += 1;
                }
                // grep convention: ':' after the locator for a match, '-' for
                // a context line.
                let sep = if line.is_match { ':' } else { '-' };
                let mut text: &[u8] = &line.line;
                if text.last() == Some(&b'\r') {
                    text = &text[..text.len() - 1];
                }
                if self.heading {
                    if self.color {
                        write!(out, "\x1b[32m{}\x1b[0m{sep}", line.line_number)?;
                    } else {
                        write!(out, "{}{sep}", line.line_number)?;
                    }
                } else if self.color {
                    write!(
                        out,
                        "\x1b[35m{}\x1b[0m{sep}\x1b[32m{}\x1b[0m{sep}",
                        fr.rel_path, line.line_number
                    )?;
                } else {
                    write!(out, "{}{sep}{}{sep}", fr.rel_path, line.line_number)?;
                }
                write_highlighted(&mut out, text, &line.spans, self.color)?;
                writeln!(out)?;
            }
        }
        out.flush()?;
        Ok(total)
    }
}

fn write_highlighted(
    out: &mut impl Write,
    text: &[u8],
    spans: &[(usize, usize)],
    color: bool,
) -> std::io::Result<()> {
    if !color || spans.is_empty() {
        out.write_all(text)?;
        return Ok(());
    }
    let mut pos = 0;
    for &(s, e) in spans {
        let (s, e) = (s.min(text.len()), e.min(text.len()));
        if s < pos {
            continue;
        }
        out.write_all(&text[pos..s])?;
        out.write_all(b"\x1b[30;43m")?; // highlighter-pen style: black on yellow
        out.write_all(&text[s..e])?;
        out.write_all(b"\x1b[0m")?;
        pos = e;
    }
    out.write_all(&text[pos..])?;
    Ok(())
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Turn `grix <pat> <path>...` arguments into scopes relative to the index
/// root. A path equal to the root yields no scope (whole tree).
fn paths_to_scopes(paths: &[PathBuf], root: &Path) -> Result<Vec<String>, String> {
    let mut scopes = Vec::new();
    for p in paths {
        let canon =
            store::canonical_root(p).map_err(|e| format!("cannot resolve {}: {e}", p.display()))?;
        if canon == root {
            continue; // searching the whole tree
        }
        let rel = canon.strip_prefix(root).map_err(|_| {
            format!(
                "{} is outside the indexed tree ({})",
                p.display(),
                root.display()
            )
        })?;
        let scope = rel.to_string_lossy().replace('\\', "/");
        if !scope.is_empty() {
            scopes.push(scope);
        }
    }
    Ok(scopes)
}

fn cmd_search(cli: &Cli) -> Result<ExitCode, String> {
    let pattern = cli.pattern.as_deref().expect("pattern checked in parse");
    // The index is anchored at the current directory; path arguments scope
    // the search *within* it rather than choosing a different index.
    let anchor = PathBuf::from(".");

    // Context is a feature of normal line output; -l (files) and -c (counts)
    // ignore it, matching grep/ripgrep.
    let want_context = !cli.files_only && !cli.counts && !cli.json;
    let opts = SearchOptions {
        case_insensitive: cli.case_insensitive,
        fixed_string: cli.fixed,
        matches_only: cli.files_only,
        max_count: cli.max_count,
        before: if want_context { cli.before } else { 0 },
        after: if want_context { cli.after } else { 0 },
        globs: cli.globs.clone(),
        types_select: cli.types_select.clone(),
        types_negate: cli.types_negate.clone(),
        ..Default::default()
    };
    let matcher: Matcher = search::compile(pattern, &opts).map_err(|e| e.to_string())?;

    if cli.explain {
        println!("{}", matcher.query.display());
        return Ok(ExitCode::SUCCESS);
    }

    let t0 = Instant::now();
    let (results, stats) = if cli.no_index {
        let root =
            store::canonical_root(&anchor).map_err(|e| format!("cannot resolve path: {e}"))?;
        let mut opts = opts.clone();
        opts.path_scopes = paths_to_scopes(&cli.paths, &root)?;
        search::search_walk(&root, &matcher, &opts).map_err(|e| e.to_string())?
    } else {
        // Find (or build) an index that covers the current directory.
        let found = store::find_index_upward(&anchor);
        let (idx, root) = match found {
            Some(pair) => pair,
            None => {
                if cli.no_auto_index {
                    return Err(
                        "no index covers the current directory (run: grix index, or pass --no-index)"
                            .to_string(),
                    );
                }
                let root = store::canonical_root(&anchor)
                    .map_err(|e| format!("cannot resolve path: {e}"))?;
                eprintln!(
                    "grix: no index for {} - building one (first run only)...",
                    root.display()
                );
                let idx = store::index_path(&root).map_err(|e| e.to_string())?;
                if let Some(parent) = idx.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                let t = Instant::now();
                let bstats = build::build(&root, &idx, None, &BuildOptions::default())
                    .map_err(|e| format!("index build failed: {e}"))?;
                eprintln!(
                    "grix: indexed {} files in {:.2}s",
                    human_count(bstats.files_total),
                    t.elapsed().as_secs_f64()
                );
                (idx, root)
            }
        };
        let reader = match IndexReader::open(&idx) {
            Ok(r) => r,
            Err(e) => {
                return Err(format!(
                    "cannot open index ({e}); run grix index to rebuild, or use --no-index"
                ));
            }
        };
        let mut opts = opts.clone();
        opts.path_scopes = paths_to_scopes(&cli.paths, &root)?;
        search::search_index(&reader, &root, &matcher, &opts).map_err(|e| e.to_string())?
    };
    let total_elapsed = t0.elapsed();

    let color = match cli.color {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => std::io::stdout().is_terminal(),
    };
    let printer = Printer {
        color,
        heading: !cli.no_heading && std::io::stdout().is_terminal() && !cli.json,
        json: cli.json,
        files_only: cli.files_only,
        counts: cli.counts,
        context: opts.before > 0 || opts.after > 0,
    };
    printer.print(&results).map_err(|e| e.to_string())?;

    if cli.stats {
        eprintln!();
        eprintln!("query plan:  {}", stats.query_display);
        if stats.files_in_index > 0 {
            eprintln!(
                "index:       {} files; candidates after planning: {} ({:.3}%)",
                human_count(stats.files_in_index),
                human_count(stats.candidates),
                100.0 * stats.candidates as f64 / stats.files_in_index.max(1) as f64
            );
        } else {
            eprintln!("candidates:  {} (full scan)", human_count(stats.candidates));
        }
        eprintln!(
            "scanned:     {} files; matched {} lines in {} files",
            human_count(stats.files_scanned),
            human_count(stats.lines_matched),
            human_count(stats.files_matched),
        );
        eprintln!(
            "timing:      postings {}µs · scan {}µs · total {:.1}ms",
            stats.lookup_micros,
            stats.scan_micros,
            total_elapsed.as_secs_f64() * 1e3,
        );
    }

    if results.is_empty() {
        Ok(ExitCode::from(1))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

fn main() -> ExitCode {
    let cli = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("grix: {e}");
            return ExitCode::from(2);
        }
    };
    let result = match cli.command {
        Cmd::Index => cmd_index(cli.path.as_deref()).map(|()| ExitCode::SUCCESS),
        Cmd::Status => cmd_status(cli.path.as_deref()).map(|()| ExitCode::SUCCESS),
        Cmd::Forget => cmd_forget(cli.path.as_deref()).map(|()| ExitCode::SUCCESS),
        Cmd::Search => cmd_search(&cli),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("grix: {e}");
            ExitCode::from(2)
        }
    }
}
