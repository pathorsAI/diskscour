//! Fast parallel directory scanning and the in-memory size tree.
//!
//! Scanning uses `jwalk` (parallel walk on a rayon pool). File sizes are read
//! in the parallel `process_read_dir` callback and carried in each entry's
//! client state, then assembled into an arena-backed tree with aggregated
//! directory sizes.
//!
//! Nodes store only their file name; full paths are reconstructed on demand via
//! [`Tree::path`] by walking parent links. This keeps per-node memory small for
//! trees with millions of entries.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::index::Index;

/// How often to publish an intermediate snapshot while a scan is running.
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(400);

/// Live progress shared with the UI thread during a scan.
#[derive(Default)]
pub struct ScanProgress {
    pub files: AtomicU64,
    pub bytes: AtomicU64,
    pub done: AtomicBool,
}

/// One node in the size tree (file or directory).
#[derive(Clone)]
pub struct Node {
    pub name: String,
    /// Allocated bytes: self size for files, aggregated for directories.
    pub size: u64,
    pub is_dir: bool,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    /// Number of files at or below this node.
    pub file_count: u64,
    /// Set when the node has been moved to trash this session.
    pub removed: bool,
    /// Directory mtime in nanoseconds since the epoch (0 for files / unknown).
    /// Incremental refresh compares this against the cached index to decide
    /// whether a directory's own files need re-stating.
    pub mtime_ns: i64,
    /// True for the stand-in node that represents a directory's collapsed small
    /// files. It has no path of its own, so it must never be revealed or trashed.
    pub synthetic: bool,
}

/// Arena-backed tree. Nodes are referenced by index; `root` is the scan root.
pub struct Tree {
    pub nodes: Vec<Node>,
    pub root: usize,
    pub root_path: PathBuf,
}

impl Tree {
    /// Reconstruct the absolute path of a node by walking parent links.
    pub fn path(&self, idx: usize) -> PathBuf {
        if idx == self.root {
            return self.root_path.clone();
        }
        let mut parts: Vec<&str> = Vec::new();
        let mut cur = idx;
        while cur != self.root {
            parts.push(self.nodes[cur].name.as_str());
            match self.nodes[cur].parent {
                Some(p) => cur = p,
                None => break, // detached node — shouldn't happen for live nodes
            }
        }
        let mut p = self.root_path.clone();
        for name in parts.iter().rev() {
            p.push(name);
        }
        p
    }

    /// Path from root → idx (inclusive) as node indices.
    pub fn ancestry(&self, idx: usize) -> Vec<usize> {
        let mut v = vec![idx];
        let mut cur = self.nodes[idx].parent;
        while let Some(p) = cur {
            v.push(p);
            cur = self.nodes[p].parent;
        }
        v.reverse();
        v
    }

    /// Sort every directory's children by descending size.
    pub fn sort_children(&mut self) {
        sort_children(&mut self.nodes);
    }

    /// Unlink a node and subtract its size/file-count from every ancestor.
    pub fn remove(&mut self, idx: usize) {
        if self.nodes[idx].removed {
            return;
        }
        let size = self.nodes[idx].size;
        let fc = self.nodes[idx].file_count;
        if let Some(p) = self.nodes[idx].parent {
            self.nodes[p].children.retain(|&c| c != idx);
        }
        let mut cur = self.nodes[idx].parent;
        while let Some(p) = cur {
            self.nodes[p].size = self.nodes[p].size.saturating_sub(size);
            self.nodes[p].file_count = self.nodes[p].file_count.saturating_sub(fc);
            cur = self.nodes[p].parent;
        }
        self.nodes[idx].removed = true;
        self.nodes[idx].parent = None;
        // Mark the entire subtree removed so detect()/treemap/navigation ignore
        // descendants of a trashed directory. Do NOT subtract their sizes again —
        // ancestor totals were already reduced by idx's full aggregate above.
        let mut stack: Vec<usize> = self.nodes[idx].children.clone();
        while let Some(c) = stack.pop() {
            self.nodes[c].removed = true;
            stack.extend(self.nodes[c].children.iter().copied());
        }
    }
}

/// Allocated size of a file, counting each hardlinked inode only once.
#[cfg(unix)]
fn counted_size(md: &std::fs::Metadata, seen: &Mutex<HashSet<(u64, u64)>>) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let sz = md.blocks().saturating_mul(512);
    if md.nlink() > 1 {
        // Count a multiply-linked inode only the first time we encounter it,
        // so pnpm stores / Time Machine local snapshots don't inflate totals.
        let mut s = seen.lock().unwrap();
        if s.insert((md.dev(), md.ino())) {
            sz
        } else {
            0
        }
    } else {
        sz
    }
}

#[cfg(not(unix))]
fn counted_size(md: &std::fs::Metadata, _seen: &Mutex<HashSet<(u64, u64)>>) -> u64 {
    md.len()
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn new_dir(name: String) -> Node {
    Node {
        name,
        size: 0,
        is_dir: true,
        parent: None,
        children: Vec::new(),
        file_count: 0,
        removed: false,
        mtime_ns: 0,
        synthetic: false,
    }
}

/// Per-directory facts gathered on the parallel walk and consumed when the
/// directory's node is built. Keyed by directory path.
#[derive(Clone, Copy, Default)]
struct DirInfo {
    mtime_ns: i64,
    /// The directory's mtime matched the cached index, so its own files were
    /// dropped from the walk and their sizes come from the index instead.
    files_cached: bool,
}

/// State carried on each walked entry. `size`/`mtime_ns` come from the parallel
/// `process_read_dir` callback; `reused` marks a directory whose whole subtree
/// was served from the index and never descended into.
#[derive(Clone, Debug, Default)]
struct EntryState {
    size: u64,
    reused: Option<u32>,
}

/// What an incremental refresh may reuse from a previous scan.
pub struct Reuse<'a> {
    pub prior: &'a Index,
    /// Directories reported changed since the previous scan, closed over their
    /// ancestors. When present, any directory *absent* from this set can be
    /// served whole from the index without touching the filesystem.
    ///
    /// When `None`, only the weaker mtime check applies: a directory's own files
    /// can be reused, but every directory is still visited, because a change deep
    /// in a subtree does not move its ancestors' mtimes.
    pub dirty_closure: Option<&'a HashSet<PathBuf>>,
}

/// Scan `root`, invoking `on_snapshot` periodically (every [`SNAPSHOT_INTERVAL`])
/// with a browsable snapshot of the tree built so far, so the UI can show results
/// as they are discovered. Snapshot node indices match the returned final tree
/// (the arena is append-only), so index-keyed UI state stays valid across
/// refreshes.
pub fn scan_streaming(
    root: PathBuf,
    progress: Arc<ScanProgress>,
    on_snapshot: impl FnMut(Tree),
) -> Tree {
    scan_impl(root, progress, on_snapshot, Some(SNAPSHOT_INTERVAL), None)
}

/// Refresh a previous scan, reusing everything `reuse` says is unchanged.
pub fn scan_incremental(
    root: PathBuf,
    progress: Arc<ScanProgress>,
    reuse: Reuse<'_>,
    on_snapshot: impl FnMut(Tree),
) -> Tree {
    scan_impl(
        root,
        progress,
        on_snapshot,
        Some(SNAPSHOT_INTERVAL),
        Some(reuse),
    )
}

/// Directory mtime in nanoseconds, or 0 when unavailable.
fn dir_mtime_ns(path: &Path) -> i64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match std::fs::symlink_metadata(path) {
            Ok(md) => md.mtime().saturating_mul(1_000_000_000) + md.mtime_nsec(),
            Err(_) => 0,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        0
    }
}

/// Shared scan implementation. When `interval` is `Some`, a snapshot is published
/// via `on_snapshot` no more often than that interval; when `None`, snapshots are
/// never built (so non-streaming callers pay no clone/aggregate cost).
///
/// When `reuse` is `Some`, unchanged parts of the tree are served from the cached
/// index instead of the filesystem — see [`Reuse`].
fn scan_impl(
    root: PathBuf,
    progress: Arc<ScanProgress>,
    mut on_snapshot: impl FnMut(Tree),
    interval: Option<Duration>,
    reuse: Option<Reuse<'_>>,
) -> Tree {
    use jwalk::WalkDirGeneric;

    let prog = progress.clone();
    let hardlinks: Arc<Mutex<HashSet<(u64, u64)>>> = Arc::new(Mutex::new(HashSet::new()));
    let hl = hardlinks.clone();

    // Facts about each directory, filled in on the walk threads and read back
    // when the corresponding node is built. One lock per directory, not per file.
    let dir_info: Arc<Mutex<HashMap<PathBuf, DirInfo>>> = Arc::new(Mutex::new(HashMap::new()));
    let di = dir_info.clone();

    // Read-only lookups shared with the walk threads.
    let prior_paths: Arc<HashMap<PathBuf, u32>> = Arc::new(match &reuse {
        Some(r) => r.prior.by_path(),
        None => HashMap::new(),
    });
    let prior_mtimes: Arc<Vec<i64>> = Arc::new(match &reuse {
        Some(r) => r.prior.dirs.iter().map(|d| d.mtime_ns).collect(),
        None => Vec::new(),
    });
    let dirty: Option<Arc<HashSet<PathBuf>>> = reuse
        .as_ref()
        .and_then(|r| r.dirty_closure)
        .map(|d| Arc::new(d.clone()));
    let has_prior = reuse.is_some();
    // The walk closure takes ownership of one handle; the build loop keeps another.
    let paths_for_build = prior_paths.clone();

    let walk = WalkDirGeneric::<((), EntryState)>::new(&root)
        .skip_hidden(false)
        .follow_links(false)
        .process_read_dir(move |_depth, path, _state, children| {
            // One stat per directory, on the walk pool. Its result is needed both
            // to decide reuse here and to record the mtime in the next index.
            let mtime = dir_mtime_ns(path);
            let rec = prior_paths.get(path).copied();
            // Same directory, same mtime → its direct entries are unchanged, so
            // the sizes of its own files can come from the index.
            let files_cached = has_prior
                && mtime != 0
                && rec
                    .and_then(|ix| prior_mtimes.get(ix as usize).copied())
                    .is_some_and(|m| m == mtime);

            di.lock().unwrap().insert(
                path.to_path_buf(),
                DirInfo {
                    mtime_ns: mtime,
                    files_cached,
                },
            );

            if files_cached {
                // Their sizes are served from the index; don't walk or stat them.
                children.retain(|e| e.as_ref().map(|e| e.file_type.is_dir()).unwrap_or(false));
            }

            for entry in children.iter_mut().flatten() {
                if entry.file_type.is_dir() {
                    // A directory with no changed path at or below it can be served
                    // whole from the index — don't descend.
                    if let Some(dirty) = &dirty {
                        let child_path = entry.path();
                        if !dirty.contains(&child_path)
                            && let Some(&ix) = prior_paths.get(&child_path)
                        {
                            entry.client_state.reused = Some(ix);
                            entry.read_children_path = None;
                        }
                    }
                } else if entry.file_type.is_file()
                    && let Ok(md) = std::fs::symlink_metadata(entry.path())
                {
                    let sz = counted_size(&md, &hl);
                    entry.client_state.size = sz;
                    prog.files.fetch_add(1, Ordering::Relaxed);
                    prog.bytes.fetch_add(sz, Ordering::Relaxed);
                }
            }
        });

    let mut nodes: Vec<Node> = Vec::new();
    // Transient path → index map, used only to wire up parent/child links during
    // the build; dropped before the tree is returned.
    let mut index: HashMap<PathBuf, usize> = HashMap::new();
    // Child adjacency over the cached index, built once and only when reusing.
    let prior_kids: Option<Vec<Vec<u32>>> = reuse.as_ref().map(|r| children_of(r.prior));

    let get_or_create =
        |nodes: &mut Vec<Node>, index: &mut HashMap<PathBuf, usize>, p: &Path| -> usize {
            if let Some(&i) = index.get(p) {
                return i;
            }
            let i = nodes.len();
            nodes.push(new_dir(file_label(p)));
            index.insert(p.to_path_buf(), i);
            i
        };

    let mut last_snapshot = Instant::now();
    for entry in walk {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        let idx = get_or_create(&mut nodes, &mut index, &path);
        let is_dir = entry.file_type.is_dir();
        nodes[idx].is_dir = is_dir;
        if !is_dir {
            nodes[idx].size = entry.client_state.size;
            nodes[idx].file_count = 1;
        }
        if path != root
            && let Some(parent) = path.parent()
        {
            let pidx = get_or_create(&mut nodes, &mut index, parent);
            nodes[idx].parent = Some(pidx);
            nodes[pidx].children.push(idx);
        }

        if is_dir && let Some(r) = &reuse {
            let info = dir_info.lock().unwrap().get(&path).copied();
            if let Some(info) = info {
                nodes[idx].mtime_ns = info.mtime_ns;
                if info.files_cached
                    && let Some(&rec) = paths_for_build.get(&path)
                {
                    attach_cached_files(&mut nodes, idx, r.prior, rec, &progress);
                }
            }
            // A pruned directory: rebuild its entire subtree from the index.
            if let Some(rec) = entry.client_state.reused
                && let Some(kids) = &prior_kids
            {
                nodes[idx].mtime_ns = r.prior.dirs[rec as usize].mtime_ns;
                attach_cached_subtree(&mut nodes, idx, r.prior, kids, rec, &progress);
            }
        } else if is_dir && let Some(info) = dir_info.lock().unwrap().get(&path) {
            nodes[idx].mtime_ns = info.mtime_ns;
        }

        // Periodically hand the UI a browsable snapshot of what we have so far.
        // Skipped entirely (no clone/aggregate cost) for non-streaming callers.
        if let Some(iv) = interval
            && last_snapshot.elapsed() >= iv
            && let Some(&ri) = index.get(&root)
        {
            on_snapshot(snapshot(&nodes, ri, &root));
            last_snapshot = Instant::now();
        }
    }

    if nodes.is_empty() {
        nodes.push(new_dir(file_label(&root)));
        index.insert(root.clone(), 0);
    }
    let root_idx = *index.get(&root).unwrap_or(&0);
    drop(index); // free the transient path map before we return the tree

    aggregate(&mut nodes, root_idx);
    sort_children(&mut nodes);

    progress.done.store(true, Ordering::Relaxed);
    Tree {
        nodes,
        root: root_idx,
        root_path: root,
    }
}

/// Child adjacency list over a cached index.
fn children_of(prior: &Index) -> Vec<Vec<u32>> {
    let mut kids: Vec<Vec<u32>> = vec![Vec::new(); prior.dirs.len()];
    for (i, d) in prior.dirs.iter().enumerate() {
        if let Some(p) = d.parent
            && (p as usize) < kids.len()
        {
            kids[p as usize].push(i as u32);
        }
    }
    kids
}

/// Attach the cached file children of one directory: large files individually,
/// everything smaller as a single synthetic node.
fn attach_cached_files(
    nodes: &mut Vec<Node>,
    parent: usize,
    prior: &Index,
    rec: u32,
    progress: &ScanProgress,
) {
    let d = &prior.dirs[rec as usize];
    for (name, sz) in &d.large {
        let fi = nodes.len();
        nodes.push(Node {
            name: name.clone(),
            size: *sz,
            is_dir: false,
            parent: Some(parent),
            children: Vec::new(),
            file_count: 1,
            removed: false,
            mtime_ns: 0,
            synthetic: false,
        });
        nodes[parent].children.push(fi);
        progress.files.fetch_add(1, Ordering::Relaxed);
        progress.bytes.fetch_add(*sz, Ordering::Relaxed);
    }
    if d.own_files > 0 {
        let fi = nodes.len();
        nodes.push(Node {
            name: format!("({} small files)", d.own_files),
            size: d.own_bytes,
            is_dir: false,
            parent: Some(parent),
            children: Vec::new(),
            file_count: d.own_files,
            removed: false,
            mtime_ns: 0,
            synthetic: true,
        });
        nodes[parent].children.push(fi);
        progress.files.fetch_add(d.own_files, Ordering::Relaxed);
        progress.bytes.fetch_add(d.own_bytes, Ordering::Relaxed);
    }
}

/// Rebuild a whole cached subtree (directories and files) beneath `parent`.
fn attach_cached_subtree(
    nodes: &mut Vec<Node>,
    parent: usize,
    prior: &Index,
    kids: &[Vec<u32>],
    rec: u32,
    progress: &ScanProgress,
) {
    attach_cached_files(nodes, parent, prior, rec, progress);
    for &child in &kids[rec as usize] {
        let d = &prior.dirs[child as usize];
        let ci = nodes.len();
        nodes.push(Node {
            name: d.name.clone(),
            size: 0,
            is_dir: true,
            parent: Some(parent),
            children: Vec::new(),
            file_count: 0,
            removed: false,
            mtime_ns: d.mtime_ns,
            synthetic: false,
        });
        nodes[parent].children.push(ci);
        attach_cached_subtree(nodes, ci, prior, kids, child, progress);
    }
}

/// Build an immutable snapshot of the in-progress arena for the UI: clone the
/// nodes (so scanning keeps mutating the originals), then aggregate sizes and
/// sort children on the copy. Directory self-sizes in the arena stay 0 during
/// the walk, so re-aggregating a fresh clone each time is always correct.
fn snapshot(nodes: &[Node], root_idx: usize, root_path: &Path) -> Tree {
    let mut nodes = nodes.to_vec();
    aggregate(&mut nodes, root_idx);
    sort_children(&mut nodes);
    Tree {
        nodes,
        root: root_idx,
        root_path: root_path.to_path_buf(),
    }
}

/// Iterative post-order aggregation of directory sizes and file counts.
fn aggregate(nodes: &mut [Node], root: usize) {
    let mut stack: Vec<(usize, bool)> = vec![(root, false)];
    while let Some((i, processed)) = stack.pop() {
        if processed {
            let mut total = nodes[i].size;
            let mut fc = nodes[i].file_count;
            for k in 0..nodes[i].children.len() {
                let ch = nodes[i].children[k];
                total += nodes[ch].size;
                fc += nodes[ch].file_count;
            }
            nodes[i].size = total;
            nodes[i].file_count = fc;
        } else {
            stack.push((i, true));
            for k in 0..nodes[i].children.len() {
                stack.push((nodes[i].children[k], false));
            }
        }
    }
}

/// Sort every directory's children by descending size.
fn sort_children(nodes: &mut [Node]) {
    for i in 0..nodes.len() {
        if nodes[i].is_dir && !nodes[i].children.is_empty() {
            let mut kids = std::mem::take(&mut nodes[i].children);
            kids.sort_by(|&a, &b| nodes[b].size.cmp(&nodes[a].size));
            nodes[i].children = kids;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Build a small directory tree in a unique temp dir. Returns its path.
    fn make_tree(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("diskscour_{}_{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("a/b")).unwrap();
        fs::create_dir_all(base.join("c")).unwrap();
        fs::write(base.join("a/f1"), vec![0u8; 4096]).unwrap();
        fs::write(base.join("a/b/f2"), vec![0u8; 8192]).unwrap();
        fs::write(base.join("c/f3"), vec![0u8; 2048]).unwrap();
        base
    }

    #[test]
    fn streaming_snapshots_are_monotonic_and_match_final() {
        let base = make_tree("stream");
        let prog = Arc::new(ScanProgress::default());

        // Force a snapshot on every entry so streaming is exercised deterministically.
        let mut snaps = 0usize;
        let mut last_total = 0u64;
        let mut last_files = 0u64;
        let final_tree = scan_impl(
            base.clone(),
            prog,
            |snap| {
                snaps += 1;
                let total = snap.nodes[snap.root].size;
                let files = snap.nodes[snap.root].file_count;
                // Growing arena → root totals never shrink between snapshots.
                assert!(total >= last_total, "root size regressed across snapshots");
                assert!(files >= last_files, "file count regressed across snapshots");
                last_total = total;
                last_files = files;
            },
            Some(Duration::ZERO),
            None,
        );

        assert!(snaps >= 1, "expected at least one snapshot to fire");
        let final_total = final_tree.nodes[final_tree.root].size;
        let final_files = final_tree.nodes[final_tree.root].file_count;
        assert_eq!(final_files, 3, "three files expected under the tree");
        assert!(final_total > 0);
        // The last snapshot reflects the fully-walked arena, so it matches the final.
        assert_eq!(last_files, final_files);
        assert_eq!(last_total, final_total);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn streaming_final_equals_non_streaming_scan() {
        let base = make_tree("equal");
        let a = scan_impl(
            base.clone(),
            Arc::new(ScanProgress::default()),
            |_| {},
            None,
            None,
        );
        let b = scan_streaming(base.clone(), Arc::new(ScanProgress::default()), |_| {});
        assert_eq!(a.nodes[a.root].size, b.nodes[b.root].size);
        assert_eq!(a.nodes[a.root].file_count, b.nodes[b.root].file_count);
        let _ = fs::remove_dir_all(&base);
    }
}
