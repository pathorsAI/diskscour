//! Finding what changed since the last scan, via the macOS FSEvents history.
//!
//! Every volume keeps a persistent log of filesystem changes in `/.fseventsd`.
//! Handing a previously-recorded [`FSEventStreamEventId`] to `FSEventStreamCreate`
//! as `sinceWhen` replays that log: the callback receives the historical events
//! first, then a `HistoryDone` marker, then live events. Recording the stream
//! position at the end of each scan therefore lets the next scan ask the kernel
//! which directories to look at instead of walking the whole tree.
//!
//! The history is not guaranteed. It can be purged, it can overflow, and it does
//! not exist at all on network mounts. Every one of those cases surfaces here as
//! [`Replay::Unavailable`] so the caller falls back to a slower but always-correct
//! strategy — this module never reports "nothing changed" when it does not know.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Outcome of asking for the changes since a recorded stream position.
pub enum Replay {
    /// Directories that changed. May be empty, which genuinely means "nothing".
    Changed(Vec<PathBuf>),
    /// The history could not be trusted; the caller must not prune from this.
    Unavailable(&'static str),
}

/// Longest we will pump the run loop waiting for the history replay to finish.
/// Reaching it means the log is unexpectedly large or slow; falling back costs
/// a slower scan, whereas waiting forever would hang the app.
const REPLAY_TIMEOUT_SECS: f64 = 10.0;
/// After the history marker, keep listening until the stream has been quiet
/// for this long — events still on their way to fseventsd when the replay was
/// requested arrive live, a little after the marker.
const SETTLE_SECS: f64 = 0.2;
const SETTLE_SLICE_SECS: f64 = 0.05;
/// Upper bound on the settle wait, however busy the volume is.
const SETTLE_MAX_SECS: f64 = 2.0;

#[cfg(target_os = "macos")]
mod imp {
    use super::{REPLAY_TIMEOUT_SECS, Replay, SETTLE_MAX_SECS, SETTLE_SECS, SETTLE_SLICE_SECS};
    use std::ffi::CStr;
    use std::os::raw::{c_char, c_uint, c_void};
    use std::path::{Path, PathBuf};

    use core_foundation_sys::array::{CFArrayCreate, CFArrayRef, kCFTypeArrayCallBacks};
    use core_foundation_sys::base::{CFAllocatorRef, CFIndex, CFRelease, kCFAllocatorDefault};
    use core_foundation_sys::runloop::{
        CFRunLoopGetCurrent, CFRunLoopRef, CFRunLoopRunInMode, kCFRunLoopDefaultMode,
    };
    use core_foundation_sys::string::{
        CFStringCreateWithBytes, CFStringRef, kCFStringEncodingUTF8,
    };

    // Declared here rather than pulled from `fsevent-sys`: that crate now depends
    // on `core-foundation` and `dispatch2`, which is a lot of tree for the eight
    // symbols below. Signatures mirror <CoreServices/FSEvents.h>.
    type FSEventStreamRef = *mut c_void;
    type FSEventStreamEventId = u64;
    type FSEventStreamCreateFlags = c_uint;
    type FSEventStreamEventFlags = c_uint;
    type CFTimeInterval = f64;
    type Boolean = u8;

    type FSEventStreamCallback = extern "C" fn(
        FSEventStreamRef,
        *mut c_void,                    // clientCallBackInfo
        usize,                          // numEvents
        *mut c_void,                    // eventPaths (char** unless UseCFTypes)
        *const FSEventStreamEventFlags, // eventFlags[]
        *const FSEventStreamEventId,    // eventIds[]
    );

    #[repr(C)]
    struct FSEventStreamContext {
        version: CFIndex,
        info: *mut c_void,
        retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
        release: Option<extern "C" fn(*const c_void)>,
        copy_description: Option<extern "C" fn(*const c_void) -> CFStringRef>,
    }

    const CREATE_FLAG_NO_DEFER: FSEventStreamCreateFlags = 0x0000_0002;
    const CREATE_FLAG_WATCH_ROOT: FSEventStreamCreateFlags = 0x0000_0004;

    const FLAG_MUST_SCAN_SUBDIRS: FSEventStreamEventFlags = 0x0000_0001;
    const FLAG_USER_DROPPED: FSEventStreamEventFlags = 0x0000_0002;
    const FLAG_KERNEL_DROPPED: FSEventStreamEventFlags = 0x0000_0004;
    const FLAG_EVENT_IDS_WRAPPED: FSEventStreamEventFlags = 0x0000_0008;
    const FLAG_HISTORY_DONE: FSEventStreamEventFlags = 0x0000_0010;
    const FLAG_ROOT_CHANGED: FSEventStreamEventFlags = 0x0000_0020;
    const FLAG_MOUNT: FSEventStreamEventFlags = 0x0000_0040;
    const FLAG_UNMOUNT: FSEventStreamEventFlags = 0x0000_0080;

    /// Flags that mean "this report is incomplete — go look for yourself".
    /// Treating any of them as fatal is deliberate: a missed change would surface
    /// as a wrong size, which is worse than a slow scan.
    const UNTRUSTWORTHY: FSEventStreamEventFlags = FLAG_MUST_SCAN_SUBDIRS
        | FLAG_USER_DROPPED
        | FLAG_KERNEL_DROPPED
        | FLAG_EVENT_IDS_WRAPPED
        | FLAG_ROOT_CHANGED
        | FLAG_MOUNT
        | FLAG_UNMOUNT;

    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn FSEventStreamCreate(
            allocator: CFAllocatorRef,
            callback: FSEventStreamCallback,
            context: *const FSEventStreamContext,
            paths_to_watch: CFArrayRef,
            since_when: FSEventStreamEventId,
            latency: CFTimeInterval,
            flags: FSEventStreamCreateFlags,
        ) -> FSEventStreamRef;
        fn FSEventStreamScheduleWithRunLoop(
            stream: FSEventStreamRef,
            run_loop: CFRunLoopRef,
            mode: CFStringRef,
        );
        fn FSEventStreamStart(stream: FSEventStreamRef) -> Boolean;
        fn FSEventStreamStop(stream: FSEventStreamRef);
        fn FSEventStreamFlushSync(stream: FSEventStreamRef);
        fn FSEventStreamInvalidate(stream: FSEventStreamRef);
        fn FSEventStreamRelease(stream: FSEventStreamRef);
        fn FSEventsGetCurrentEventId() -> FSEventStreamEventId;
    }

    struct Collect {
        paths: Vec<PathBuf>,
        history_done: bool,
        dropped: bool,
    }

    extern "C" fn callback(
        _stream: FSEventStreamRef,
        info: *mut c_void,
        num: usize,
        event_paths: *mut c_void,
        flags: *const FSEventStreamEventFlags,
        _ids: *const FSEventStreamEventId,
    ) {
        // SAFETY: `info` is the `Collect` handed to FSEventStreamCreate via the
        // stream context, and it outlives the stream (see `changed_since`). The
        // flag and path arrays are valid for `num` elements for this call only,
        // and the stream runs on this thread's run loop, so nothing aliases.
        unsafe {
            let state = &mut *(info as *mut Collect);
            let paths = event_paths as *const *const c_char;
            for i in 0..num {
                let f = *flags.add(i);
                if f & FLAG_HISTORY_DONE != 0 {
                    state.history_done = true;
                    continue;
                }
                if f & UNTRUSTWORTHY != 0 {
                    state.dropped = true;
                }
                let p = *paths.add(i);
                if p.is_null() {
                    continue;
                }
                if let Ok(s) = CStr::from_ptr(p).to_str() {
                    state.paths.push(PathBuf::from(s));
                }
            }
        }
    }

    pub fn current_event_id() -> u64 {
        // SAFETY: takes no arguments and returns a plain integer.
        unsafe { FSEventsGetCurrentEventId() }
    }

    pub fn changed_since(root: &Path, since: u64) -> Replay {
        if since == 0 {
            return Replay::Unavailable("no recorded stream position");
        }
        if current_event_id() < since {
            // The volume's counter went backwards — the log was reset.
            return Replay::Unavailable("event id went backwards");
        }
        let root_str = root.to_string_lossy().into_owned();

        // SAFETY: every Core Foundation object created below is released on all
        // paths, and the stream is stopped, invalidated and released before
        // `state` — which the callback borrows through the context — is dropped.
        unsafe {
            let cf_path = CFStringCreateWithBytes(
                kCFAllocatorDefault,
                root_str.as_ptr(),
                root_str.len() as CFIndex,
                kCFStringEncodingUTF8,
                0,
            );
            if cf_path.is_null() {
                return Replay::Unavailable("could not encode root path");
            }
            let mut path_ptr = cf_path as *const c_void;
            let paths = CFArrayCreate(
                kCFAllocatorDefault,
                &mut path_ptr as *mut *const c_void,
                1,
                &kCFTypeArrayCallBacks,
            );
            CFRelease(cf_path as *const c_void);
            if paths.is_null() {
                return Replay::Unavailable("could not build path array");
            }

            let mut state = Collect {
                paths: Vec::new(),
                history_done: false,
                dropped: false,
            };
            let context = FSEventStreamContext {
                version: 0,
                info: &mut state as *mut Collect as *mut c_void,
                retain: None,
                release: None,
                copy_description: None,
            };

            let stream = FSEventStreamCreate(
                kCFAllocatorDefault,
                callback,
                &context,
                paths,
                since,
                0.0, // no coalescing latency: take the history as fast as it comes
                CREATE_FLAG_NO_DEFER | CREATE_FLAG_WATCH_ROOT,
            );
            CFRelease(paths as *const c_void);
            if stream.is_null() {
                return Replay::Unavailable("could not create event stream");
            }

            FSEventStreamScheduleWithRunLoop(stream, CFRunLoopGetCurrent(), kCFRunLoopDefaultMode);
            if FSEventStreamStart(stream) == 0 {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                return Replay::Unavailable("could not start event stream");
            }

            // Pump the run loop in short slices until the replay says it is done,
            // or we give up. `state` is only touched by the callback, which only
            // runs while we are inside CFRunLoopRunInMode.
            let mut waited = 0.0f64;
            while !state.history_done && waited < REPLAY_TIMEOUT_SECS {
                CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.25, 1);
                waited += 0.25;
            }

            // "History done" means fseventsd has replayed what it had already
            // processed — not what was still in flight from the kernel when the
            // stream was created. A change made a moment before this call can
            // therefore be missing, which shows up as "0 directories changed"
            // after a real edit. The stream stays live after the marker, so ask
            // fseventsd to push out everything it holds, then keep listening for
            // a short grace period to catch the rest. Cheap next to a walk.
            // The wait ends after SETTLE_SECS of silence rather than a fixed
            // interval, so a burst still landing keeps us listening, bounded
            // by SETTLE_MAX_SECS so a busy volume can't hold a scan hostage.
            if state.history_done {
                FSEventStreamFlushSync(stream);
                let mut quiet = 0.0f64;
                let mut total = 0.0f64;
                while quiet < SETTLE_SECS && total < SETTLE_MAX_SECS {
                    let seen = state.paths.len();
                    CFRunLoopRunInMode(kCFRunLoopDefaultMode, SETTLE_SLICE_SECS, 0);
                    total += SETTLE_SLICE_SECS;
                    quiet = if state.paths.len() > seen {
                        0.0
                    } else {
                        quiet + SETTLE_SLICE_SECS
                    };
                }
            }

            FSEventStreamStop(stream);
            FSEventStreamInvalidate(stream);
            FSEventStreamRelease(stream);

            if state.dropped {
                return Replay::Unavailable("history incomplete (events dropped)");
            }
            if !state.history_done {
                return Replay::Unavailable("history replay timed out");
            }
            let mut paths = state.paths;
            paths.sort();
            paths.dedup();
            Replay::Changed(paths)
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::Replay;
    use std::path::Path;

    pub fn current_event_id() -> u64 {
        0
    }
    pub fn changed_since(_root: &Path, _since: u64) -> Replay {
        Replay::Unavailable("FSEvents is macOS-only")
    }
}

/// The volume's current stream position, to be recorded with a finished scan.
/// Returns 0 where FSEvents is unavailable, which disables replay next time.
pub fn current_event_id() -> u64 {
    imp::current_event_id()
}

/// Directories under `root` that changed since stream position `since`.
pub fn changed_since(root: &Path, since: u64) -> Replay {
    imp::changed_since(root, since)
}

/// The directories a walk must still visit, given the changed paths.
///
/// A directory needs visiting exactly when it is an ancestor-or-self of some
/// changed path; anything else can be served from the cached index. Events name
/// files as well as directories, so a path that is not a directory contributes
/// its parent instead.
///
/// Returns the closure together with how many of `changed` actually landed under
/// `root`. That count matters: FSEvents reports *resolved* paths, so a root
/// reached through a symlink (`/tmp`, `/var`) yields events that match nothing.
/// Silently treating that as "nothing changed" would prune the entire tree and
/// report stale sizes, so callers must fall back when the count is zero but
/// `changed` was not empty.
pub fn dirty_closure(root: &Path, changed: &[PathBuf]) -> (HashSet<PathBuf>, usize) {
    let mut set = HashSet::new();
    set.insert(root.to_path_buf());
    // Events arrive in the filesystem's own namespace; map them back onto the
    // root the caller asked about so prefix comparisons line up.
    let canon = root.canonicalize().ok();
    let mut matched = 0usize;

    for p in changed {
        let mapped = if p.starts_with(root) {
            p.clone()
        } else if let Some(c) = &canon
            && let Ok(rel) = p.strip_prefix(c)
        {
            root.join(rel)
        } else {
            continue; // outside the scanned root
        };
        matched += 1;

        // A path that no longer exists (or is a file) makes its parent dirty:
        // that is the directory whose listing has to be re-read.
        let start = if mapped.is_dir() {
            mapped
        } else {
            match mapped.parent() {
                Some(parent) => parent.to_path_buf(),
                None => continue,
            }
        };
        let mut cur = Some(start.as_path());
        while let Some(c) = cur {
            if !set.insert(c.to_path_buf()) {
                break; // this ancestor chain is already covered
            }
            if c == root {
                break;
            }
            cur = c.parent();
        }
    }
    (set, matched)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closure_covers_ancestors_and_stops_at_root() {
        let root = Path::new("/a/b");
        let (set, matched) = dirty_closure(root, &[PathBuf::from("/a/b/c/d/e")]);
        assert_eq!(matched, 1);
        assert!(set.contains(Path::new("/a/b")));
        assert!(set.contains(Path::new("/a/b/c")));
        // `/a/b/c/d/e` does not exist, so its parent is what got inserted.
        assert!(set.contains(Path::new("/a/b/c/d")));
        assert!(!set.contains(Path::new("/a")));
    }

    #[test]
    fn paths_outside_root_do_not_count_as_matches() {
        let root = Path::new("/a/b");
        let (set, matched) = dirty_closure(root, &[PathBuf::from("/x/y/z")]);
        assert_eq!(
            matched, 0,
            "a non-matching event must not look like a match"
        );
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn closure_of_nothing_is_just_the_root() {
        let (set, matched) = dirty_closure(Path::new("/a/b"), &[]);
        assert_eq!(set.len(), 1);
        assert_eq!(matched, 0);
    }

    #[test]
    fn maps_resolved_paths_back_onto_a_symlinked_root() {
        // macOS resolves /tmp to /private/tmp, and FSEvents reports the resolved
        // form. Without the remap every event would look like it was outside the
        // root, and the whole tree would be wrongly pruned.
        let root = Path::new("/tmp");
        let Ok(canon) = root.canonicalize() else {
            return; // not a symlink on this platform; nothing to assert
        };
        if canon == root {
            return;
        }
        let (set, matched) = dirty_closure(root, &[canon.join("some-dir")]);
        assert_eq!(matched, 1);
        assert!(set.contains(Path::new("/tmp")));
    }
}
