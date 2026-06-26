//! `grix mcp`: a Model Context Protocol server over stdio.
//!
//! Exposes grix's exact code search as first-class MCP tools so coding agents
//! (Claude Code, Cursor, Windsurf, …) can call it directly instead of shelling
//! out to grep/rg and parsing text. While the server runs it keeps the index
//! fresh in the background (a watcher thread), so an agent's repeated searches
//! over the same tree are all instant *and* current.
//!
//! Transport: newline-delimited JSON-RPC 2.0. stdout carries protocol messages
//! ONLY — everything human-readable goes to stderr.

use std::io::{self, BufRead, Write};
use std::path::Path;

use serde_json::{json, Value};

use crate::index::build::BuildOptions;
use crate::index::format::IndexReader;
use crate::search::{self, FileResult, SearchOptions};
use crate::{store, watch};

const DEFAULT_PROTOCOL: &str = "2025-06-18";
const DEFAULT_MAX_RESULTS: usize = 200;

pub fn run() -> io::Result<()> {
    let root = store::canonical_root(Path::new("."))
        .map_err(|e| io::Error::other(format!("cannot resolve working directory: {e}")))?;
    let idx = store::index_path(&root)?;
    if let Some(p) = idx.parent() {
        std::fs::create_dir_all(p)?;
    }

    // Build and keep the index fresh in a background thread so the MCP
    // handshake stays instant even on a large repo (a synchronous build here
    // would block `initialize`). Until the first build lands, searches fall
    // back to a full walk — correct, just not yet index-fast.
    {
        let (root2, idx2) = (root.clone(), idx.clone());
        std::thread::spawn(move || {
            let _ = watch::run(&root2, &idx2, &BuildOptions::default());
        });
    }
    eprintln!(
        "grix mcp: serving {} (index kept fresh in the background)",
        root.display()
    );

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut reader = stdin.lock();
    let mut buf = String::new();
    loop {
        buf.clear();
        if reader.read_line(&mut buf)? == 0 {
            break; // EOF: client disconnected
        }
        let line = buf.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(line) else {
            continue; // ignore non-JSON lines
        };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let outcome = handle(method, req.get("params"), &root, &idx);
        // Requests (with an id) get a reply; notifications do not.
        if let Some(id) = id {
            let msg = match outcome {
                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err((code, message)) => {
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
                }
            };
            writeln!(out, "{msg}")?;
            out.flush()?;
        }
    }

    store::remove_watch_marker(&idx);
    Ok(())
}

type RpcResult = Result<Value, (i64, String)>;

fn handle(method: &str, params: Option<&Value>, root: &Path, idx: &Path) -> RpcResult {
    match method {
        "initialize" => {
            let protocol = params
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_PROTOCOL);
            Ok(json!({
                "protocolVersion": protocol,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "grix", "version": env!("CARGO_PKG_VERSION")},
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tool_schemas()})),
        "tools/call" => tools_call(params, root, idx),
        // notifications/initialized and other notifications: nothing to reply.
        "notifications/initialized" => Ok(json!({})),
        _ => Err((-32601, format!("method not found: {method}"))),
    }
}

fn tool_schemas() -> Value {
    json!([
        {
            "name": "code_search",
            "description": "Search the codebase for a regular expression. Exact, not semantic: returns the same lines ripgrep would, answered from a live trigram index. Prefer this over grep/rg for fast, repeatable code search. Output is `path:line:text` per match.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "regular expression to search for"},
                    "path": {"type": "string", "description": "optional file or directory to limit the search to"},
                    "glob": {"type": "string", "description": "optional glob filter, e.g. *.rs (prefix ! to exclude)"},
                    "type": {"type": "string", "description": "optional file type, e.g. rust, py, js"},
                    "ignore_case": {"type": "boolean", "description": "case-insensitive search"},
                    "fixed": {"type": "boolean", "description": "treat the pattern as a literal string"},
                    "context": {"type": "integer", "description": "lines of context to show around each match"},
                    "max_results": {"type": "integer", "description": "cap on matched lines returned (default 200)"}
                },
                "required": ["pattern"]
            }
        },
        {
            "name": "list_matching_files",
            "description": "List the files that contain a pattern, without the matching lines. Cheap reconnaissance before a full search (like `grep -l`).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "regular expression to search for"},
                    "path": {"type": "string", "description": "optional file or directory to limit the search to"},
                    "glob": {"type": "string", "description": "optional glob filter, e.g. *.rs (prefix ! to exclude)"},
                    "type": {"type": "string", "description": "optional file type, e.g. rust, py, js"},
                    "ignore_case": {"type": "boolean", "description": "case-insensitive search"},
                    "fixed": {"type": "boolean", "description": "treat the pattern as a literal string"}
                },
                "required": ["pattern"]
            }
        }
    ])
}

fn tools_call(params: Option<&Value>, root: &Path, idx: &Path) -> RpcResult {
    let params = params.ok_or((-32602, "missing params".to_string()))?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((-32602, "missing tool name".to_string()))?;
    let empty = json!({});
    let args = params.get("arguments").unwrap_or(&empty);

    let result = match name {
        "code_search" => code_search(args, root, idx),
        "list_matching_files" => list_matching_files(args, root, idx),
        other => return Err((-32602, format!("unknown tool: {other}"))),
    };
    // Tool-execution problems (e.g. a bad pattern) are reported as content with
    // isError=true, not as JSON-RPC protocol errors.
    Ok(match result {
        Ok(text) => tool_text(text, false),
        Err(e) => tool_text(e, true),
    })
}

fn tool_text(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

fn require_pattern(args: &Value) -> Result<&str, String> {
    args.get("pattern")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| "missing required argument: pattern".to_string())
}

fn build_opts(args: &Value, root: &Path) -> SearchOptions {
    let mut o = SearchOptions {
        case_insensitive: args
            .get("ignore_case")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        fixed_string: args.get("fixed").and_then(Value::as_bool).unwrap_or(false),
        ..Default::default()
    };
    let ctx = args.get("context").and_then(Value::as_u64).unwrap_or(0) as usize;
    o.before = ctx;
    o.after = ctx;
    if let Some(g) = args.get("glob").and_then(Value::as_str) {
        o.globs.push(g.to_string());
    }
    if let Some(t) = args.get("type").and_then(Value::as_str) {
        o.types_select.push(t.to_string());
    }
    if let Some(p) = args.get("path").and_then(Value::as_str) {
        if let Some(scope) = path_to_scope(p, root) {
            o.path_scopes.push(scope);
        }
    }
    o
}

/// A path argument relative to the project root → an index scope, or None for
/// "the whole tree" / a path outside the root.
fn path_to_scope(p: &str, root: &Path) -> Option<String> {
    let canon = store::canonical_root(Path::new(p)).ok()?;
    if canon == *root {
        return None;
    }
    let rel = canon.strip_prefix(root).ok()?;
    let s = rel.to_string_lossy().replace('\\', "/");
    (!s.is_empty()).then_some(s)
}

fn do_search(
    root: &Path,
    idx: &Path,
    matcher: &search::Matcher,
    opts: &SearchOptions,
) -> Result<Vec<FileResult>, String> {
    match IndexReader::open(idx) {
        Ok(reader) => search::search_index(&reader, root, matcher, opts)
            .map(|(r, _)| r)
            .map_err(|e| e.to_string()),
        Err(_) => search::search_walk(root, matcher, opts)
            .map(|(r, _)| r)
            .map_err(|e| e.to_string()),
    }
}

fn code_search(args: &Value, root: &Path, idx: &Path) -> Result<String, String> {
    let pattern = require_pattern(args)?;
    let max = args
        .get("max_results")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .max(1);
    let opts = build_opts(args, root);
    let matcher = search::compile(pattern, &opts).map_err(|e| e.to_string())?;
    let results = do_search(root, idx, &matcher, &opts)?;
    Ok(format_matches(&results, max))
}

fn list_matching_files(args: &Value, root: &Path, idx: &Path) -> Result<String, String> {
    let pattern = require_pattern(args)?;
    let mut opts = build_opts(args, root);
    opts.matches_only = true;
    let matcher = search::compile(pattern, &opts).map_err(|e| e.to_string())?;
    let results = do_search(root, idx, &matcher, &opts)?;
    if results.is_empty() {
        return Ok("No files match.".to_string());
    }
    let mut out = String::new();
    for fr in &results {
        out.push_str(&fr.rel_path);
        out.push('\n');
    }
    let n = results.len();
    out.push_str(&format!("\n{n} file{}", if n == 1 { "" } else { "s" }));
    Ok(out)
}

fn format_matches(results: &[FileResult], max: usize) -> String {
    let total: usize = results
        .iter()
        .map(|r| r.lines.iter().filter(|l| l.is_match).count())
        .sum();
    if total == 0 {
        return "No matches.".to_string();
    }
    let mut out = String::new();
    let mut shown = 0usize;
    'files: for fr in results {
        for line in &fr.lines {
            if line.is_match && shown >= max {
                break 'files;
            }
            let text = String::from_utf8_lossy(&line.line);
            let text = text.trim_end_matches('\r');
            // grep convention: ':' for a match line, '-' for a context line.
            if line.is_match {
                out.push_str(&format!("{}:{}:{}\n", fr.rel_path, line.line_number, text));
                shown += 1;
            } else {
                out.push_str(&format!("{}-{}-{}\n", fr.rel_path, line.line_number, text));
            }
        }
    }
    let files = results.len();
    out.push_str(&format!(
        "\n{total} match{} in {files} file{}",
        if total == 1 { "" } else { "es" },
        if files == 1 { "" } else { "s" },
    ));
    if shown < total {
        out.push_str(" — showing first matches; narrow with path/glob/type or raise max_results");
    }
    out
}
