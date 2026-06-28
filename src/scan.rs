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

/// Live progress shared with the UI thread during a scan.
#[derive(Default)]
pub struct ScanProgress {
    pub files: AtomicU64,
    pub bytes: AtomicU64,
    pub done: AtomicBool,
}

/// One node in the size tree (file or directory).
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
    }
}

/// Scan `root`, reporting progress via `progress`, and return the size tree.
pub fn scan(root: PathBuf, progress: Arc<ScanProgress>) -> Tree {
    use jwalk::WalkDirGeneric;

    let prog = progress.clone();
    let hardlinks: Arc<Mutex<HashSet<(u64, u64)>>> = Arc::new(Mutex::new(HashSet::new()));
    let hl = hardlinks.clone();
    // DirEntryState = u64 carries the allocated size of each entry.
    let walk = WalkDirGeneric::<((), u64)>::new(&root)
        .skip_hidden(false)
        .follow_links(false)
        .process_read_dir(move |_depth, _path, _state, children| {
            for entry in children.iter_mut().flatten() {
                if entry.file_type.is_file() {
                    if let Ok(md) = std::fs::symlink_metadata(entry.path()) {
                        let sz = counted_size(&md, &hl);
                        entry.client_state = sz;
                        prog.files.fetch_add(1, Ordering::Relaxed);
                        prog.bytes.fetch_add(sz, Ordering::Relaxed);
                    }
                } else {
                    // directories and symlinks contribute no self-size
                    entry.client_state = 0;
                }
            }
        });

    let mut nodes: Vec<Node> = Vec::new();
    // Transient path → index map, used only to wire up parent/child links during
    // the build; dropped before the tree is returned.
    let mut index: HashMap<PathBuf, usize> = HashMap::new();

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
            nodes[idx].size = entry.client_state;
            nodes[idx].file_count = 1;
        }
        if path != root
            && let Some(parent) = path.parent()
        {
            let pidx = get_or_create(&mut nodes, &mut index, parent);
            nodes[idx].parent = Some(pidx);
            nodes[pidx].children.push(idx);
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
