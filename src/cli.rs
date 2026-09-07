//! The headless commands. `diskscour` with no arguments opens the window;
//! everything here is for a terminal or another program.
//!
//! Every listing command takes `--json`, because the most common non-human
//! caller is a coding agent that would otherwise have to parse columns.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::caches;
use crate::cleanup;
use crate::engine::{self, Freshness};
use crate::index::Index;
use crate::live;
use crate::registration;
use crate::scan::ScanProgress;
use crate::util;

pub const USAGE: &str = "\
DiskScour — disk-usage analyzer with dev-cache cleanup

  diskscour                        open the window
  diskscour scan <path> [--json] [--full]
                                   scan a folder and update its cached index
  diskscour caches <path> [--json] [--min <bytes>]
                                   list regenerable dev caches from the index
  diskscour status [--json]        show every indexed folder
  diskscour trash <path>... [--yes] [--allow-any]
                                   move caches to the Trash (previews unless --yes)
  diskscour mcp                    run as an MCP server on stdio

Scans reuse the previous result where they can, so a repeat scan of the same
folder is fast. --full re-stats everything.
";

/// Parsed flags, plus whatever was left over as positional arguments.
struct Args {
    positional: Vec<String>,
    json: bool,
    full: bool,
    yes: bool,
    allow_any: bool,
    min: u64,
}

fn parse(rest: &[String]) -> Result<Args, String> {
    let mut a = Args {
        positional: Vec::new(),
        json: false,
        full: false,
        yes: false,
        allow_any: false,
        min: 0,
    };
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => a.json = true,
            "--full" => a.full = true,
            "--yes" | "-y" => a.yes = true,
            "--allow-any" => a.allow_any = true,
            "--min" => {
                let v = it.next().ok_or("--min needs a value")?;
                a.min = v.parse().map_err(|_| format!("--min: not a number: {v}"))?;
            }
            other if other.starts_with('-') => return Err(format!("unknown flag: {other}")),
            other => a.positional.push(other.to_string()),
        }
    }
    Ok(a)
}

fn path_arg(a: &Args, what: &str) -> Result<PathBuf, String> {
    let raw = a
        .positional
        .first()
        .ok_or_else(|| format!("{what} needs a path"))?;
    Ok(expand(raw))
}

fn expand(raw: &str) -> PathBuf {
    if raw == "~" {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    let p = PathBuf::from(raw);
    if p.is_absolute() {
        p
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    }
}

/// Run a subcommand. Returns the process exit code.
pub fn run(command: &str, rest: &[String]) -> i32 {
    let args = match parse(rest) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("diskscour: {e}\n\n{USAGE}");
            return 2;
        }
    };
    let result = match command {
        "scan" => cmd_scan(&args),
        "caches" => cmd_caches(&args),
        "status" => cmd_status(&args),
        "trash" => cmd_trash(&args),
        other => Err(format!("unknown command: {other}")),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("diskscour: {e}");
            1
        }
    }
}

fn emit(json: bool, value: &Value, human: impl FnOnce()) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(value).unwrap_or_default()
        );
    } else {
        human();
    }
}

// ---- scan -------------------------------------------------------------------

fn cmd_scan(a: &Args) -> Result<(), String> {
    let root = path_arg(a, "scan")?;
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    let freshness = if a.full {
        Freshness::Full
    } else {
        Freshness::Auto
    };
    let r = engine::refresh(
        root.clone(),
        freshness,
        Arc::new(ScanProgress::default()),
        |_| {},
    );
    let hits = caches::detect_in_index(&r.index);
    let refs = live::Refs::collect();
    // Reclaimable counts only what deleting would really free: the private
    // figure, and only for directories nothing is using.
    let reclaimable: u64 = hits
        .iter()
        .filter(|h| refs.protecting(&h.path).is_empty())
        .map(|h| h.private)
        .sum();
    let in_use: u64 = hits
        .iter()
        .filter(|h| !refs.protecting(&h.path).is_empty())
        .map(|h| h.private)
        .sum();
    let apparent: u64 = hits.iter().map(|h| h.size).sum();
    let total = r.index.total_bytes();

    let value = json!({
        "root": root.to_string_lossy(),
        "total_bytes": total,
        "total_human": util::human(total),
        "files": r.index.total_files(),
        "reclaimable_bytes": reclaimable,
        "reclaimable_human": util::human(reclaimable),
        "apparent_bytes": apparent,
        "apparent_human": util::human(apparent),
        "in_use_bytes": in_use,
        "in_use_human": util::human(in_use),
        "cache_dirs": hits.len(),
        "seconds": (r.secs * 100.0).round() / 100.0,
        "mode": r.mode.as_str(),
        "why": r.reason,
        "changed_dirs": r.changed_dirs,
        "index_saved": r.saved,
        "largest": hits.iter().take(25).map(|h| json!({
            "path": h.path.to_string_lossy(),
            "in_use": refs.protecting(&h.path).first().map(|(exe, r)|
                format!("{} — {}", exe.display(), r.describe())),
            "bytes": h.private,
            "human": util::human(h.private),
            "apparent_bytes": h.size,
            "apparent_human": util::human(h.size),
            "shared": h.size > h.private.saturating_mul(2),
            "category": h.category.label(),
        })).collect::<Vec<_>>(),
    });

    emit(a.json, &value, || {
        println!(
            "\n{}\n  {} across {} files in {:.2}s  [{}: {}]\n",
            root.display(),
            util::human(total),
            r.index.total_files(),
            r.secs,
            r.mode.as_str(),
            r.reason
        );
        println!(
            "Dev caches: {} reclaimable across {} dirs  ({} apparent{})",
            util::human(reclaimable),
            hits.len(),
            util::human(apparent),
            if in_use > 0 {
                format!(", {} in use", util::human(in_use))
            } else {
                String::new()
            }
        );
        let mut by_private: Vec<&caches::IndexHit> = hits.iter().collect();
        by_private.sort_by_key(|h| std::cmp::Reverse(h.private));
        for h in by_private.iter().take(25) {
            let rel = h.path.strip_prefix(&root).unwrap_or(&h.path);
            // Flag entries whose apparent size is mostly shared blocks, so the
            // gap between the two numbers never looks like a mistake.
            let note = if let Some((exe, r)) = refs.protecting(&h.path).first() {
                format!("  ⚠ IN USE: {} — {}", exe.display(), r.describe())
            } else if h.size > h.private.saturating_mul(2) {
                format!(" (looks like {}, mostly shared)", util::human(h.size))
            } else {
                String::new()
            };
            println!(
                "  {:>10}  [{}] {}{}",
                util::human(h.private),
                h.category.label(),
                rel.display(),
                note
            );
        }
    });
    Ok(())
}

// ---- caches -----------------------------------------------------------------

fn require_index(path: &std::path::Path) -> Result<Index, String> {
    engine::index_covering(path).ok_or_else(|| {
        format!(
            "no index covers {} — run `diskscour scan {}` first",
            path.display(),
            path.display()
        )
    })
}

fn cmd_caches(a: &Args) -> Result<(), String> {
    let path = path_arg(a, "caches")?;
    let idx = require_index(&path)?;
    let all = caches::detect_in_index(&idx);
    let hits: Vec<&caches::IndexHit> = all
        .iter()
        .filter(|h| h.path.starts_with(&path) || path.starts_with(&h.path))
        .filter(|h| h.private >= a.min)
        .collect();
    let refs = live::Refs::collect();
    let total: u64 = hits
        .iter()
        .filter(|h| refs.protecting(&h.path).is_empty())
        .map(|h| h.private)
        .sum();
    let apparent: u64 = hits.iter().map(|h| h.size).sum();

    let value = json!({
        "root": idx.root.to_string_lossy(),
        "scanned_at": idx.scanned_at,
        "age_seconds": idx.age_secs(),
        "mode": idx.mode.as_str(),
        "reclaimable_bytes": total,
        "reclaimable_human": util::human(total),
        "apparent_bytes": apparent,
        "apparent_human": util::human(apparent),
        "caches": hits.iter().map(|h| json!({
            "path": h.path.to_string_lossy(),
            "in_use": refs.protecting(&h.path).first().map(|(exe, r)|
                format!("{} — {}", exe.display(), r.describe())),
            "bytes": h.private,
            "human": util::human(h.private),
            "apparent_bytes": h.size,
            "apparent_human": util::human(h.size),
            "shared": h.size > h.private.saturating_mul(2),
            "files": h.files,
            "category": h.category.label(),
            "note": h.note,
        })).collect::<Vec<_>>(),
    });

    emit(a.json, &value, || {
        println!(
            "{} reclaimable across {} dirs under {}  ({} apparent)",
            util::human(total),
            hits.len(),
            path.display(),
            util::human(apparent)
        );
        for h in &hits {
            let note = if let Some((exe, r)) = refs.protecting(&h.path).first() {
                format!("  ⚠ IN USE: {} — {}", exe.display(), r.describe())
            } else if h.size > h.private.saturating_mul(2) {
                format!(" (looks like {}, mostly shared)", util::human(h.size))
            } else {
                String::new()
            };
            println!(
                "  {:>10}  [{}] {}{}",
                util::human(h.private),
                h.category.label(),
                h.path.display(),
                note
            );
        }
    });
    Ok(())
}

// ---- status -----------------------------------------------------------------

fn cmd_status(a: &Args) -> Result<(), String> {
    let roots = engine::cached_roots();
    let rows: Vec<Value> = roots
        .iter()
        .map(|idx| {
            let hits = caches::detect_in_index(idx);
            let refs = live::Refs::collect();
            let recl: u64 = hits
                .iter()
                .filter(|h| refs.protecting(&h.path).is_empty())
                .map(|h| h.private)
                .sum();
            json!({
                "root": idx.root.to_string_lossy(),
                "total_bytes": idx.total_bytes(),
                "total_human": util::human(idx.total_bytes()),
                "unshared_bytes": idx.total_private(),
                "unshared_human": util::human(idx.total_private()),
                "files": idx.total_files(),
                "reclaimable_bytes": recl,
                "reclaimable_human": util::human(recl),
                "scanned_at": idx.scanned_at,
                "age_seconds": idx.age_secs(),
                "mode": idx.mode.as_str(),
            })
        })
        .collect();

    let mcp = registration::Status::collect();

    emit(
        a.json,
        &json!({"indexed_roots": rows, "mcp": mcp.to_json()}),
        || {
            if roots.is_empty() {
                println!("Nothing indexed yet. Try `diskscour scan ~`.");
            }
            for (idx, row) in roots.iter().zip(&rows) {
                println!(
                    "{:>10}  {:>10} reclaimable  {:>6}  {}",
                    util::human(idx.total_bytes()),
                    row["reclaimable_human"].as_str().unwrap_or("-"),
                    idx.mode.as_str(),
                    idx.root.display()
                );
            }
            println!();
            print_mcp(&mcp);
        },
    );
    Ok(())
}

/// The MCP block of `diskscour status`: transport, registration, sessions.
fn print_mcp(mcp: &registration::Status) {
    println!(
        "MCP server  stdio · v{} · {}",
        mcp.this_version,
        mcp.this_binary.display()
    );
    if mcp.registrations.is_empty() {
        println!("  not registered with Claude Code. To add it:");
        println!("    {}", mcp.add_command());
    } else {
        for r in &mcp.registrations {
            println!(
                "  registered  {} · {} · {} {}  [{}]",
                r.client,
                r.scope,
                r.command.display(),
                r.args.join(" "),
                r.health.describe(mcp.this_version)
            );
        }
        if mcp.level() == registration::Level::Warn {
            println!("  to re-register this build:");
            println!("    claude mcp remove diskscour && {}", mcp.add_command());
        }
    }
    match mcp.sessions.len() {
        0 => println!("  sessions    none connected"),
        n => println!("  sessions    {n} connected · {}", mcp.sessions_by_client()),
    }
}

// ---- trash ------------------------------------------------------------------

fn cmd_trash(a: &Args) -> Result<(), String> {
    if a.positional.is_empty() {
        return Err("trash needs at least one path".into());
    }
    let paths: Vec<PathBuf> = a.positional.iter().map(|p| expand(p)).collect();
    let idx = require_index(&paths[0])?;
    let root = idx.root.clone();

    // Sizes come from the index. Re-walking every target just to print a total
    // costs far more than the deletion, and at a few hundred paths it dominates.
    let known: std::collections::HashMap<PathBuf, u64> = caches::detect_in_index(&idx)
        .into_iter()
        .map(|h| (h.path, h.private))
        .collect();
    let plan = cleanup::plan_with_sizes(&root, &paths, a.allow_any, |p| known.get(p).copied());
    for r in &plan.rejected {
        eprintln!("  skipped  {}  ({})", r.path.display(), r.reason);
    }
    if plan.items.is_empty() {
        return Err("nothing to trash".into());
    }

    if !a.yes {
        let value = json!({
            "confirmed": false,
            "would_free_bytes": plan.total_bytes(),
            "would_free_human": util::human(plan.total_bytes()),
            "would_trash": plan.items.iter().map(|i| json!({
                "path": i.path.to_string_lossy(),
                "bytes": i.bytes,
                "human": util::human(i.bytes),
                "category": i.category.map(|c| c.label()),
                "note": i.note,
            })).collect::<Vec<_>>(),
        });
        emit(a.json, &value, || {
            println!(
                "Would move {} to the Trash ({} items):",
                util::human(plan.total_bytes()),
                plan.items.len()
            );
            for i in &plan.items {
                println!("  {:>10}  {}", util::human(i.bytes), i.path.display());
            }
            println!("\nRe-run with --yes to do it.");
        });
        return Ok(());
    }

    let outcomes = cleanup::execute(&root, &plan.items, a.allow_any);
    let freed: u64 = outcomes.iter().filter(|o| o.trashed).map(|o| o.bytes).sum();
    let failed = outcomes.iter().filter(|o| !o.trashed).count();

    let value = json!({
        "confirmed": true,
        "freed_bytes": freed,
        "freed_human": util::human(freed),
        "trashed_count": outcomes.iter().filter(|o| o.trashed).count(),
        "failed_count": failed,
        "results": outcomes.iter().map(|o| json!({
            "path": o.path.to_string_lossy(),
            "trashed": o.trashed,
            "bytes": o.bytes,
            "error": o.error,
        })).collect::<Vec<_>>(),
    });

    emit(a.json, &value, || {
        for o in &outcomes {
            match &o.error {
                None => println!(
                    "  trashed  {:>10}  {}",
                    util::human(o.bytes),
                    o.path.display()
                ),
                Some(e) => println!("  FAILED   {}  ({e})", o.path.display()),
            }
        }
        println!("\nFreed {} to the Trash.", util::human(freed));
    });

    if failed > 0 {
        Err(format!("{failed} item(s) could not be trashed"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_and_positionals_separate() {
        let a = parse(&[
            "/tmp/x".into(),
            "--json".into(),
            "--min".into(),
            "1024".into(),
            "/tmp/y".into(),
        ])
        .unwrap();
        assert_eq!(a.positional, vec!["/tmp/x", "/tmp/y"]);
        assert!(a.json);
        assert_eq!(a.min, 1024);
        assert!(!a.full && !a.yes && !a.allow_any);
    }

    #[test]
    fn unknown_flags_are_an_error_not_a_path() {
        assert!(parse(&["--nope".into()]).is_err());
        assert!(parse(&["--min".into()]).is_err());
        assert!(parse(&["--min".into(), "abc".into()]).is_err());
    }

    #[test]
    fn tilde_and_relative_paths_become_absolute() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand("~"), PathBuf::from(&home));
        assert_eq!(expand("~/x"), PathBuf::from(&home).join("x"));
        assert!(expand("relative").is_absolute());
        assert_eq!(expand("/abs"), PathBuf::from("/abs"));
    }
}
