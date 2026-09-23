//! An MCP server over stdio, so a coding agent can read scan results and clean
//! up without shelling out and parsing text.
//!
//! Register it with:
//!
//! ```text
//! claude mcp add diskscour -- /usr/local/bin/diskscour mcp
//! ```
//!
//! Only three JSON-RPC methods matter for a tools-only server (`initialize`,
//! `tools/list`, `tools/call`), which is why this speaks the protocol directly
//! instead of pulling in an SDK and an async runtime.
//!
//! Reads are served from the persistent index, never by scanning: a tool call
//! should be cheap enough to make freely. Every response therefore carries
//! `scanned_at`, `age_seconds` and `mode` so the caller knows how old the
//! numbers are — and [`crate::cleanup`] re-checks the filesystem before deleting
//! anything, so acting on a stale read still cannot delete the wrong thing.
//!
//! Scans run as background jobs, one per root, because a whole-disk scan takes
//! minutes and an MCP client gives up on a tool call after about a minute:
//! `ds_scan` waits a bounded time, then reports progress and lets the caller
//! ask again while the scan keeps going.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::caches;
use crate::cleanup;
use crate::engine::{self, Freshness};
use crate::index::Index;
use crate::scan::{FULL_DISK_ACCESS_HINT, ScanProgress};
use crate::util;

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Cap on rows returned by a listing tool, so one call can't flood a context.
const MAX_ROWS: usize = 500;
const DEFAULT_ROWS: usize = 30;

/// How long `ds_scan` waits for the scan before answering with progress.
/// The cap stays under the roughly 60 s after which MCP clients give up.
const DEFAULT_WAIT_SECS: u64 = 20;
const MAX_WAIT_SECS: u64 = 55;
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A scan in flight on its own thread. The thread hands back the result JSON
/// only: a whole-disk tree is gigabytes, so it builds the summary and drops the
/// tree before it exits, and reads go to the saved index like every other tool.
struct Scan {
    progress: Arc<ScanProgress>,
    started: Instant,
    handle: JoinHandle<Value>,
}

/// Everything the server remembers between requests: the scans still running,
/// keyed by root. A root is absent once its scan has been joined.
#[derive(Default)]
struct Server {
    scans: BTreeMap<PathBuf, Scan>,
}

impl Server {
    /// The running scan whose root contains `path`, if any.
    fn scan_covering(&self, path: &Path) -> Option<(&Path, &Scan)> {
        self.scans
            .iter()
            .find(|(root, _)| path.starts_with(root))
            .map(|(root, scan)| (root.as_path(), scan))
    }
}

/// Run the server until stdin closes.
pub fn serve() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut line = String::new();
    let mut reader = stdin.lock();
    let mut server = Server::default();

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(()); // client closed the pipe
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                // No id available, so this is the one case that must report a
                // null-id error rather than stay silent.
                write_msg(
                    &mut stdout,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": Value::Null,
                        "error": {"code": -32700, "message": format!("parse error: {e}")}
                    }),
                )?;
                continue;
            }
        };
        // A message without an id is a notification: act on it, answer nothing.
        let Some(id) = req.get("id").cloned() else {
            continue;
        };
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(Value::Null);
        let response = match dispatch(&mut server, method, &params) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(err) => json!({"jsonrpc": "2.0", "id": id, "error": err.to_json()}),
        };
        write_msg(&mut stdout, &response)?;
    }
}

fn write_msg(out: &mut impl Write, v: &Value) -> std::io::Result<()> {
    writeln!(out, "{v}")?;
    out.flush()
}

#[derive(Debug)]
struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn method_not_found(m: &str) -> Self {
        RpcError {
            code: -32601,
            message: format!("unknown method: {m}"),
        }
    }
    fn invalid(msg: impl Into<String>) -> Self {
        RpcError {
            code: -32602,
            message: msg.into(),
        }
    }
    fn to_json(&self) -> Value {
        json!({"code": self.code, "message": self.message})
    }
}

fn dispatch(server: &mut Server, method: &str, params: &Value) -> Result<Value, RpcError> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "diskscour", "version": SERVER_VERSION},
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tool_definitions()})),
        "tools/call" => call_tool(server, params),
        _ => Err(RpcError::method_not_found(method)),
    }
}

// ---- tool definitions -------------------------------------------------------

fn path_prop(desc: &str) -> Value {
    json!({"type": "string", "description": desc})
}

fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "ds_status",
            "description": "List every folder DiskScour has indexed, with its total size, \
                            reclaimable dev-cache size, when it was last scanned and how. \
                            Start here to find out what is already known without scanning.",
            "inputSchema": {"type": "object", "properties": {}},
        }),
        json!({
            "name": "ds_scan",
            "description": "Scan a folder and update its index. Reuses the previous scan where \
                            it can (FSEvents tells it what changed), so a repeat scan is fast. \
                            Pass mode='full' to re-stat everything, which is the way to correct \
                            sizes of files that grew in place. This is the only tool that touches \
                            the whole filesystem; the read tools are served from the index. \
                            The whole disk ('/') works. A scan runs in the background: when it \
                            outlasts wait_seconds the result has status 'running' with progress \
                            so far, and calling again with the same path keeps waiting for it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": path_prop("Absolute path of the folder to scan."),
                    "mode": {"type": "string", "enum": ["auto", "full"],
                             "description": "auto (default) reuses the cached index; full rescans everything."},
                    "wait_seconds": {"type": "integer",
                                     "description": "How long to wait for the scan before answering with \
                                                     progress (default 20, max 55)."},
                },
                "required": ["path"],
            },
        }),
        json!({
            "name": "ds_caches",
            "description": "List regenerable developer caches (node_modules, target, .next, \
                            DerivedData, Pods, .venv, …) under an indexed folder, biggest first. \
                            Each entry is confirmed against the filesystem, so anything already \
                            deleted is left out. These are the paths ds_trash accepts by default.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": path_prop("Absolute path, either an indexed root or anything inside one."),
                    "category": {"type": "string", "description": "Filter by ecosystem, e.g. 'rust', 'js', 'python', 'apple'."},
                    "min_bytes": {"type": "integer", "description": "Only entries at least this large."},
                    "limit": {"type": "integer", "description": "Max rows (default 30, max 500)."},
                },
                "required": ["path"],
            },
        }),
        json!({
            "name": "ds_top",
            "description": "Largest entries anywhere under a path, biggest first — the fastest \
                            way to answer 'what is eating my disk'. Unlike ds_tree this looks \
                            through the whole subtree, not just one level.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": path_prop("Absolute path, either an indexed root or anything inside one."),
                    "limit": {"type": "integer", "description": "Max rows (default 30, max 500)."},
                    "dirs_only": {"type": "boolean", "description": "Skip individual files (default false)."},
                },
                "required": ["path"],
            },
        }),
        json!({
            "name": "ds_tree",
            "description": "Immediate children of a path with their sizes, biggest first — use \
                            it to drill down one level at a time. Files smaller than 1 MB are \
                            summarised as a single '(N small files)' row rather than listed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": path_prop("Absolute path, either an indexed root or anything inside one."),
                    "limit": {"type": "integer", "description": "Max rows (default 30, max 500)."},
                },
                "required": ["path"],
            },
        }),
        json!({
            "name": "ds_trash",
            "description": "Move paths to the macOS Trash (recoverable — never a hard delete). \
                            Returns a plan and deletes NOTHING unless confirm=true, so call it \
                            once to see what would go, then again to do it. By default only \
                            recognised regenerable caches are accepted; every target is \
                            re-checked against the filesystem at the moment of deletion.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "paths": {"type": "array", "items": {"type": "string"},
                              "description": "Absolute paths to trash. All must sit inside one indexed root."},
                    "confirm": {"type": "boolean",
                                "description": "false (default) previews only. Set true to actually trash."},
                    "allow_any": {"type": "boolean",
                                  "description": "Permit paths that are NOT recognised caches. Only set this \
                                                  when the user has specifically asked for that path by name — \
                                                  it removes the check that keeps real work from being deleted."},
                },
                "required": ["paths"],
            },
        }),
    ]
}

// ---- dispatch ---------------------------------------------------------------

fn call_tool(server: &mut Server, params: &Value) -> Result<Value, RpcError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid("missing tool name"))?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let body = match name {
        "ds_status" => tool_status(server),
        "ds_scan" => tool_scan(server, &args),
        "ds_caches" => tool_caches(server, &args),
        "ds_top" => tool_top(server, &args),
        "ds_tree" => tool_tree(server, &args),
        "ds_trash" => tool_trash(server, &args),
        other => return Err(RpcError::invalid(format!("unknown tool: {other}"))),
    };

    Ok(match body {
        Ok(v) => json!({
            "content": [{"type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default()}],
            "isError": false,
        }),
        Err(msg) => json!({
            "content": [{"type": "text", "text": msg}],
            "isError": true,
        }),
    })
}

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn arg_limit(args: &Value) -> usize {
    args.get("limit")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(1, MAX_ROWS))
        .unwrap_or(DEFAULT_ROWS)
}

/// Expand `~` and make a path absolute, as an agent may pass either form.
fn resolve_arg_path(raw: &str) -> PathBuf {
    let expanded = if raw == "~" {
        std::env::var_os("HOME").map(PathBuf::from)
    } else if let Some(rest) = raw.strip_prefix("~/") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest))
    } else {
        None
    };
    let p = expanded.unwrap_or_else(|| PathBuf::from(raw));
    if p.is_absolute() {
        p
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    }
}

fn index_for(server: &Server, path: &Path) -> Result<Index, String> {
    engine::index_covering(path).ok_or_else(|| match server.scan_covering(path) {
        Some((root, scan)) => format!(
            "No index covers {} yet: a scan of {} is running ({} files, {} so far). \
             Call ds_scan on it to wait for the result.",
            path.display(),
            root.display(),
            scan.progress.files.load(Ordering::Relaxed),
            util::human(scan.progress.bytes.load(Ordering::Relaxed)),
        ),
        None => format!(
            "No index covers {}. Run ds_scan on it (or on a parent folder) first.",
            path.display()
        ),
    })
}

/// Progress of a running scan, in the shape both `ds_scan` and `ds_status` report.
fn progress_of(root: &Path, scan: &Scan) -> Value {
    let bytes = scan.progress.bytes.load(Ordering::Relaxed);
    json!({
        "root": root.to_string_lossy(),
        "files_so_far": scan.progress.files.load(Ordering::Relaxed),
        "bytes_so_far": bytes,
        "bytes_so_far_human": util::human(bytes),
        "elapsed_seconds": scan.started.elapsed().as_secs(),
    })
}

/// Provenance attached to every read, so the caller can judge the numbers.
fn freshness_of(idx: &Index) -> Value {
    let age = idx.age_secs();
    json!({
        "root": idx.root.to_string_lossy(),
        "scanned_at": idx.scanned_at,
        "age_seconds": age,
        "age_human": human_age(age),
        "mode": idx.mode.as_str(),
        "stale": age > engine::MAX_INCREMENTAL_AGE,
    })
}

fn human_age(secs: u64) -> String {
    if secs < 90 {
        format!("{secs}s ago")
    } else if secs < 5400 {
        format!("{}m ago", secs / 60)
    } else if secs < 172_800 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

// ---- tools ------------------------------------------------------------------

fn tool_status(server: &Server) -> Result<Value, String> {
    let roots: Vec<Value> = engine::cached_roots()
        .iter()
        .map(|idx| {
            let hits = caches::detect_in_index(idx);
            let refs = crate::live::Refs::collect();
            let reclaimable: u64 = hits
                .iter()
                .filter(|h| refs.protecting(&h.path).is_empty())
                .map(|h| h.private)
                .sum();
            let disk = util::disk_usage(&idx.root);
            json!({
                "root": idx.root.to_string_lossy(),
                "total_bytes": idx.total_bytes(),
                "total_human": util::human(idx.total_bytes()),
                "files": idx.total_files(),
                "reclaimable_bytes": reclaimable,
                "reclaimable_human": util::human(reclaimable),
                "cache_dirs": hits.len(),
                "scanned_at": idx.scanned_at,
                "age_human": human_age(idx.age_secs()),
                "mode": idx.mode.as_str(),
                "volume_total_human": disk.map(|(t, _)| util::human(t)),
                "volume_free_human": disk.map(|(_, a)| util::human(a)),
            })
        })
        .collect();

    // How this server is wired in. `sessions` counts this process too, so a
    // lone caller sees 1 rather than 0.
    let mcp = crate::registration::Status::collect().to_json();
    let running: Vec<Value> = server
        .scans
        .iter()
        .map(|(root, scan)| progress_of(root, scan))
        .collect();

    if roots.is_empty() {
        return Ok(json!({
            "indexed_roots": [],
            "running_scans": running,
            "hint": "Nothing indexed yet. Call ds_scan with a path such as the user's home or project folder.",
            "mcp": mcp,
        }));
    }
    Ok(json!({"indexed_roots": roots, "running_scans": running, "mcp": mcp}))
}

fn tool_scan(server: &mut Server, args: &Value) -> Result<Value, String> {
    let raw = arg_str(args, "path").ok_or("path is required")?;
    let root = resolve_arg_path(&raw);
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    let freshness = match arg_str(args, "mode").as_deref() {
        Some("full") => Freshness::Full,
        _ => Freshness::Auto,
    };
    let wait = args
        .get("wait_seconds")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_WAIT_SECS, |n| n.min(MAX_WAIT_SECS));

    // Re-attach to a scan already running on this root rather than start a
    // second one.
    let scan = match server.scans.remove(&root) {
        Some(scan) => scan,
        None => start_scan(root.clone(), freshness)?,
    };

    let deadline = Instant::now() + Duration::from_secs(wait);
    while !scan.handle.is_finished() {
        if Instant::now() >= deadline {
            let mut v = progress_of(&root, &scan);
            v["status"] = json!("running");
            v["hint"] = json!(
                "Call ds_scan again with the same path to wait more; the scan keeps running."
            );
            server.scans.insert(root, scan);
            return Ok(v);
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    match scan.handle.join() {
        Ok(result) => Ok(result),
        Err(payload) => Err(format!(
            "scan of {} panicked: {}",
            root.display(),
            panic_message(&*payload)
        )),
    }
}

fn start_scan(root: PathBuf, freshness: Freshness) -> Result<Scan, String> {
    let progress = Arc::new(ScanProgress::default());
    let handle = std::thread::Builder::new()
        .name(format!("scan {}", root.display()))
        .spawn({
            let progress = progress.clone();
            move || scan_result(root, freshness, progress)
        })
        .map_err(|e| format!("could not start the scan thread: {e}"))?;
    Ok(Scan {
        progress,
        started: Instant::now(),
        handle,
    })
}

/// Run the scan to completion and summarise it. The `Refreshed` (and the tree
/// inside it) is dropped here, on the scan thread, so only the summary outlives
/// the walk.
fn scan_result(root: PathBuf, freshness: Freshness, progress: Arc<ScanProgress>) -> Value {
    let result = engine::refresh(root.clone(), freshness, progress.clone(), |_| {});
    let unreadable = progress.unreadable.load(Ordering::Relaxed);
    let hits = caches::detect_in_index(&result.index);
    let refs = crate::live::Refs::collect();
    let reclaimable: u64 = hits
        .iter()
        .filter(|h| refs.protecting(&h.path).is_empty())
        .map(|h| h.private)
        .sum();

    let mut v = json!({
        "status": "done",
        "root": root.to_string_lossy(),
        "total_bytes": result.index.total_bytes(),
        "total_human": util::human(result.index.total_bytes()),
        "files": result.index.total_files(),
        "reclaimable_bytes": reclaimable,
        "reclaimable_human": util::human(reclaimable),
        "cache_dirs": hits.len(),
        "seconds": (result.secs * 100.0).round() / 100.0,
        "mode": result.mode.as_str(),
        "why": result.reason,
        "changed_dirs": result.changed_dirs,
        "unreadable_dirs": unreadable,
        "index_saved": result.saved,
    });
    if unreadable > 0 && root == Path::new("/") {
        v["hint"] = json!(FULL_DISK_ACCESS_HINT);
    }
    drop(result);
    return_freed_memory();
    v
}

/// Hand the pages a finished scan freed back to the kernel. macOS malloc keeps
/// them mapped for reuse, so without this a server that just walked a disk
/// stays at gigabytes of RSS while it idles.
#[cfg(target_os = "macos")]
fn return_freed_memory() {
    unsafe extern "C" {
        fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
    }
    // A null zone means every zone; a zero goal means as much as possible.
    unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
}

#[cfg(not(target_os = "macos"))]
fn return_freed_memory() {}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// Match a user-supplied ecosystem filter against a category label.
fn category_matches(filter: &str, c: caches::Category) -> bool {
    let f = filter.to_ascii_lowercase();
    let label = c.label().to_ascii_lowercase();
    label.contains(&f)
        || matches!(
            (f.as_str(), c),
            ("js", caches::Category::JsTs)
                | ("ts", caches::Category::JsTs)
                | ("node", caches::Category::JsTs)
                | ("java", caches::Category::Jvm)
                | ("xcode", caches::Category::Apple)
                | ("swift", caches::Category::Apple)
                | ("ios", caches::Category::Apple)
        )
}

fn tool_caches(server: &Server, args: &Value) -> Result<Value, String> {
    let raw = arg_str(args, "path").ok_or("path is required")?;
    let path = resolve_arg_path(&raw);
    let idx = index_for(server, &path)?;
    let limit = arg_limit(args);
    let min_bytes = args.get("min_bytes").and_then(Value::as_u64).unwrap_or(0);
    let category = arg_str(args, "category");

    let all = caches::detect_in_index(&idx);
    let filtered: Vec<&caches::IndexHit> = all
        .iter()
        .filter(|h| h.path.starts_with(&path) || path.starts_with(&h.path))
        .filter(|h| h.private >= min_bytes)
        .filter(|h| {
            category
                .as_deref()
                .is_none_or(|c| category_matches(c, h.category))
        })
        .collect();

    let refs = crate::live::Refs::collect();
    let total: u64 = filtered
        .iter()
        .filter(|h| refs.protecting(&h.path).is_empty())
        .map(|h| h.private)
        .sum();
    let apparent: u64 = filtered.iter().map(|h| h.size).sum();
    let rows: Vec<Value> = filtered
        .iter()
        .take(limit)
        .map(|h| {
            let shared = h.size > h.private.saturating_mul(2);
            let in_use = refs
                .protecting(&h.path)
                .first()
                .map(|(exe, r)| format!("{} — {}", exe.display(), r.describe()));
            json!({
                "path": h.path.to_string_lossy(),
                "in_use": in_use,
                "bytes": h.private,
                "human": util::human(h.private),
                "apparent_bytes": h.size,
                "apparent_human": util::human(h.size),
                "mostly_shared": shared,
                "files": h.files,
                "category": h.category.label(),
                "note": h.note,
            })
        })
        .collect();

    Ok(json!({
        "index": freshness_of(&idx),
        "reclaimable_bytes": total,
        "reclaimable_human": util::human(total),
        "apparent_bytes": apparent,
        "apparent_human": util::human(apparent),
        "size_note": "bytes/human is what deleting would actually free. apparent_* is what \
                      du would report; entries flagged mostly_shared occupy blocks shared \
                      with a global package store, so removing them frees almost nothing. \
                      An entry with in_use set holds an executable something is running or \
                      reaching through $PATH — ds_trash refuses those.",
        "matches": filtered.len(),
        "shown": rows.len(),
        "caches": rows,
        "next_step": "Pass any of these paths to ds_trash (confirm=false first to preview).",
    }))
}

/// Locate a node by absolute path in a materialized tree.
fn node_at(tree: &crate::scan::Tree, path: &Path) -> Option<usize> {
    let rel = path.strip_prefix(&tree.root_path).ok()?;
    let mut cur = tree.root;
    'outer: for comp in rel.components() {
        let name = comp.as_os_str().to_string_lossy();
        for &c in &tree.nodes[cur].children {
            if tree.nodes[c].name == name {
                cur = c;
                continue 'outer;
            }
        }
        return None;
    }
    Some(cur)
}

fn tool_top(server: &Server, args: &Value) -> Result<Value, String> {
    let raw = arg_str(args, "path").ok_or("path is required")?;
    let path = resolve_arg_path(&raw);
    let idx = index_for(server, &path)?;
    let limit = arg_limit(args);
    let dirs_only = arg_bool(args, "dirs_only");

    let tree = crate::index::to_tree(&idx);
    let start = node_at(&tree, &path).ok_or_else(|| {
        format!(
            "{} is not in the index for {}",
            path.display(),
            idx.root.display()
        )
    })?;

    // Collect the whole subtree, then take the biggest. Synthetic small-file
    // stand-ins are left out: they have no path to act on.
    let mut all: Vec<usize> = Vec::new();
    let mut stack = vec![start];
    while let Some(n) = stack.pop() {
        for &c in &tree.nodes[n].children {
            let node = &tree.nodes[c];
            if node.synthetic {
                continue;
            }
            if !dirs_only || node.is_dir {
                all.push(c);
            }
            if node.is_dir {
                stack.push(c);
            }
        }
    }
    all.sort_by_key(|&i| std::cmp::Reverse(tree.nodes[i].size));

    let rows: Vec<Value> = all
        .iter()
        .take(limit)
        .map(|&i| {
            let n = &tree.nodes[i];
            json!({
                "path": tree.path(i).to_string_lossy(),
                "bytes": n.size,
                "human": util::human(n.size),
                "is_dir": n.is_dir,
                "files": n.file_count,
            })
        })
        .collect();

    Ok(json!({
        "index": freshness_of(&idx),
        "path": path.to_string_lossy(),
        "total_bytes": tree.nodes[start].size,
        "total_human": util::human(tree.nodes[start].size),
        "entries": rows,
    }))
}

fn tool_tree(server: &Server, args: &Value) -> Result<Value, String> {
    let raw = arg_str(args, "path").ok_or("path is required")?;
    let path = resolve_arg_path(&raw);
    let idx = index_for(server, &path)?;
    let limit = arg_limit(args);

    let tree = crate::index::to_tree(&idx);
    let start = node_at(&tree, &path).ok_or_else(|| {
        format!(
            "{} is not in the index for {}",
            path.display(),
            idx.root.display()
        )
    })?;

    let kids = &tree.nodes[start].children;
    let rows: Vec<Value> = kids
        .iter()
        .take(limit)
        .map(|&c| {
            let n = &tree.nodes[c];
            json!({
                "name": n.name,
                "path": if n.synthetic { Value::Null } else { json!(tree.path(c).to_string_lossy()) },
                "bytes": n.size,
                "human": util::human(n.size),
                "is_dir": n.is_dir,
                "files": n.file_count,
                "summary_row": n.synthetic,
            })
        })
        .collect();

    Ok(json!({
        "index": freshness_of(&idx),
        "path": path.to_string_lossy(),
        "total_bytes": tree.nodes[start].size,
        "total_human": util::human(tree.nodes[start].size),
        "children": kids.len(),
        "shown": rows.len(),
        "entries": rows,
    }))
}

fn tool_trash(server: &Server, args: &Value) -> Result<Value, String> {
    let raw = args
        .get("paths")
        .and_then(Value::as_array)
        .ok_or("paths is required and must be an array")?;
    if raw.is_empty() {
        return Err("paths is empty".into());
    }
    let paths: Vec<PathBuf> = raw
        .iter()
        .filter_map(Value::as_str)
        .map(resolve_arg_path)
        .collect();
    if paths.is_empty() {
        return Err("paths contained no usable strings".into());
    }

    // Every target must live under one indexed root; that root is the boundary
    // all the structural guards are measured against.
    let idx = index_for(server, &paths[0])?;
    let root = idx.root.clone();
    let allow_any = arg_bool(args, "allow_any");
    let confirm = arg_bool(args, "confirm");

    let known: std::collections::HashMap<PathBuf, u64> = caches::detect_in_index(&idx)
        .into_iter()
        .map(|h| (h.path, h.private))
        .collect();
    let plan = cleanup::plan_with_sizes(&root, &paths, allow_any, |p| known.get(p).copied());
    let planned: Vec<Value> = plan
        .items
        .iter()
        .map(|i| {
            json!({
                "path": i.path.to_string_lossy(),
                "bytes": i.bytes,
                "human": util::human(i.bytes),
                "category": i.category.map(|c| c.label()),
                "note": i.note,
            })
        })
        .collect();
    let rejected: Vec<Value> = plan
        .rejected
        .iter()
        .map(|r| json!({"path": r.path.to_string_lossy(), "reason": r.reason}))
        .collect();

    if !confirm {
        return Ok(json!({
            "confirmed": false,
            "root": root.to_string_lossy(),
            "would_trash": planned,
            "would_free_bytes": plan.total_bytes(),
            "would_free_human": util::human(plan.total_bytes()),
            "rejected": rejected,
            "next_step": "Nothing was deleted. Show this plan to the user, and call again with \
                          confirm=true once they agree.",
        }));
    }

    let outcomes = cleanup::execute(&root, &plan.items, allow_any);
    let freed: u64 = outcomes.iter().filter(|o| o.trashed).map(|o| o.bytes).sum();
    let results: Vec<Value> = outcomes
        .iter()
        .map(|o| {
            json!({
                "path": o.path.to_string_lossy(),
                "trashed": o.trashed,
                "bytes": o.bytes,
                "human": util::human(o.bytes),
                "error": o.error,
            })
        })
        .collect();

    Ok(json!({
        "confirmed": true,
        "root": root.to_string_lossy(),
        "trashed_count": outcomes.iter().filter(|o| o.trashed).count(),
        "freed_bytes": freed,
        "freed_human": util::human(freed),
        "results": results,
        "rejected": rejected,
        "note": "Items were moved to the macOS Trash and can be restored from there. \
                 The index is now out of date — call ds_scan to refresh it.",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_reports_a_tools_capability() {
        let mut server = Server::default();
        let r = dispatch(&mut server, "initialize", &Value::Null).expect("initialize");
        assert_eq!(r["protocolVersion"], PROTOCOL_VERSION);
        assert!(r["capabilities"]["tools"].is_object());
        assert_eq!(r["serverInfo"]["name"], "diskscour");
    }

    #[test]
    fn every_tool_has_a_description_and_schema() {
        let tools = tool_definitions();
        assert_eq!(tools.len(), 6);
        for t in &tools {
            assert!(t["name"].as_str().is_some_and(|n| n.starts_with("ds_")));
            assert!(t["description"].as_str().is_some_and(|d| d.len() > 40));
            assert_eq!(t["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn unknown_method_and_tool_are_reported_distinctly() {
        let mut server = Server::default();
        assert_eq!(
            dispatch(&mut server, "nope", &Value::Null)
                .err()
                .unwrap()
                .code,
            -32601
        );
        let call = json!({"name": "ds_nope", "arguments": {}});
        assert_eq!(
            dispatch(&mut server, "tools/call", &call)
                .err()
                .unwrap()
                .code,
            -32602
        );
    }

    #[test]
    fn a_failing_tool_answers_with_is_error_not_a_protocol_error() {
        // Tool-level failures must come back as content so the agent can read
        // them, not as JSON-RPC errors that it cannot act on.
        let call = json!({"name": "ds_caches", "arguments": {"path": "/definitely/not/here"}});
        let r =
            dispatch(&mut Server::default(), "tools/call", &call).expect("dispatch should succeed");
        assert_eq!(r["isError"], true);
        assert!(
            r["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("ds_scan")
        );
    }

    #[test]
    fn trash_without_confirm_never_reports_a_deletion() {
        let call = json!({"name": "ds_trash", "arguments": {"paths": ["/tmp/does-not-exist-xyz"]}});
        let r = dispatch(&mut Server::default(), "tools/call", &call).expect("dispatch");
        let text = r["content"][0]["text"].as_str().unwrap();
        assert!(!text.contains("\"confirmed\": true"));
    }

    #[test]
    fn tilde_paths_expand() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(resolve_arg_path("~"), PathBuf::from(&home));
        assert_eq!(
            resolve_arg_path("~/Github"),
            PathBuf::from(&home).join("Github")
        );
    }

    #[test]
    fn category_filter_accepts_common_shorthands() {
        assert!(category_matches("rust", caches::Category::Rust));
        assert!(category_matches("js", caches::Category::JsTs));
        assert!(category_matches("node", caches::Category::JsTs));
        assert!(category_matches("xcode", caches::Category::Apple));
        assert!(!category_matches("rust", caches::Category::Python));
    }
}
