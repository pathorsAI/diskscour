//! Deciding how much of a scan can be skipped, and keeping the index current.
//!
//! Everything that reads disk usage — the GUI, the CLI, the MCP server — goes
//! through [`refresh`]. It picks the cheapest strategy it can justify and always
//! records why, because the answer changes how much the numbers can be trusted:
//!
//! * [`Mode::Events`] — FSEvents named the directories that changed, so only
//!   those subtrees were walked. Cheapest and the most precise.
//! * [`Mode::Mtime`] — no usable event history, so every directory was visited
//!   but unchanged ones reused their cached file sizes.
//! * [`Mode::Full`] — everything walked and stat'ed.
//!
//! Both incremental modes reuse cached sizes for files that were not re-stat'ed,
//! so a file that grew *in place* without its directory changing keeps its old
//! size until the next full scan. [`MAX_INCREMENTAL_AGE`] bounds how long that
//! can persist.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crate::fsevents::{self, Replay};
use crate::index::{self, Index, Mode};
use crate::scan::{self, Reuse, ScanProgress, Tree};

/// Past this age an "auto" refresh does a full scan regardless of what the event
/// history says, so in-place file growth cannot go unnoticed indefinitely.
pub const MAX_INCREMENTAL_AGE: u64 = 7 * 24 * 60 * 60;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Reuse whatever can be justified.
    Auto,
    /// Walk and stat everything.
    Full,
}

pub struct Refreshed {
    pub tree: Tree,
    pub index: Index,
    pub mode: Mode,
    /// Human-readable explanation of why this mode was chosen.
    pub reason: String,
    /// Directories re-walked, when the mode was [`Mode::Events`].
    pub changed_dirs: usize,
    pub secs: f32,
    /// Whether the fresh index was written to the cache.
    pub saved: bool,
}

/// Load the cached index for `root` without touching the filesystem.
pub fn cached(root: &Path) -> Option<Index> {
    Index::load(root)
}

/// Every root that currently has a cached index, largest first.
///
/// Indexes whose root has since been deleted are dropped *and* their cache files
/// removed. Without that, a scan of a temporary directory would leave an entry
/// behind forever, and the list of "folders DiskScour knows about" would fill up
/// with things that no longer exist.
pub fn cached_roots() -> Vec<Index> {
    let Some(dir) = index::cache_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|s| s.to_str()) != Some("idx") {
            continue;
        }
        let Ok(buf) = std::fs::read(&path) else {
            continue;
        };
        match Index::decode(&buf) {
            Some(idx) if idx.root.is_dir() => out.push(idx),
            // Unreadable, or a root that is gone: the file has no further use.
            _ => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    out.sort_by_key(|idx| std::cmp::Reverse(idx.total_bytes()));
    out
}

/// The index covering `path`: an exact root match, otherwise the most specific
/// indexed root that contains it.
pub fn index_covering(path: &Path) -> Option<Index> {
    if let Some(idx) = cached(path) {
        return Some(idx);
    }
    cached_roots()
        .into_iter()
        .filter(|idx| path.starts_with(&idx.root))
        .max_by_key(|idx| idx.root.components().count())
}

/// Scan `root`, reusing as much of the cached index as can be justified, then
/// write the refreshed index back. Snapshots are streamed to `on_snapshot`.
pub fn refresh(
    root: PathBuf,
    freshness: Freshness,
    progress: Arc<ScanProgress>,
    on_snapshot: impl FnMut(Tree),
) -> Refreshed {
    let started = Instant::now();
    // Read the stream position *before* walking, so anything that changes while
    // we scan is picked up by the next refresh rather than silently missed.
    let event_id = fsevents::current_event_id();
    let device = index::device_of(&root);

    let prior = match freshness {
        Freshness::Full => None,
        Freshness::Auto => cached(&root),
    };

    let (tree, mode, reason, changed_dirs) = match plan(&root, prior.as_ref(), device) {
        Plan::Full(reason) => {
            let tree = scan::scan_streaming(root.clone(), progress, on_snapshot);
            (tree, Mode::Full, reason, 0)
        }
        Plan::Mtime(prior, reason) => {
            let tree = scan::scan_incremental(
                root.clone(),
                progress,
                Reuse {
                    prior,
                    dirty_closure: None,
                },
                on_snapshot,
            );
            (tree, Mode::Mtime, reason, 0)
        }
        Plan::Events(prior, dirty, changed) => {
            let tree = scan::scan_incremental(
                root.clone(),
                progress,
                Reuse {
                    prior,
                    dirty_closure: Some(&dirty),
                },
                on_snapshot,
            );
            let reason = format!(
                "{changed} director{} changed since last scan",
                plural(changed)
            );
            (tree, Mode::Events, reason, changed)
        }
    };

    let mut idx = index::from_tree(&tree, mode, event_id, index::DEFAULT_THRESHOLD);
    idx.device_id = device;
    let saved = idx.save().is_ok();

    Refreshed {
        tree,
        index: idx,
        mode,
        reason,
        changed_dirs,
        secs: started.elapsed().as_secs_f32(),
        saved,
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "y" } else { "ies" }
}

enum Plan<'a> {
    Full(String),
    Mtime(&'a Index, String),
    Events(&'a Index, HashSet<PathBuf>, usize),
}

/// Choose a strategy, and say why. Every fallback path lands on something that
/// is still correct, only slower — this never prunes on a guess.
fn plan<'a>(root: &Path, prior: Option<&'a Index>, device: u64) -> Plan<'a> {
    let Some(prior) = prior else {
        return Plan::Full("no cached index".into());
    };
    if prior.device_id != 0 && device != 0 && prior.device_id != device {
        return Plan::Full("root moved to a different volume".into());
    }
    if prior.age_secs() > MAX_INCREMENTAL_AGE {
        return Plan::Full(format!(
            "cached index is {} days old",
            prior.age_secs() / 86_400
        ));
    }
    match fsevents::changed_since(root, prior.last_event_id) {
        Replay::Changed(paths) => {
            let (dirty, matched) = fsevents::dirty_closure(root, &paths);
            if !paths.is_empty() && matched == 0 {
                // Events came back but none of them landed under this root. That
                // is a namespace mismatch, not quiet — pruning on it would report
                // stale sizes for a tree that did change.
                return Plan::Mtime(prior, "event paths did not match the scanned root".into());
            }
            Plan::Events(prior, dirty, matched)
        }
        Replay::Unavailable(why) => Plan::Mtime(prior, format!("event history unusable: {why}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("ds_engine_{}_{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("proj/src")).unwrap();
        fs::write(base.join("proj/Cargo.toml"), b"[package]").unwrap();
        fs::write(base.join("proj/src/main.rs"), vec![0u8; 3000]).unwrap();
        fs::create_dir_all(base.join("proj/target/debug")).unwrap();
        fs::write(base.join("proj/target/debug/blob"), vec![0u8; 40_000]).unwrap();
        base
    }

    #[test]
    fn full_scan_then_incremental_agree() {
        let base = tmp("agree");
        let full = refresh(
            base.clone(),
            Freshness::Full,
            Arc::new(ScanProgress::default()),
            |_| {},
        );
        assert_eq!(full.mode, Mode::Full);
        let total = full.tree.nodes[full.tree.root].size;
        assert!(total > 40_000, "expected the blob to be counted");

        // Nothing changed in between, so the refresh must land on the same total.
        let again = refresh(
            base.clone(),
            Freshness::Auto,
            Arc::new(ScanProgress::default()),
            |_| {},
        );
        assert_ne!(
            again.mode,
            Mode::Full,
            "should have reused the cached index"
        );
        assert_eq!(
            again.tree.nodes[again.tree.root].size, total,
            "incremental refresh changed the total ({})",
            again.reason
        );

        let _ = fs::remove_dir_all(&base);
    }

    /// The kernel folds back-to-back events on one directory into a single
    /// record, so a rescan a millisecond after a write can legitimately see
    /// nothing new. No one rescans that fast; give the event its own moment.
    fn settle() {
        std::thread::sleep(std::time::Duration::from_millis(250));
    }

    #[test]
    fn incremental_picks_up_a_new_file() {
        let base = tmp("newfile");
        let first = refresh(
            base.clone(),
            Freshness::Full,
            Arc::new(ScanProgress::default()),
            |_| {},
        );
        let before = first.tree.nodes[first.tree.root].size;

        fs::write(base.join("proj/target/debug/extra"), vec![0u8; 80_000]).unwrap();
        settle();

        let after = refresh(
            base.clone(),
            Freshness::Auto,
            Arc::new(ScanProgress::default()),
            |_| {},
        );
        assert!(
            after.tree.nodes[after.tree.root].size > before,
            "new file not picked up (mode {}, {})",
            after.mode.as_str(),
            after.reason
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn incremental_notices_a_deleted_directory() {
        let base = tmp("deleted");
        let first = refresh(
            base.clone(),
            Freshness::Full,
            Arc::new(ScanProgress::default()),
            |_| {},
        );
        let before = first.tree.nodes[first.tree.root].size;

        fs::remove_dir_all(base.join("proj/target")).unwrap();
        settle();

        let after = refresh(
            base.clone(),
            Freshness::Auto,
            Arc::new(ScanProgress::default()),
            |_| {},
        );
        assert!(
            after.tree.nodes[after.tree.root].size < before,
            "deletion not picked up (mode {}, {})",
            after.mode.as_str(),
            after.reason
        );

        let _ = fs::remove_dir_all(&base);
    }
}
