use grix::{index, mcp, search, store, watch};

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
    grix [OPTIONS] <PATTERN> [PATH...]  search (auto-indexes, and refreshes
                                        the index, on each run); PATH... limits
                                        the search to those files/directories
    grix index [PATH]                  build or refresh the index
    grix watch [PATH]                  keep the index fresh in the background
                                        (searches then stay instant + current)
    grix mcp                           run an MCP server (code search for AI
                                        coding agents); see README
    grix status [PATH]                 show index info
    grix forget [PATH]                 delete the index

OPTIONS:
    -i              case-insensitive search
    -F              treat the pattern as a literal string
    -e <PATTERN>    use PATTERN; repeat to match any of several patterns
                    (also needed for patterns starting with '-')
    --              end of options; everything after is pattern/paths
    -U              multiline: matches may span lines (shows every line touched)
    -r <TEXT>       replace matches with TEXT in the output ($1/$name refs;
                    files are never modified)
    -w              only match surrounded by word boundaries
    -v              invert: print non-matching lines (cannot use the index)
    -o              print each match on its own line instead of the whole line
    -S              smart case: insensitive unless the pattern has uppercase
    --hidden        search hidden files too (.dotfiles; indexed either way)
    --no-ignore, -u ignore the ignore rules; searches via a full scan
    -uu             --no-ignore plus --hidden
    -l              list matching files only
    --files         list every file the index covers (no pattern; instant)
    --type-list     show all file types usable with -t / -T
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
    --no-auto-index use the existing index as-is: skip the pre-search
                    refresh and never build a missing one (fastest, but
                    results can be stale)
    --no-heading    grep-style path:line:text output
    --color <WHEN>  always | never | auto (default: auto)
    -h, --help      show this help
    -V, --version   show version
";

struct Cli {
    /// Search patterns; more than one (repeated -e) matches like rg: any of
    /// them. Combined into one alternation before compiling.
    patterns: Vec<String>,
    path: Option<PathBuf>,
    /// Extra path arguments for `search`: files/dirs to scope the search to.
    paths: Vec<PathBuf>,
    command: Cmd,
    case_insensitive: bool,
    fixed: bool,
    multiline: bool,
    replace: Option<String>,
    word: bool,
    invert: bool,
    only_matching: bool,
    smart_case: bool,
    files_list: bool,
    type_list: bool,
    hidden: bool,
    no_ignore: bool,
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
    Watch,
    Mcp,
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
        patterns: Vec::new(),
        path: None,
        paths: Vec::new(),
        command: Cmd::Search,
        case_insensitive: false,
        fixed: false,
        multiline: false,
        replace: None,
        word: false,
        invert: false,
        only_matching: false,
        smart_case: false,
        files_list: false,
        type_list: false,
        hidden: false,
        no_ignore: false,
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
    let mut e_patterns: Vec<String> = Vec::new();
    // A subcommand word is only recognized in the very first argument slot.
    // Once any option (or `--`) precedes it, a leading positional is a
    // pattern, so `grix -F index` searches for "index" instead of indexing.
    let mut no_subcommand = false;
    while let Some(arg) = args.next() {
        let positionals_before = positionals.len();
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
            "-U" | "--multiline" => cli.multiline = true,
            "-r" | "--replace" => {
                let v = args.next().ok_or("-r needs replacement text")?;
                cli.replace = Some(v);
            }
            "--files" => cli.files_list = true,
            "--type-list" => cli.type_list = true,
            "--hidden" => cli.hidden = true,
            "--no-ignore" | "-u" => cli.no_ignore = true,
            "-uu" => {
                cli.no_ignore = true;
                cli.hidden = true;
            }
            "-uuu" => {
                return Err(
                    "-uuu (searching binary files) is not supported; the closest is -uu".into(),
                );
            }
            "-w" | "--word-regexp" => cli.word = true,
            "-v" | "--invert-match" => cli.invert = true,
            "-o" | "--only-matching" => cli.only_matching = true,
            "-S" | "--smart-case" => cli.smart_case = true,
            "-e" => {
                let v = args.next().ok_or("-e needs a pattern")?;
                e_patterns.push(v);
            }
            "--" => {
                if positionals.is_empty() {
                    no_subcommand = true;
                }
                positionals.extend(&mut args);
                break;
            }
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
            s if s.starts_with('-') && s.len() > 1 => {
                return Err(format!("unknown option: {s}"));
            }
            _ => positionals.push(arg),
        }
        if positionals.len() == positionals_before && positionals_before == 0 {
            no_subcommand = true; // an option came before any positional
        }
    }

    if !e_patterns.is_empty() || no_subcommand {
        // --files / --type-list need no pattern; positionals are all paths.
        if e_patterns.is_empty() && !cli.files_list && !cli.type_list {
            if positionals.is_empty() {
                return Err("missing pattern (try --help)".into());
            }
            e_patterns.push(positionals.remove(0));
        }
        cli.patterns = e_patterns;
        cli.paths = positionals.iter().map(PathBuf::from).collect();
        return Ok(cli);
    }

    match positionals.first().map(String::as_str) {
        Some("index") => {
            cli.command = Cmd::Index;
            cli.path = positionals.get(1).map(PathBuf::from);
        }
        Some("watch") => {
            cli.command = Cmd::Watch;
            cli.path = positionals.get(1).map(PathBuf::from);
        }
        Some("mcp") => {
            cli.command = Cmd::Mcp;
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
            cli.patterns = vec![positionals.remove(0)];
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
    // As a detached background builder (spawned by a first search), keep the
    // watch heartbeat fresh so concurrent searches neither self-refresh nor
    // spawn duplicate builders while this build runs.
    let hb = std::env::var_os("GRIX_BUILD_HEARTBEAT")
        .map(|_| store::start_heartbeat(&idx, std::time::Duration::from_secs(5)));
    let t0 = Instant::now();
    let result = build::build(&root, &idx, &BuildOptions::default())
        .map_err(|e| format!("index build failed: {e}"));
    if let Some(hb) = hb {
        hb.stop_and_clear();
    }
    let stats = result?;
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

/// Claim the freshness marker for a background build. Returns false when a
/// watcher or another builder already owns it (then nothing should spawn).
fn claim_background_index(idx: &Path) -> bool {
    if store::watcher_is_live(idx) {
        return false;
    }
    let _ = store::write_watch_heartbeat(idx);
    true
}

/// Fire-and-forget `grix index <root>` in a detached child. Called *after*
/// the walk-scan answer has been printed: the walk warms the filesystem
/// cache, so the builder rides it instead of fighting the search for cold
/// reads. The claim from `claim_background_index` keeps racing searches off
/// in between.
fn spawn_background_index(idx: &Path, root: &Path) {
    if store::watcher_is_live_other(idx) {
        return; // someone else took over while we were scanning
    }
    let Ok(exe) = std::env::current_exe() else {
        store::remove_watch_marker(idx);
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("index")
        .arg(root)
        .env("GRIX_BUILD_HEARTBEAT", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    match cmd.spawn() {
        // Deliberately not waited on: the child outlives this process (on
        // unix the eventual zombie is reaped when we exit).
        Ok(child) => std::mem::forget(child),
        // Could not start it: clear the claim so searches self-refresh.
        Err(_) => store::remove_watch_marker(idx),
    }
}

fn cmd_watch(path: Option<&Path>) -> Result<(), String> {
    let root = store::canonical_root(path.unwrap_or(Path::new(".")))
        .map_err(|e| format!("cannot resolve path: {e}"))?;
    let idx = store::index_path(&root).map_err(|e| e.to_string())?;
    watch::run(&root, &idx, &BuildOptions::default()).map_err(|e| format!("watch failed: {e}"))
}

fn cmd_status(path: Option<&Path>) -> Result<(), String> {
    let start = path.unwrap_or(Path::new("."));
    match store::find_index_upward(start) {
        Some((idx, root)) => {
            if let Err(grix::index::format::IndexError::WrongVersion(v)) = IndexReader::open(&idx) {
                println!("root:     {}", root.display());
                println!("index:    {} (old format v{v})", idx.display());
                println!(
                    "          the next search rebuilds it automatically (or run: grix index)"
                );
                return Ok(());
            }
            let reader = IndexReader::open(&idx).map_err(|e| e.to_string())?;
            let size = std::fs::metadata(&idx).map(|m| m.len()).unwrap_or(0);
            println!("root:     {}", root.display());
            println!("index:    {}", idx.display());
            println!("files:    {}", human_count(reader.file_count()));
            println!("trigrams: {}", human_count(reader.trigram_count()));
            println!("size:     {}", human_bytes(size));
            let opath = grix::index::format::overlay_path(&idx);
            if let Ok(o) = IndexReader::open(&opath) {
                if o.index_ids().parent_id == reader.index_ids().build_id {
                    let osize = std::fs::metadata(&opath).map(|m| m.len()).unwrap_or(0);
                    println!(
                        "overlay:  {} changed files, {} superseded ({}) since the base was built",
                        human_count(o.file_count()),
                        human_count(o.tombstones().count()),
                        human_bytes(osize),
                    );
                }
            }
            println!(
                "watch:    {}",
                if store::watcher_is_live(&idx) {
                    "live (index kept fresh in the background)"
                } else {
                    "off (searches refresh the index themselves)"
                }
            );
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
            let _ = std::fs::remove_file(grix::index::format::overlay_path(&idx));
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
    /// -U/-o: -c counts matches, not matched lines.
    count_matches: bool,
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
                let n = if self.count_matches {
                    fr.lines.iter().map(|l| l.starts as usize).sum()
                } else {
                    fr.lines.iter().filter(|l| l.is_match).count()
                };
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

/// `--type-list`: the type definitions -t / -T accept, like rg.
fn cmd_type_list() -> Result<ExitCode, String> {
    let mut b = ignore::types::TypesBuilder::new();
    b.add_defaults();
    let types = b.build().map_err(|e| e.to_string())?;
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for def in types.definitions() {
        let _ = writeln!(out, "{}: {}", def.name(), def.globs().join(", "));
    }
    let _ = out.flush();
    Ok(ExitCode::SUCCESS)
}

/// `--files`: list every file a search would consider. With an index this
/// is a table read — no directory walk at all.
fn cmd_files(cli: &Cli) -> Result<ExitCode, String> {
    let opts = SearchOptions {
        globs: cli.globs.clone(),
        types_select: cli.types_select.clone(),
        types_negate: cli.types_negate.clone(),
        ..Default::default()
    };
    let filter = search::FileFilter::build(&opts).map_err(|e| e.to_string())?;
    let anchor = PathBuf::from(".");

    let walk_list = |root: &Path, scopes: &[String]| -> Result<Vec<String>, String> {
        let mut names = Vec::new();
        for c in build::collect_candidates(root, opts.threads, cli.no_ignore)
            .map_err(|e| e.to_string())?
        {
            if c.hidden && !cli.hidden {
                continue;
            }
            if search::in_scope(&c.rel_path, scopes) && filter.accept(&c.rel_path) {
                names.push(c.rel_path);
            }
        }
        Ok(names)
    };

    let mut names: Vec<String>;
    if cli.no_index || cli.no_ignore {
        let root =
            store::canonical_root(&anchor).map_err(|e| format!("cannot resolve path: {e}"))?;
        let scopes = paths_to_scopes(&cli.paths, &root)?;
        names = walk_list(&root, &scopes)?;
    } else {
        match store::find_index_upward(&anchor) {
            Some((idx, root)) => {
                let watcher_live = store::watcher_is_live(&idx);
                let usable = IndexReader::open(&idx).is_ok();
                let scopes = paths_to_scopes(&cli.paths, &root)?;
                if !usable && !cli.no_auto_index {
                    // Old format: rebuild in the background, list via a
                    // walk now (metadata only, so no cold-read contention).
                    if claim_background_index(&idx) {
                        spawn_background_index(&idx, &root);
                    }
                    names = walk_list(&root, &scopes)?;
                } else {
                    if usable && !cli.no_auto_index && !watcher_live {
                        if let Err(e) = build::build(&root, &idx, &BuildOptions::default()) {
                            eprintln!("grix: index refresh skipped ({e})");
                        }
                    }
                    let reader = IndexReader::open(&idx).map_err(|e| {
                        format!("cannot open index ({e}); run grix index, or use --no-index")
                    })?;
                    let overlay = IndexReader::open(&grix::index::format::overlay_path(&idx))
                        .ok()
                        .filter(|o| o.index_ids().parent_id == reader.index_ids().build_id);
                    let view = search::View::new(&reader, overlay.as_ref());
                    names = Vec::with_capacity(view.file_count());
                    for id in view.all_ids() {
                        let meta = view.file(id).map_err(|e| e.to_string())?;
                        if meta.flags & grix::index::format::FLAG_HIDDEN != 0 && !cli.hidden {
                            continue;
                        }
                        if search::in_scope(meta.rel_path, &scopes) && filter.accept(meta.rel_path)
                        {
                            names.push(meta.rel_path.to_string());
                        }
                    }
                }
            }
            None => {
                if cli.no_auto_index {
                    return Err(
                        "no index covers the current directory (run: grix index, or pass --no-index)"
                            .to_string(),
                    );
                }
                let root = store::canonical_root(&anchor)
                    .map_err(|e| format!("cannot resolve path: {e}"))?;
                let idx = store::index_path(&root).map_err(|e| e.to_string())?;
                if let Some(parent) = idx.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                if claim_background_index(&idx) {
                    spawn_background_index(&idx, &root);
                }
                let scopes = paths_to_scopes(&cli.paths, &root)?;
                names = walk_list(&root, &scopes)?;
            }
        }
    }

    names.sort_unstable();
    let empty = names.is_empty();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for n in &names {
        if writeln!(out, "{n}").is_err() {
            break; // downstream closed the pipe
        }
    }
    let _ = out.flush();
    Ok(if empty {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn cmd_search(cli: &Cli) -> Result<ExitCode, String> {
    if cli.type_list {
        return cmd_type_list();
    }
    if cli.files_list {
        return cmd_files(cli);
    }
    // Repeated -e patterns match like rg: any of them. Fixed strings are
    // escaped here so the alternation itself stays a regex.
    let mut fixed = cli.fixed;
    let pattern_owned = match cli.patterns.len() {
        0 => return Err("missing pattern (try --help)".into()),
        1 => cli.patterns[0].clone(),
        _ => {
            let parts: Vec<String> = if cli.fixed {
                fixed = false;
                cli.patterns
                    .iter()
                    .map(|p| regex_syntax::escape(p))
                    .collect()
            } else {
                cli.patterns.iter().map(|p| format!("(?:{p})")).collect()
            };
            parts.join("|")
        }
    };
    let pattern = pattern_owned.as_str();
    // The index is anchored at the current directory; path arguments scope
    // the search *within* it rather than choosing a different index.
    let anchor = PathBuf::from(".");

    // Context is a feature of normal line output; -l (files), -c (counts)
    // and -o (match-only rows) ignore it, matching grep/ripgrep.
    let want_context = !cli.files_only && !cli.counts && !cli.json && !cli.only_matching;
    let opts = SearchOptions {
        case_insensitive: cli.case_insensitive,
        fixed_string: fixed,
        matches_only: cli.files_only,
        multiline: cli.multiline,
        replace: cli.replace.clone().map(String::into_bytes),
        word: cli.word,
        invert: cli.invert,
        only_matching: cli.only_matching,
        smart_case: cli.smart_case,
        hidden: cli.hidden,
        no_ignore: cli.no_ignore,
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
    // Set when a live `grix watch` daemon owns freshness for this index, so we
    // neither refresh nor warn about staleness below.
    let mut watcher_live = false;
    // Deferred background build: spawned after results are printed, so the
    // walk-scan and the builder don't fight over a cold filesystem cache.
    let mut background_index: Option<(PathBuf, PathBuf)> = None;
    // --no-ignore searches files the index deliberately does not cover
    // (gitignored build output etc.), so it runs as a walk-scan like
    // --no-index.
    let (results, stats) = if cli.no_index || cli.no_ignore {
        let root =
            store::canonical_root(&anchor).map_err(|e| format!("cannot resolve path: {e}"))?;
        let mut opts = opts.clone();
        opts.path_scopes = paths_to_scopes(&cli.paths, &root)?;
        search::search_walk(&root, &matcher, &opts).map_err(|e| e.to_string())?
    } else {
        match store::find_index_upward(&anchor) {
            Some((idx, root)) => {
                watcher_live = store::watcher_is_live(&idx);
                let usable = IndexReader::open(&idx).is_ok();
                if !usable && !cli.no_auto_index {
                    // Old format or corrupt: rebuild in a detached child and
                    // answer this search from a full walk right now — the
                    // user never waits on a from-scratch build.
                    if claim_background_index(&idx) {
                        background_index = Some((idx.clone(), root.clone()));
                    }
                    eprintln!(
                        "grix: index needs a rebuild - this search runs as a full scan; rebuilding in the background..."
                    );
                    let mut opts = opts.clone();
                    opts.path_scopes = paths_to_scopes(&cli.paths, &root)?;
                    search::search_walk(&root, &matcher, &opts).map_err(|e| e.to_string())?
                } else {
                    // Keep the index current: a quick incremental refresh
                    // before searching so files added/changed since the last
                    // build are picked up. Changes land in the overlay, so
                    // this costs a walk + the churn since the base was
                    // built. Skipped with --no-auto-index, or when a `grix
                    // watch` daemon (or background builder) owns freshness.
                    if usable && !cli.no_auto_index && !watcher_live {
                        let t = Instant::now();
                        match build::build(&root, &idx, &BuildOptions::default()) {
                            Ok(bstats) if cli.stats => eprintln!(
                                "refresh:     {} changed/new, {} reused in {:.2}s",
                                human_count(bstats.files_extracted),
                                human_count(bstats.files_reused),
                                t.elapsed().as_secs_f64()
                            ),
                            Ok(_) => {}
                            // A failed refresh is non-fatal: fall back to the
                            // existing index rather than abort the search.
                            Err(e) => eprintln!("grix: index refresh skipped ({e})"),
                        }
                    }
                    let reader = match IndexReader::open(&idx) {
                        Ok(r) => r,
                        Err(e) => {
                            return Err(format!(
                                "cannot open index ({e}); run grix index to rebuild, or use --no-index"
                            ));
                        }
                    };
                    // Changes since the base was built live in the sidecar overlay.
                    let overlay = IndexReader::open(&grix::index::format::overlay_path(&idx))
                        .ok()
                        .filter(|o| o.index_ids().parent_id == reader.index_ids().build_id);
                    let view = search::View::new(&reader, overlay.as_ref());
                    let mut opts = opts.clone();
                    opts.path_scopes = paths_to_scopes(&cli.paths, &root)?;
                    search::search_index(&view, &root, &matcher, &opts)
                        .map_err(|e| e.to_string())?
                }
            }
            None => {
                if cli.no_auto_index {
                    return Err(
                        "no index covers the current directory (run: grix index, or pass --no-index)"
                            .to_string(),
                    );
                }
                let root = store::canonical_root(&anchor)
                    .map_err(|e| format!("cannot resolve path: {e}"))?;
                let idx = store::index_path(&root).map_err(|e| e.to_string())?;
                if let Some(parent) = idx.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                // First contact with this tree: answer from a full walk now,
                // build the index in a detached child. Search results are
                // identical either way; only the speed differs until the
                // index lands.
                if claim_background_index(&idx) {
                    background_index = Some((idx.clone(), root.clone()));
                }
                eprintln!(
                    "grix: no index for {} - this search runs as a full scan; building the index in the background...",
                    root.display()
                );
                let mut opts = opts.clone();
                opts.path_scopes = paths_to_scopes(&cli.paths, &root)?;
                search::search_walk(&root, &matcher, &opts).map_err(|e| e.to_string())?
            }
        }
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
        // -v is line output again, so it counts lines even under -U/-o.
        count_matches: (cli.multiline || cli.only_matching) && !cli.invert,
    };
    if let Err(e) = printer.print(&results) {
        // Downstream closed the pipe (e.g. `grix foo | head`): finish
        // quietly with the normal exit status, like grep/ripgrep.
        if e.kind() != std::io::ErrorKind::BrokenPipe {
            return Err(e.to_string());
        }
    }

    // The answer is out; now start the deferred background build (the walk
    // above just warmed the filesystem cache for it).
    if let Some((idx, root)) = &background_index {
        spawn_background_index(idx, root);
    }

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
        // With --no-auto-index we used the index as-is; a 0-result here might
        // just mean the index is stale. Say so instead of looking like a
        // definitive "not found". (A live watcher keeps it fresh, so skip the
        // hint then.)
        if cli.no_auto_index && !cli.no_index && !watcher_live && stats.files_in_index > 0 {
            eprintln!(
                "grix: no matches (index used as-is via --no-auto-index; it may be stale - run `grix index` or drop the flag to auto-refresh)"
            );
        }
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
        Cmd::Watch => cmd_watch(cli.path.as_deref()).map(|()| ExitCode::SUCCESS),
        Cmd::Mcp => mcp::run()
            .map(|()| ExitCode::SUCCESS)
            .map_err(|e| format!("mcp server failed: {e}")),
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
