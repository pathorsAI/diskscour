//! Reading a directory's entries and their sizes in one syscall per batch.
//!
//! The obvious way to size a tree is `readdir` plus one `lstat` per file, which
//! is what most tools do. macOS offers `getattrlistbulk(2)`, which returns the
//! names *and* the attributes for many entries at once — measured on a 197k-file
//! `node_modules`, that is 0.72s against 5.25s for the same walk built from
//! `lstat` calls.
//!
//! It also carries the attribute that makes the reported numbers honest.
//! `st_blocks * 512` counts every block a file occupies, but on APFS a block can
//! belong to several files at once: `clonefile(2)` — which `bun install` and
//! `cp -c` use — gives each copy its own inode pointing at shared extents. Ask
//! `du` about thirteen cloned `node_modules` and it will happily report thirteen
//! times the space, none of which you get back by deleting them.
//! [`ATTR_CMNEXT_PRIVATESIZE`] is the number of bytes a file does *not* share
//! with any other file, which is exactly "what would deleting this give back".

#![cfg(target_os = "macos")]

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

// ---- libSystem bindings -----------------------------------------------------

const ATTR_BIT_MAP_COUNT: u16 = 5;

const ATTR_CMN_NAME: u32 = 0x0000_0001;
const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
const ATTR_CMN_MODTIME: u32 = 0x0000_0400;
const ATTR_CMN_FILEID: u32 = 0x0200_0000;
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;

const ATTR_FILE_LINKCOUNT: u32 = 0x0000_0001;
const ATTR_FILE_ALLOCSIZE: u32 = 0x0000_0004;

const ATTR_CMNEXT_PRIVATESIZE: u32 = 0x0000_0008;

/// Required for any `ATTR_CMNEXT_*` attribute to be honoured.
const FSOPT_ATTR_CMN_EXTENDED: c_uint = 0x0000_0020;

const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

#[repr(C)]
#[derive(Default)]
struct AttrList {
    bitmapcount: u16,
    reserved: u16,
    commonattr: u32,
    volattr: u32,
    dirattr: u32,
    fileattr: u32,
    forkattr: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AttributeSet {
    commonattr: u32,
    volattr: u32,
    dirattr: u32,
    fileattr: u32,
    forkattr: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AttrReference {
    data_offset: i32,
    length: u32,
}

unsafe extern "C" {
    fn open(path: *const c_char, flags: c_int, ...) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn getattrlistbulk(
        dirfd: c_int,
        attrlist: *const AttrList,
        attr_buf: *mut c_void,
        attr_buf_size: usize,
        options: u64,
    ) -> c_int;
}

const O_RDONLY: c_int = 0x0000;
const O_DIRECTORY: c_int = 0x0010_0000;

/// One entry in a directory.
pub struct Entry {
    pub name: String,
    /// True only for regular files. Directories are walked by the caller, and
    /// symlinks and devices contribute no size, so both land on `false`.
    pub is_file: bool,
    /// Directory mtime in nanoseconds. Read here so the walker never needs a
    /// separate `stat` per directory to decide whether it changed.
    pub mtime_ns: i64,
    /// Bytes allocated to this file, shared blocks included. What `du` reports.
    pub allocated: u64,
    /// Bytes allocated to this file *exclusively* — what deleting it frees.
    /// Equals `allocated` for an ordinary file; 0 for a file whose every block
    /// is shared with a clone elsewhere.
    pub private: u64,
    pub fileid: u64,
    pub nlink: u32,
}

/// Buffer size per syscall. 64 KiB holds a few hundred entries, which is enough
/// that even a large directory takes only a handful of calls.
const BUF_LEN: usize = 64 * 1024;

/// Read every entry of `dir` with its sizes. Returns `None` when the directory
/// cannot be opened or the filesystem does not support the bulk interface, so
/// callers can fall back to `lstat`.
pub fn read_dir(dir: &Path) -> Option<Vec<Entry>> {
    let c_path = CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c_path` is a valid NUL-terminated string for the duration of the call.
    let fd = unsafe { open(c_path.as_ptr(), O_RDONLY | O_DIRECTORY) };
    if fd < 0 {
        return None;
    }
    let out = read_fd(fd, dir);
    // SAFETY: `fd` was returned by `open` above and is not used afterwards.
    unsafe { close(fd) };
    out
}

fn read_fd(fd: c_int, dir: &Path) -> Option<Vec<Entry>> {
    let attrs = AttrList {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS
            | ATTR_CMN_NAME
            | ATTR_CMN_OBJTYPE
            | ATTR_CMN_MODTIME
            | ATTR_CMN_FILEID,
        volattr: 0,
        dirattr: 0,
        fileattr: ATTR_FILE_LINKCOUNT | ATTR_FILE_ALLOCSIZE,
        forkattr: ATTR_CMNEXT_PRIVATESIZE,
    };

    let mut buf = vec![0u8; BUF_LEN];
    let mut out: Vec<Entry> = Vec::new();
    let mut first = true;

    loop {
        // SAFETY: `buf` is a live allocation of `BUF_LEN` bytes and `attrs` is a
        // well-formed attrlist; the kernel writes at most `BUF_LEN` bytes.
        let n = unsafe {
            getattrlistbulk(
                fd,
                &attrs,
                buf.as_mut_ptr() as *mut c_void,
                BUF_LEN,
                FSOPT_ATTR_CMN_EXTENDED as u64,
            )
        };
        if n < 0 {
            // Unsupported filesystem, or a genuine error. Either way the caller
            // must fall back rather than accept a partial listing — a mid-walk
            // failure would silently drop the rest of the directory.
            let _ = (dir, first);
            return None;
        }
        if n == 0 {
            break;
        }
        first = false;
        let mut off = 0usize;
        for _ in 0..n {
            let (entry, len) = parse_entry(&buf[off..])?;
            if let Some(e) = entry {
                out.push(e);
            }
            off += len;
        }
    }
    Some(out)
}

fn rd_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn rd_u64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// Decode one entry, returning it and the number of bytes it occupied.
///
/// Fields are packed with no alignment padding and appear in bit order —
/// common attributes first, then file attributes, then fork attributes — so the
/// cursor has to walk them in exactly that sequence, honouring the
/// `returned_attrs` mask because the kernel may omit any of them.
fn parse_entry(b: &[u8]) -> Option<(Option<Entry>, usize)> {
    let total = rd_u32(b, 0)? as usize;
    if total < 4 || total > b.len() {
        return None;
    }
    let mut p = 4usize;
    let returned = AttributeSet {
        commonattr: rd_u32(b, p)?,
        volattr: rd_u32(b, p + 4)?,
        dirattr: rd_u32(b, p + 8)?,
        fileattr: rd_u32(b, p + 12)?,
        forkattr: rd_u32(b, p + 16)?,
    };
    p += 20;

    let mut name = String::new();
    if returned.commonattr & ATTR_CMN_NAME != 0 {
        let r = AttrReference {
            data_offset: rd_u32(b, p)? as i32,
            length: rd_u32(b, p + 4)?,
        };
        let start = (p as i64 + r.data_offset as i64) as usize;
        // `length` counts the trailing NUL.
        let end = start + r.length.saturating_sub(1) as usize;
        name = String::from_utf8_lossy(b.get(start..end)?).into_owned();
        p += 8;
    }
    let mut objtype = 0u32;
    if returned.commonattr & ATTR_CMN_OBJTYPE != 0 {
        objtype = rd_u32(b, p)?;
        p += 4;
    }
    // struct timespec: two 64-bit words on every platform this builds for.
    let mut mtime_ns = 0i64;
    if returned.commonattr & ATTR_CMN_MODTIME != 0 {
        let secs = rd_u64(b, p)? as i64;
        let nsecs = rd_u64(b, p + 8)? as i64;
        mtime_ns = secs.saturating_mul(1_000_000_000).saturating_add(nsecs);
        p += 16;
    }
    let mut fileid = 0u64;
    if returned.commonattr & ATTR_CMN_FILEID != 0 {
        fileid = rd_u64(b, p)?;
        p += 8;
    }
    let mut nlink = 1u32;
    if returned.fileattr & ATTR_FILE_LINKCOUNT != 0 {
        nlink = rd_u32(b, p)?;
        p += 4;
    }
    let mut allocated = 0u64;
    if returned.fileattr & ATTR_FILE_ALLOCSIZE != 0 {
        allocated = rd_u64(b, p)?;
        p += 8;
    }
    let mut private = allocated;
    if returned.forkattr & ATTR_CMNEXT_PRIVATESIZE != 0 {
        private = rd_u64(b, p)?;
    }

    if name.is_empty() || name == "." || name == ".." {
        return Some((None, total));
    }
    let is_file = objtype == VREG;
    // Symlinks and devices contribute no size, matching the previous behaviour.
    let (allocated, private) = if is_file {
        (allocated, private)
    } else {
        (0, 0)
    };
    let _ = (VDIR, VLNK);

    Some((
        Some(Entry {
            name,
            is_file,
            mtime_ns,
            allocated,
            private,
            fileid,
            nlink,
        }),
        total,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn reads_names_and_sizes() {
        let base = std::env::temp_dir().join(format!("ds_bulk_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("sub")).unwrap();
        fs::write(base.join("a.bin"), vec![0u8; 40_000]).unwrap();

        let entries = read_dir(&base).expect("bulk read should work on APFS");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"a.bin"), "got {names:?}");
        assert!(names.contains(&"sub"));

        let f = entries.iter().find(|e| e.name == "a.bin").unwrap();
        assert!(f.is_file);
        assert!(f.allocated >= 40_000, "allocated {}", f.allocated);
        // A freshly written file shares nothing, so all of it is private.
        assert_eq!(f.private, f.allocated);
        assert!(f.fileid > 0);

        let d = entries.iter().find(|e| e.name == "sub").unwrap();
        assert!(!d.is_file, "a directory must not be counted as a file");
        assert_eq!(d.allocated, 0, "directories contribute no size here");
        assert!(
            d.mtime_ns > 0,
            "directory mtime should come back with the entry"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_clone_reports_no_private_bytes() {
        // The whole point of reading PRIVATESIZE: cloned files each report their
        // full allocated size, but deleting one frees nothing.
        let base = std::env::temp_dir().join(format!("ds_clone_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let orig = base.join("orig.bin");
        fs::write(&orig, vec![7u8; 2_000_000]).unwrap();

        let ok = std::process::Command::new("cp")
            .arg("-c")
            .arg(&orig)
            .arg(base.join("clone.bin"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            let _ = fs::remove_dir_all(&base);
            return; // not a cloning filesystem; nothing to assert
        }

        let entries = read_dir(&base).unwrap();
        let total_alloc: u64 = entries.iter().map(|e| e.allocated).sum();
        let total_private: u64 = entries.iter().map(|e| e.private).sum();
        assert!(
            total_alloc >= 4_000_000,
            "allocated should double-count the clone: {total_alloc}"
        );
        assert!(
            total_private < total_alloc / 2,
            "private should not double-count: alloc={total_alloc} private={total_private}"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn missing_directory_reports_failure() {
        assert!(read_dir(Path::new("/definitely/not/a/directory/xyz")).is_none());
    }
}
