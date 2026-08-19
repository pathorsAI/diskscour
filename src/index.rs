//! Persistent, directory-granular index of a scanned root.
//!
//! A full scan produces a [`crate::scan::Tree`] with a node per file — millions
//! of them for a home directory. That is far too much to write to disk on every
//! scan, and incremental refresh only ever reuses whole *directories* anyway, so
//! the on-disk index keeps one record per directory plus the individually
//! interesting files (>= [`Index::threshold`]). Everything smaller is collapsed
//! into the directory's `own_bytes` / `own_files` totals and re-materialized as a
//! single synthetic "N small files" node.
//!
//! The index also carries what incremental refresh needs to decide what changed:
//! each directory's mtime (see [`crate::scan`] for how that is used) and the
//! FSEvents stream position at the end of the scan (see [`crate::fsevents`]).
//!
//! Format is a hand-rolled little-endian binary blob — JSON for half a million
//! directories would cost more to parse than the rescan it is meant to avoid.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::scan::{Node, Tree};

const MAGIC: &[u8; 4] = b"DSCR";
const FORMAT_VERSION: u32 = 1;

/// Files at or above this size get their own record; smaller ones are collapsed.
pub const DEFAULT_THRESHOLD: u64 = 1 << 20; // 1 MiB

/// No parent (the root record).
const NO_PARENT: u32 = u32::MAX;

/// How the index was produced. Surfaced to callers so a consumer can tell how
/// much to trust the numbers before acting on them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Every directory was walked and every file stat'ed.
    Full,
    /// Unchanged directories reused cached file sizes (no per-file stat).
    Mtime,
    /// Only subtrees reported changed by FSEvents were re-walked.
    Events,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Full => "full",
            Mode::Mtime => "mtime",
            Mode::Events => "events",
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            Mode::Full => 0,
            Mode::Mtime => 1,
            Mode::Events => 2,
        }
    }

    fn from_u8(b: u8) -> Mode {
        match b {
            1 => Mode::Mtime,
            2 => Mode::Events,
            _ => Mode::Full,
        }
    }
}

/// One directory in the index.
#[derive(Clone)]
pub struct DirRec {
    pub parent: Option<u32>,
    pub name: String,
    /// Directory mtime in nanoseconds since the epoch, or 0 if unavailable.
    pub mtime_ns: i64,
    /// Bytes/count of this directory's own files that were collapsed (below the
    /// large-file threshold). Files at or above it live in `large` instead.
    pub own_bytes: u64,
    pub own_files: u64,
    /// Aggregate over this directory and everything below it, large files included.
    pub subtree_bytes: u64,
    pub subtree_files: u64,
    /// Individually-tracked files directly in this directory: (name, bytes).
    pub large: Vec<(String, u64)>,
}

pub struct Index {
    pub root: PathBuf,
    /// Unix seconds when the scan that produced this index finished.
    pub scanned_at: u64,
    pub mode: Mode,
    /// Device the root lives on. FSEvents ids are only meaningful within one volume.
    pub device_id: u64,
    /// FSEvents stream position at the end of the scan, or 0 if unavailable.
    pub last_event_id: u64,
    pub threshold: u64,
    pub dirs: Vec<DirRec>,
    pub root_ix: u32,
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Directory holding all cached indexes.
///
/// `Caches/` is the honest home for this: the index is regenerable, and the
/// worst consequence of the system purging it is one slower scan.
#[cfg(not(test))]
pub fn cache_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join("Library/Caches/com.pathors.diskscour")
            .join("roots"),
    )
}

/// Under test, indexes go to a scratch directory. Tests scan temp trees, and
/// without this every `cargo test` run would leave entries in the real user
/// cache for directories that no longer exist. A `OnceLock` rather than an env
/// var keeps it race-free under the parallel test runner.
#[cfg(test)]
pub fn cache_dir() -> Option<PathBuf> {
    use std::sync::OnceLock;
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    Some(
        DIR.get_or_init(|| {
            std::env::temp_dir().join(format!("diskscour_test_cache_{}", std::process::id()))
        })
        .clone(),
    )
}

/// FNV-1a over the canonical root path — stable, dependency-free, and only ever
/// used to name a cache file (never for integrity).
fn root_key(root: &Path) -> String {
    let s = root.to_string_lossy();
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Path of the cache file for `root`.
pub fn index_path(root: &Path) -> Option<PathBuf> {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    Some(cache_dir()?.join(format!("{}.idx", root_key(&canonical))))
}

/// Device id of the filesystem containing `path`, used to scope FSEvents ids.
pub fn device_of(path: &Path) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).map(|m| m.dev()).unwrap_or(0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        0
    }
}

// ---- binary encoding --------------------------------------------------------

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_i64(out: &mut Vec<u8>, v: i64) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn string(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        // Guard against a corrupt length claiming more than the file holds.
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
}

impl Index {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.dirs.len() * 64);
        out.extend_from_slice(MAGIC);
        put_u32(&mut out, FORMAT_VERSION);
        put_str(&mut out, &self.root.to_string_lossy());
        put_u64(&mut out, self.scanned_at);
        out.push(self.mode.to_u8());
        put_u64(&mut out, self.device_id);
        put_u64(&mut out, self.last_event_id);
        put_u64(&mut out, self.threshold);
        put_u32(&mut out, self.root_ix);
        put_u32(&mut out, self.dirs.len() as u32);
        for d in &self.dirs {
            put_u32(&mut out, d.parent.unwrap_or(NO_PARENT));
            put_str(&mut out, &d.name);
            put_i64(&mut out, d.mtime_ns);
            put_u64(&mut out, d.own_bytes);
            put_u64(&mut out, d.own_files);
            put_u64(&mut out, d.subtree_bytes);
            put_u64(&mut out, d.subtree_files);
            put_u32(&mut out, d.large.len() as u32);
            for (name, sz) in &d.large {
                put_str(&mut out, name);
                put_u64(&mut out, *sz);
            }
        }
        out
    }

    pub fn decode(buf: &[u8]) -> Option<Index> {
        let mut c = Cursor { buf, pos: 0 };
        if c.take(4)? != MAGIC {
            return None;
        }
        if c.u32()? != FORMAT_VERSION {
            return None; // older/newer layout — treat as no cache, rescan.
        }
        let root = PathBuf::from(c.string()?);
        let scanned_at = c.u64()?;
        let mode = Mode::from_u8(*c.take(1)?.first()?);
        let device_id = c.u64()?;
        let last_event_id = c.u64()?;
        let threshold = c.u64()?;
        let root_ix = c.u32()?;
        let n = c.u32()? as usize;
        let mut dirs = Vec::with_capacity(n.min(1 << 20));
        for _ in 0..n {
            let parent = match c.u32()? {
                NO_PARENT => None,
                p => Some(p),
            };
            let name = c.string()?;
            let mtime_ns = c.i64()?;
            let own_bytes = c.u64()?;
            let own_files = c.u64()?;
            let subtree_bytes = c.u64()?;
            let subtree_files = c.u64()?;
            let ln = c.u32()? as usize;
            let mut large = Vec::with_capacity(ln.min(1 << 16));
            for _ in 0..ln {
                let nm = c.string()?;
                let sz = c.u64()?;
                large.push((nm, sz));
            }
            dirs.push(DirRec {
                parent,
                name,
                mtime_ns,
                own_bytes,
                own_files,
                subtree_bytes,
                subtree_files,
                large,
            });
        }
        if dirs.is_empty() || root_ix as usize >= dirs.len() {
            return None;
        }
        Some(Index {
            root,
            scanned_at,
            mode,
            device_id,
            last_event_id,
            threshold,
            dirs,
            root_ix,
        })
    }

    /// Write to the cache location for this index's root. Writes to a temp file
    /// and renames, so a crash mid-write can never leave a half-written index
    /// that later decodes into wrong numbers.
    pub fn save(&self) -> std::io::Result<PathBuf> {
        let path = index_path(&self.root).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no HOME for cache dir")
        })?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("idx.tmp");
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&self.encode())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    /// Load the cached index for `root`, or `None` if absent/unreadable/stale-format.
    pub fn load(root: &Path) -> Option<Index> {
        let path = index_path(root)?;
        let mut f = std::fs::File::open(path).ok()?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).ok()?;
        let idx = Index::decode(&buf)?;
        // A cache file whose recorded root disagrees with what we asked for means
        // a hash collision or a moved cache — safer to ignore it.
        if idx.root != root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
            && idx.root != root
        {
            return None;
        }
        Some(idx)
    }

    pub fn age_secs(&self) -> u64 {
        now_secs().saturating_sub(self.scanned_at)
    }

    pub fn total_bytes(&self) -> u64 {
        self.dirs[self.root_ix as usize].subtree_bytes
    }

    pub fn total_files(&self) -> u64 {
        self.dirs[self.root_ix as usize].subtree_files
    }

    /// Absolute path of a directory record.
    pub fn path_of(&self, ix: u32) -> PathBuf {
        if ix == self.root_ix {
            return self.root.clone();
        }
        let mut parts: Vec<&str> = Vec::new();
        let mut cur = ix;
        while cur != self.root_ix {
            parts.push(self.dirs[cur as usize].name.as_str());
            match self.dirs[cur as usize].parent {
                Some(p) => cur = p,
                None => break,
            }
        }
        let mut p = self.root.clone();
        for name in parts.iter().rev() {
            p.push(name);
        }
        p
    }

    /// Map every directory path to its record index. Built on demand — callers
    /// that need it (incremental refresh) hold it for one scan and drop it.
    ///
    /// `from_tree` always assigns a parent a lower record index than its
    /// children, so one forward pass can extend each parent's path by a name.
    pub fn by_path(&self) -> HashMap<PathBuf, u32> {
        let mut paths: Vec<PathBuf> = Vec::with_capacity(self.dirs.len());
        for (i, d) in self.dirs.iter().enumerate() {
            let p = match d.parent {
                _ if i as u32 == self.root_ix => self.root.clone(),
                Some(par) if (par as usize) < i => paths[par as usize].join(&d.name),
                // Records out of order (or a detached parent) are rare enough to
                // pay the walk-up cost rather than complicate the fast path.
                _ => self.path_of(i as u32),
            };
            paths.push(p);
        }
        paths
            .into_iter()
            .enumerate()
            .map(|(i, p)| (p, i as u32))
            .collect()
    }
}

// ---- Tree <-> Index ---------------------------------------------------------

/// Build a compact index from a freshly scanned tree.
pub fn from_tree(tree: &Tree, mode: Mode, last_event_id: u64, threshold: u64) -> Index {
    let mut dirs: Vec<DirRec> = Vec::new();
    // tree node index -> dir record index, for the directories we keep.
    let mut map: HashMap<usize, u32> = HashMap::new();

    // Breadth-first so a record's parent always precedes it.
    let mut queue: Vec<(usize, Option<u32>)> = vec![(tree.root, None)];
    while let Some((node_ix, parent)) = queue.pop() {
        let n = &tree.nodes[node_ix];
        if n.removed {
            continue;
        }
        let rec_ix = dirs.len() as u32;
        map.insert(node_ix, rec_ix);
        let mut own_bytes = 0u64;
        let mut own_files = 0u64;
        let mut large: Vec<(String, u64)> = Vec::new();
        for &c in &n.children {
            let ch = &tree.nodes[c];
            if ch.removed {
                continue;
            }
            if ch.is_dir {
                queue.push((c, Some(rec_ix)));
            } else if ch.size >= threshold && !ch.synthetic {
                large.push((ch.name.clone(), ch.size));
            } else {
                // Small files, and any synthetic "N small files" node carried over
                // from a previous index, fold back into the collapsed totals.
                own_bytes += ch.size;
                own_files += ch.file_count;
            }
        }
        dirs.push(DirRec {
            parent,
            name: n.name.clone(),
            mtime_ns: n.mtime_ns,
            own_bytes,
            own_files,
            subtree_bytes: n.size,
            subtree_files: n.file_count,
            large,
        });
    }

    Index {
        root: tree.root_path.clone(),
        scanned_at: now_secs(),
        mode,
        device_id: device_of(&tree.root_path),
        last_event_id,
        threshold,
        dirs,
        root_ix: 0,
    }
}

/// Rebuild a browsable tree from the index alone, with no filesystem access.
/// Small files come back as one synthetic node per directory.
pub fn to_tree(idx: &Index) -> Tree {
    let mut nodes: Vec<Node> = Vec::with_capacity(idx.dirs.len() * 2);
    // record index -> tree node index
    let mut map: Vec<usize> = vec![usize::MAX; idx.dirs.len()];

    for (i, d) in idx.dirs.iter().enumerate() {
        let n = Node {
            name: d.name.clone(),
            size: d.subtree_bytes,
            is_dir: true,
            parent: None,
            children: Vec::new(),
            file_count: d.subtree_files,
            removed: false,
            mtime_ns: d.mtime_ns,
            synthetic: false,
        };
        map[i] = nodes.len();
        nodes.push(n);
    }
    // Wire directories to their parents.
    for (i, d) in idx.dirs.iter().enumerate() {
        if let Some(p) = d.parent {
            let (ci, pi) = (map[i], map[p as usize]);
            nodes[ci].parent = Some(pi);
            nodes[pi].children.push(ci);
        }
    }
    // Attach files: individually for large ones, collapsed for the rest.
    for (i, d) in idx.dirs.iter().enumerate() {
        let pi = map[i];
        for (name, sz) in &d.large {
            let fi = nodes.len();
            nodes.push(Node {
                name: name.clone(),
                size: *sz,
                is_dir: false,
                parent: Some(pi),
                children: Vec::new(),
                file_count: 1,
                removed: false,
                mtime_ns: 0,
                synthetic: false,
            });
            nodes[pi].children.push(fi);
        }
        if d.own_files > 0 {
            let fi = nodes.len();
            nodes.push(Node {
                name: format!("({} small files)", d.own_files),
                size: d.own_bytes,
                is_dir: false,
                parent: Some(pi),
                children: Vec::new(),
                file_count: d.own_files,
                removed: false,
                mtime_ns: 0,
                synthetic: true,
            });
            nodes[pi].children.push(fi);
        }
    }

    let root = map[idx.root_ix as usize];
    let mut tree = Tree {
        nodes,
        root,
        root_path: idx.root.clone(),
    };
    tree.sort_children();
    tree
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Index {
        Index {
            root: PathBuf::from("/tmp/ds-test"),
            scanned_at: 1_700_000_000,
            mode: Mode::Full,
            device_id: 42,
            last_event_id: 999,
            threshold: 2048,
            root_ix: 0,
            dirs: vec![
                DirRec {
                    parent: None,
                    name: "ds-test".into(),
                    mtime_ns: 1234,
                    own_bytes: 100,
                    own_files: 2,
                    subtree_bytes: 5100,
                    subtree_files: 4,
                    large: vec![("big.bin".into(), 4000)],
                },
                DirRec {
                    parent: Some(0),
                    name: "sub".into(),
                    mtime_ns: 5678,
                    own_bytes: 1000,
                    own_files: 1,
                    subtree_bytes: 1000,
                    subtree_files: 1,
                    large: vec![],
                },
            ],
        }
    }

    #[test]
    fn roundtrips_through_binary() {
        let idx = sample();
        let bytes = idx.encode();
        let back = Index::decode(&bytes).expect("decode");
        assert_eq!(back.root, idx.root);
        assert_eq!(back.scanned_at, idx.scanned_at);
        assert_eq!(back.mode, Mode::Full);
        assert_eq!(back.device_id, 42);
        assert_eq!(back.last_event_id, 999);
        assert_eq!(back.dirs.len(), 2);
        assert_eq!(back.dirs[0].large, vec![("big.bin".to_string(), 4000)]);
        assert_eq!(back.dirs[1].name, "sub");
        assert_eq!(back.total_bytes(), 5100);
    }

    #[test]
    fn rejects_garbage_and_truncation() {
        assert!(Index::decode(b"not an index").is_none());
        let bytes = sample().encode();
        // Every truncation must fail cleanly rather than panic.
        for cut in 0..bytes.len() {
            let _ = Index::decode(&bytes[..cut]);
        }
        assert!(Index::decode(&bytes[..bytes.len() - 1]).is_none());
    }

    #[test]
    fn materializes_a_browsable_tree() {
        let idx = sample();
        let tree = to_tree(&idx);
        let root = &tree.nodes[tree.root];
        assert_eq!(root.size, 5100);
        assert_eq!(root.file_count, 4);
        // root children: sub/, big.bin, and one synthetic small-files node
        assert_eq!(root.children.len(), 3);
        let names: Vec<&str> = root
            .children
            .iter()
            .map(|&c| tree.nodes[c].name.as_str())
            .collect();
        assert!(names.contains(&"sub"));
        assert!(names.contains(&"big.bin"));
        assert!(names.iter().any(|n| n.contains("small files")));
        // Children are sorted biggest-first.
        let sizes: Vec<u64> = root.children.iter().map(|&c| tree.nodes[c].size).collect();
        assert!(sizes.windows(2).all(|w| w[0] >= w[1]));
    }

    #[test]
    fn tree_index_roundtrip_preserves_totals() {
        let idx = sample();
        let tree = to_tree(&idx);
        let back = from_tree(&tree, Mode::Full, 7, idx.threshold);
        assert_eq!(back.total_bytes(), idx.total_bytes());
        assert_eq!(back.total_files(), idx.total_files());
        assert_eq!(back.dirs.len(), idx.dirs.len());
        // The synthetic node folds back into own_* rather than becoming a file record.
        let root = &back.dirs[back.root_ix as usize];
        assert_eq!(root.own_files, 2);
        assert_eq!(root.own_bytes, 100);
        assert_eq!(root.large.len(), 1);
    }
}
