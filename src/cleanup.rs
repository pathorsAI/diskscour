//! The one path by which anything gets deleted, and the guards on it.
//!
//! The GUI, the CLI and the MCP server all go through [`plan`] and [`execute`].
//! Centralising it matters because the MCP server hands this capability to an
//! agent: the rules below have to hold no matter what a caller asks for, so they
//! live in the code rather than in a tool description.
//!
//! 1. A target must sit strictly inside the root that was scanned.
//! 2. It must currently match a cache rule, checked against the filesystem —
//!    unless the caller explicitly opted out with `allow_any`.
//! 3. Obviously-wrong targets (a home directory, a volume root, anything with a
//!    `..` in it, a symlink pointing out of the root) are refused outright.
//! 4. Everything is re-verified at the moment of deletion, so a stale index
//!    cannot cause the wrong thing to go.
//! 5. Deletion always means the macOS Trash. There is no hard-delete path here.

use std::path::{Path, PathBuf};

use crate::caches::{self, Category};

/// A single vetted deletion target.
pub struct Item {
    pub path: PathBuf,
    pub bytes: u64,
    pub category: Option<Category>,
    pub note: Option<&'static str>,
}

/// A target that was refused, and why.
pub struct Rejected {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Default)]
pub struct Plan {
    pub items: Vec<Item>,
    pub rejected: Vec<Rejected>,
}

impl Plan {
    pub fn total_bytes(&self) -> u64 {
        self.items.iter().map(|i| i.bytes).sum()
    }
}

/// Outcome of actually trashing one item.
pub struct Outcome {
    pub path: PathBuf,
    pub bytes: u64,
    pub trashed: bool,
    pub error: Option<String>,
}

/// Directory size on disk, counted the same way the scanner counts it.
pub fn measure(path: &Path) -> u64 {
    let mut total = 0u64;
    let walk = jwalk::WalkDir::new(path)
        .skip_hidden(false)
        .follow_links(false);
    for entry in walk.into_iter().flatten() {
        if entry.file_type.is_file()
            && let Ok(md) = std::fs::symlink_metadata(entry.path())
        {
            total += allocated(&md);
        }
    }
    total
}

#[cfg(unix)]
fn allocated(md: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn allocated(md: &std::fs::Metadata) -> u64 {
    md.len()
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Reject targets that no rule should ever be able to reach, regardless of what
/// the caller claims. Returns `Some(reason)` when the path must not be touched.
fn structural_objection(path: &Path, root: &Path) -> Option<String> {
    if !path.is_absolute() {
        return Some("path is not absolute".into());
    }
    if path.components().any(|c| c.as_os_str() == "..") {
        return Some("path contains '..'".into());
    }
    if path == root {
        return Some("refusing to delete the scan root itself".into());
    }
    if !path.starts_with(root) {
        return Some(format!(
            "path is outside the scanned root {}",
            root.display()
        ));
    }
    if let Some(h) = home()
        && path == h
    {
        return Some("refusing to delete the home directory".into());
    }
    // Guards against `/`, `/Users`, `/Volumes/Foo` and similar.
    let depth = path.components().count();
    if depth < 4 {
        return Some(format!(
            "path is too shallow to be a build cache ({depth} components)"
        ));
    }
    // A symlink that resolves out of the root would move the deletion elsewhere.
    if let Ok(real) = path.canonicalize()
        && let Ok(real_root) = root.canonicalize()
        && !real.starts_with(&real_root)
    {
        return Some("path resolves outside the scanned root".into());
    }
    None
}

/// Vet a batch of deletion targets against `root`. Nothing is deleted here.
///
/// `allow_any` lifts only the "must be a recognised cache" requirement — every
/// other guard still applies. Callers exposing this to an agent should treat it
/// as something a person has to ask for by name.
pub fn plan(root: &Path, paths: &[PathBuf], allow_any: bool) -> Plan {
    let mut out = Plan::default();
    for p in paths {
        if let Some(reason) = structural_objection(p, root) {
            out.rejected.push(Rejected {
                path: p.clone(),
                reason,
            });
            continue;
        }
        if !p.exists() {
            out.rejected.push(Rejected {
                path: p.clone(),
                reason: "path no longer exists".into(),
            });
            continue;
        }
        match caches::classify_path(p) {
            Some((category, note)) => out.items.push(Item {
                path: p.clone(),
                bytes: measure(p),
                category: Some(category),
                note: Some(note),
            }),
            None if allow_any => out.items.push(Item {
                path: p.clone(),
                bytes: measure(p),
                category: None,
                note: None,
            }),
            None => out.rejected.push(Rejected {
                path: p.clone(),
                reason: "not a recognised regenerable cache (pass allow_any to override)".into(),
            }),
        }
    }
    out
}

/// Move vetted items to the Trash, re-checking each one first.
///
/// The re-check is the point: `plan` may have run seconds or minutes ago, and
/// this is the last moment at which the filesystem can still be consulted.
pub fn execute(root: &Path, items: &[Item], allow_any: bool) -> Vec<Outcome> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        if let Some(reason) = structural_objection(&item.path, root) {
            out.push(Outcome {
                path: item.path.clone(),
                bytes: 0,
                trashed: false,
                error: Some(reason),
            });
            continue;
        }
        if !item.path.exists() {
            out.push(Outcome {
                path: item.path.clone(),
                bytes: 0,
                trashed: false,
                error: Some("path disappeared before deletion".into()),
            });
            continue;
        }
        if !allow_any && caches::classify_path(&item.path).is_none() {
            out.push(Outcome {
                path: item.path.clone(),
                bytes: 0,
                trashed: false,
                error: Some("no longer matches a cache rule — refusing".into()),
            });
            continue;
        }
        match trash::delete(&item.path) {
            Ok(()) => out.push(Outcome {
                path: item.path.clone(),
                bytes: item.bytes,
                trashed: true,
                error: None,
            }),
            Err(e) => out.push(Outcome {
                path: item.path.clone(),
                bytes: 0,
                trashed: false,
                error: Some(e.to_string()),
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(tag: &str) -> PathBuf {
        let base = std::env::temp_dir()
            .join(format!("ds_cleanup_{}_{}", std::process::id(), tag))
            .join("workspace");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("proj/node_modules/pkg")).unwrap();
        fs::write(base.join("proj/package.json"), b"{}").unwrap();
        fs::write(base.join("proj/node_modules/pkg/index.js"), vec![0u8; 5000]).unwrap();
        fs::create_dir_all(base.join("proj/src")).unwrap();
        fs::write(base.join("proj/src/app.js"), vec![0u8; 100]).unwrap();
        base
    }

    #[test]
    fn accepts_a_real_cache_and_measures_it() {
        let base = tmp("accept");
        let target = base.join("proj/node_modules");
        let p = plan(&base, std::slice::from_ref(&target), false);
        assert_eq!(p.items.len(), 1, "{:?}", p.rejected[0].reason);
        assert_eq!(p.items[0].category, Some(Category::JsTs));
        assert!(p.items[0].bytes >= 5000);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn refuses_a_plain_source_directory() {
        let base = tmp("refuse");
        let p = plan(&base, &[base.join("proj/src")], false);
        assert!(p.items.is_empty());
        assert_eq!(p.rejected.len(), 1);
        assert!(p.rejected[0].reason.contains("not a recognised"));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn allow_any_still_enforces_the_structural_guards() {
        let base = tmp("guards");
        // Outside the root, even with allow_any.
        let p = plan(&base, &[PathBuf::from("/etc")], true);
        assert!(p.items.is_empty());
        assert!(p.rejected[0].reason.contains("outside the scanned root"));

        // The root itself, even with allow_any.
        let p = plan(&base, std::slice::from_ref(&base), true);
        assert!(p.items.is_empty());
        assert!(p.rejected[0].reason.contains("scan root itself"));

        // Traversal, even with allow_any.
        let p = plan(&base, &[base.join("proj/../../elsewhere")], true);
        assert!(p.items.is_empty());
        assert!(p.rejected[0].reason.contains(".."));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn allow_any_opens_only_the_cache_rule_check() {
        let base = tmp("anyok");
        let p = plan(&base, &[base.join("proj/src")], true);
        assert_eq!(p.items.len(), 1);
        assert_eq!(p.items[0].category, None);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn refuses_shallow_and_home_paths() {
        let base = tmp("shallow");
        assert!(structural_objection(Path::new("/"), Path::new("/")).is_some());
        assert!(structural_objection(Path::new("/Users"), Path::new("/")).is_some());
        if let Some(h) = home() {
            assert!(structural_objection(&h, Path::new("/")).is_some());
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn execute_refuses_a_target_that_stopped_matching() {
        let base = tmp("revalidate");
        let target = base.join("proj/node_modules");
        let p = plan(&base, std::slice::from_ref(&target), false);
        assert_eq!(p.items.len(), 1);

        // Between planning and executing, the context that made this a cache
        // disappears. The re-check must catch it rather than trust the plan.
        fs::remove_file(base.join("proj/package.json")).unwrap();
        // node_modules matches on name alone, so remove the directory instead to
        // simulate the target itself going away.
        fs::rename(&target, base.join("proj/renamed")).unwrap();

        let outcomes = execute(&base, &p.items, false);
        assert_eq!(outcomes.len(), 1);
        assert!(!outcomes[0].trashed);
        assert!(outcomes[0].error.is_some());
        let _ = fs::remove_dir_all(&base);
    }
}
