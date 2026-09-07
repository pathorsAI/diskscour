//! Finding build outputs that something outside the tree depends on.
//!
//! A `target/` directory is nominally regenerable, which is why the cache rules
//! flag it. But `cargo build --release` is also how a lot of local tooling gets
//! installed: the binary stays in `target/release/` and is reached from
//! elsewhere — a symlink on `$PATH`, an editor or agent configured to launch it,
//! a process already running from it. Deleting the directory then breaks a
//! working tool rather than costing a rebuild, which is exactly the failure a
//! cleanup tool must not cause.
//!
//! Two signals are checked, both cheap and both computed once per command:
//!
//! * a symlink in any `$PATH` directory resolving into the tree, and
//! * a running process whose executable lives in the tree.
//!
//! Neither is exhaustive — nothing here can see a config file naming an absolute
//! path — so this is a guard against the common cases, not a proof of safety.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Why a path is considered in use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reason {
    /// Reachable as a command because something on `$PATH` links to it.
    OnPath,
    /// A process is running from this executable right now.
    Running,
}

impl Reason {
    pub fn describe(self) -> &'static str {
        match self {
            Reason::OnPath => "linked from a directory on $PATH",
            Reason::Running => "a process is running from it",
        }
    }
}

/// Executables outside-the-tree references point at, with the reason for each.
/// Built once and queried with [`Refs::protecting`].
pub struct Refs {
    entries: HashMap<PathBuf, Reason>,
}

impl Refs {
    /// Gather every reference we can see. Cost is a listing of each `$PATH`
    /// directory plus one pass over the process table.
    pub fn collect() -> Refs {
        let mut entries: HashMap<PathBuf, Reason> = HashMap::new();
        for p in running_executables() {
            entries.insert(p, Reason::Running);
        }
        // A symlink on $PATH is the more actionable message, so let it win.
        for p in path_symlink_targets() {
            entries.insert(p, Reason::OnPath);
        }
        Refs { entries }
    }

    /// The referenced executables that live under `dir`, if any. A non-empty
    /// result means deleting `dir` would break something currently in use.
    pub fn protecting(&self, dir: &Path) -> Vec<(&Path, Reason)> {
        let canonical = dir.canonicalize();
        let dir_real = canonical.as_deref().unwrap_or(dir);
        self.entries
            .iter()
            .filter(|(p, _)| p.starts_with(dir) || p.starts_with(dir_real))
            .map(|(p, r)| (p.as_path(), *r))
            .collect()
    }
}

/// Resolved targets of every symlink sitting in a `$PATH` directory.
fn path_symlink_targets() -> Vec<PathBuf> {
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for dir in std::env::split_paths(&path) {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            // Only symlinks matter: a real binary copied onto $PATH is already
            // independent of wherever it was built.
            let Ok(md) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if !md.file_type().is_symlink() {
                continue;
            }
            if let Ok(target) = std::fs::canonicalize(&p) {
                out.push(target);
            }
        }
    }
    out
}

/// The executable behind a pid, straight from the kernel — unlike `ps`
/// output, this survives paths with spaces (`~/Library/Application Support/…`).
#[cfg(target_os = "macos")]
pub fn exe_of_pid(pid: i32) -> Option<PathBuf> {
    use std::os::raw::{c_int, c_void};
    const PROC_PIDPATHINFO_MAXSIZE: usize = 4 * 1024;
    unsafe extern "C" {
        fn proc_pidpath(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
    }
    let mut buf = vec![0u8; PROC_PIDPATHINFO_MAXSIZE];
    // SAFETY: writes at most `buffersize` bytes into a buffer we own.
    let len = unsafe {
        proc_pidpath(
            pid,
            buf.as_mut_ptr() as *mut c_void,
            PROC_PIDPATHINFO_MAXSIZE as u32,
        )
    };
    if len <= 0 {
        return None;
    }
    std::str::from_utf8(&buf[..len as usize])
        .ok()
        .map(PathBuf::from)
}

#[cfg(not(target_os = "macos"))]
pub fn exe_of_pid(_pid: i32) -> Option<PathBuf> {
    None
}

#[cfg(target_os = "macos")]
fn running_executables() -> Vec<PathBuf> {
    use std::os::raw::{c_int, c_void};

    const PROC_ALL_PIDS: u32 = 1;
    const PROC_PIDPATHINFO_MAXSIZE: usize = 4 * 1024;

    unsafe extern "C" {
        fn proc_listpids(kind: u32, typeinfo: u32, buffer: *mut c_void, buffersize: c_int)
        -> c_int;
        fn proc_pidpath(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
    }

    // SAFETY: both calls write at most the byte count we pass, into buffers we own.
    unsafe {
        let n = proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0);
        if n <= 0 {
            return Vec::new();
        }
        // The table can grow between the sizing call and the real one; ask for
        // extra room rather than risk a truncated list.
        let cap = (n as usize / 4) + 128;
        let mut pids = vec![0i32; cap];
        let got = proc_listpids(
            PROC_ALL_PIDS,
            0,
            pids.as_mut_ptr() as *mut c_void,
            (cap * 4) as c_int,
        );
        if got <= 0 {
            return Vec::new();
        }
        let count = got as usize / 4;

        let mut out = Vec::new();
        let mut buf = vec![0u8; PROC_PIDPATHINFO_MAXSIZE];
        for &pid in pids.iter().take(count) {
            if pid == 0 {
                continue;
            }
            let len = proc_pidpath(
                pid,
                buf.as_mut_ptr() as *mut c_void,
                PROC_PIDPATHINFO_MAXSIZE as u32,
            );
            // A zero or negative length just means we can't see that process
            // (permissions, or it exited); other processes still tell us plenty.
            if len > 0
                && let Ok(s) = std::str::from_utf8(&buf[..len as usize])
            {
                out.push(PathBuf::from(s));
            }
        }
        out
    }
}

#[cfg(not(target_os = "macos"))]
fn running_executables() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sees_this_test_binary_as_running() {
        let refs = Refs::collect();
        let me = std::env::current_exe().expect("current exe");
        let dir = me.parent().expect("parent");
        let hits = refs.protecting(dir);
        assert!(
            !hits.is_empty(),
            "the running test binary should protect its own directory"
        );
        assert!(hits.iter().any(|(_, r)| *r == Reason::Running));
    }

    #[test]
    fn an_unrelated_directory_is_not_protected() {
        let refs = Refs::collect();
        let base = std::env::temp_dir().join(format!("ds_live_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        assert!(refs.protecting(&base).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn reasons_read_as_explanations() {
        assert!(Reason::OnPath.describe().contains("PATH"));
        assert!(Reason::Running.describe().contains("running"));
    }
}
