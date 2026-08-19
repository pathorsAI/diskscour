//! Detection of regenerable developer / build caches.
//!
//! Each rule matches a directory by name, optionally gated by a sibling marker
//! (e.g. `node_modules` next to nothing in particular, but `target` only next to
//! `Cargo.toml`) so we don't flag unrelated folders that happen to share a name.
//! Nested matches are collapsed to the top-most hit so reclaimable totals don't
//! double-count.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use eframe::egui::Color32;

use crate::scan::Tree;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Category {
    JsTs,
    Rust,
    Python,
    Jvm,
    Apple,
    Dotnet,
    Cpp,
    Flutter,
    Unity,
    Terraform,
    Misc,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::JsTs => "JavaScript / TypeScript",
            Category::Rust => "Rust",
            Category::Python => "Python",
            Category::Jvm => "JVM (Gradle / Maven)",
            Category::Apple => "Apple / Xcode / Swift",
            Category::Dotnet => ".NET",
            Category::Cpp => "C / C++",
            Category::Flutter => "Flutter / Dart",
            Category::Unity => "Unity",
            Category::Terraform => "Terraform",
            Category::Misc => "Other build caches",
        }
    }

    pub fn color(self) -> Color32 {
        match self {
            Category::JsTs => Color32::from_rgb(0xF7, 0xDF, 0x1E),
            Category::Rust => Color32::from_rgb(0xCE, 0x42, 0x2B),
            Category::Python => Color32::from_rgb(0x4B, 0x8B, 0xBE),
            Category::Jvm => Color32::from_rgb(0x5B, 0x8A, 0x72),
            Category::Apple => Color32::from_rgb(0x9C, 0x9C, 0x9C),
            Category::Dotnet => Color32::from_rgb(0x68, 0x21, 0x7A),
            Category::Cpp => Color32::from_rgb(0x00, 0x59, 0x9C),
            Category::Flutter => Color32::from_rgb(0x02, 0x75, 0xC8),
            Category::Unity => Color32::from_rgb(0x55, 0x55, 0x55),
            Category::Terraform => Color32::from_rgb(0x84, 0x4F, 0xBA),
            Category::Misc => Color32::from_rgb(0x88, 0x88, 0x88),
        }
    }
}

struct Rule {
    /// Directory name to match. A trailing `*` means prefix match.
    name: &'static str,
    category: Category,
    /// Require ANY of these as a sibling (exact file/dir name).
    sibling_any: &'static [&'static str],
    /// ...or a sibling whose name ends with one of these.
    sibling_ext: &'static [&'static str],
    /// Require ANY of these as a direct child of the matched dir.
    child_any: &'static [&'static str],
    note: &'static str,
}

const RULES: &[Rule] = &[
    // ---- JavaScript / TypeScript ----
    Rule {
        name: "node_modules",
        category: Category::JsTs,
        sibling_any: &[],
        sibling_ext: &[],
        child_any: &[],
        note: "npm/yarn/pnpm packages — reinstall with install",
    },
    Rule {
        name: "bower_components",
        category: Category::JsTs,
        sibling_any: &[],
        sibling_ext: &[],
        child_any: &[],
        note: "legacy bower packages",
    },
    Rule {
        name: ".next",
        category: Category::JsTs,
        sibling_any: &["package.json"],
        sibling_ext: &[],
        child_any: &[],
        note: "Next.js build output",
    },
    Rule {
        name: ".nuxt",
        category: Category::JsTs,
        sibling_any: &["package.json"],
        sibling_ext: &[],
        child_any: &[],
        note: "Nuxt build output",
    },
    Rule {
        name: ".svelte-kit",
        category: Category::JsTs,
        sibling_any: &["package.json"],
        sibling_ext: &[],
        child_any: &[],
        note: "SvelteKit build output",
    },
    Rule {
        name: ".angular",
        category: Category::JsTs,
        sibling_any: &["angular.json", "package.json"],
        sibling_ext: &[],
        child_any: &[],
        note: "Angular cache",
    },
    Rule {
        name: ".turbo",
        category: Category::JsTs,
        sibling_any: &["package.json", "turbo.json"],
        sibling_ext: &[],
        child_any: &[],
        note: "Turborepo cache",
    },
    Rule {
        name: ".parcel-cache",
        category: Category::JsTs,
        sibling_any: &["package.json"],
        sibling_ext: &[],
        child_any: &[],
        note: "Parcel cache",
    },
    Rule {
        name: ".vite",
        category: Category::JsTs,
        sibling_any: &["package.json"],
        sibling_ext: &[],
        child_any: &[],
        note: "Vite cache",
    },
    Rule {
        name: ".docusaurus",
        category: Category::JsTs,
        sibling_any: &["package.json"],
        sibling_ext: &[],
        child_any: &[],
        note: "Docusaurus cache",
    },
    Rule {
        name: "dist",
        category: Category::JsTs,
        sibling_any: &["package.json"],
        sibling_ext: &[],
        child_any: &[],
        note: "build output (regenerate with build)",
    },
    Rule {
        name: "coverage",
        category: Category::JsTs,
        sibling_any: &["package.json"],
        sibling_ext: &[],
        child_any: &[],
        note: "test coverage report",
    },
    Rule {
        name: ".nyc_output",
        category: Category::JsTs,
        sibling_any: &["package.json"],
        sibling_ext: &[],
        child_any: &[],
        note: "nyc coverage data",
    },
    // ---- Rust ----
    Rule {
        name: "target",
        category: Category::Rust,
        sibling_any: &["Cargo.toml"],
        sibling_ext: &[],
        child_any: &[],
        note: "cargo build output (cargo build)",
    },
    // ---- Python ----
    Rule {
        name: "__pycache__",
        category: Category::Python,
        sibling_any: &[],
        sibling_ext: &[],
        child_any: &[],
        note: "compiled bytecode",
    },
    Rule {
        name: ".pytest_cache",
        category: Category::Python,
        sibling_any: &[],
        sibling_ext: &[],
        child_any: &[],
        note: "pytest cache",
    },
    Rule {
        name: ".mypy_cache",
        category: Category::Python,
        sibling_any: &[],
        sibling_ext: &[],
        child_any: &[],
        note: "mypy type cache",
    },
    Rule {
        name: ".ruff_cache",
        category: Category::Python,
        sibling_any: &[],
        sibling_ext: &[],
        child_any: &[],
        note: "ruff cache",
    },
    Rule {
        name: ".ipynb_checkpoints",
        category: Category::Python,
        sibling_any: &[],
        sibling_ext: &[],
        child_any: &[],
        note: "Jupyter checkpoints",
    },
    Rule {
        name: ".venv",
        category: Category::Python,
        sibling_any: &[],
        sibling_ext: &[],
        child_any: &[],
        note: "virtualenv (python -m venv)",
    },
    Rule {
        name: "venv",
        category: Category::Python,
        sibling_any: &[],
        sibling_ext: &[],
        child_any: &["pyvenv.cfg"],
        note: "virtualenv (python -m venv)",
    },
    Rule {
        name: "env",
        category: Category::Python,
        sibling_any: &[],
        sibling_ext: &[],
        child_any: &["pyvenv.cfg"],
        note: "virtualenv (python -m venv)",
    },
    Rule {
        name: ".tox",
        category: Category::Python,
        sibling_any: &["tox.ini", "setup.py", "pyproject.toml"],
        sibling_ext: &[],
        child_any: &[],
        note: "tox environments",
    },
    Rule {
        name: ".eggs",
        category: Category::Python,
        sibling_any: &["setup.py", "setup.cfg"],
        sibling_ext: &[],
        child_any: &[],
        note: "setuptools eggs",
    },
    Rule {
        name: "build",
        category: Category::Python,
        sibling_any: &["setup.py", "pyproject.toml"],
        sibling_ext: &[],
        child_any: &[],
        note: "python build output",
    },
    // ---- JVM ----
    Rule {
        name: ".gradle",
        category: Category::Jvm,
        sibling_any: &[
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
        ],
        sibling_ext: &[],
        child_any: &[],
        note: "Gradle cache",
    },
    Rule {
        name: "build",
        category: Category::Jvm,
        sibling_any: &["build.gradle", "build.gradle.kts", "pom.xml"],
        sibling_ext: &[],
        child_any: &[],
        note: "Gradle/Maven build output",
    },
    Rule {
        name: "target",
        category: Category::Jvm,
        sibling_any: &["pom.xml"],
        sibling_ext: &[],
        child_any: &[],
        note: "Maven build output",
    },
    // ---- Apple / Xcode / Swift ----
    Rule {
        name: "DerivedData",
        category: Category::Apple,
        sibling_any: &[],
        sibling_ext: &[],
        child_any: &[],
        note: "Xcode derived data",
    },
    Rule {
        name: "Pods",
        category: Category::Apple,
        sibling_any: &["Podfile"],
        sibling_ext: &[],
        child_any: &[],
        note: "CocoaPods (pod install)",
    },
    Rule {
        name: "Carthage",
        category: Category::Apple,
        sibling_any: &["Cartfile"],
        sibling_ext: &[],
        child_any: &[],
        note: "Carthage build",
    },
    Rule {
        name: ".build",
        category: Category::Apple,
        sibling_any: &["Package.swift"],
        sibling_ext: &[],
        child_any: &[],
        note: "SwiftPM build",
    },
    // ---- .NET ----
    // Require a Debug/Release child so a committed `bin/` of scripts next to a
    // .sln isn't mistaken for build output.
    Rule {
        name: "bin",
        category: Category::Dotnet,
        sibling_any: &[],
        sibling_ext: &[".csproj", ".fsproj", ".vbproj", ".sln"],
        child_any: &["Debug", "Release"],
        note: ".NET build output",
    },
    Rule {
        name: "obj",
        category: Category::Dotnet,
        sibling_any: &[],
        sibling_ext: &[".csproj", ".fsproj", ".vbproj", ".sln"],
        child_any: &["Debug", "Release"],
        note: ".NET intermediate output",
    },
    // ---- C / C++ ----
    Rule {
        name: "build",
        category: Category::Cpp,
        sibling_any: &["CMakeLists.txt"],
        sibling_ext: &[],
        child_any: &[],
        note: "CMake build dir",
    },
    Rule {
        name: "cmake-build-*",
        category: Category::Cpp,
        sibling_any: &["CMakeLists.txt"],
        sibling_ext: &[],
        child_any: &[],
        note: "CLion/CMake build dir",
    },
    // ---- Flutter / Dart ----
    Rule {
        name: ".dart_tool",
        category: Category::Flutter,
        sibling_any: &["pubspec.yaml"],
        sibling_ext: &[],
        child_any: &[],
        note: "Dart tool cache",
    },
    Rule {
        name: "build",
        category: Category::Flutter,
        sibling_any: &["pubspec.yaml"],
        sibling_ext: &[],
        child_any: &[],
        note: "Flutter build output",
    },
    // ---- Unity ----
    // Require BOTH a ProjectSettings sibling AND a Unity-specific child so we can
    // never flag a generic "Library" folder (e.g. ~/Library) for deletion.
    Rule {
        name: "Library",
        category: Category::Unity,
        sibling_any: &["ProjectSettings"],
        sibling_ext: &[],
        child_any: &[
            "PackageCache",
            "ScriptAssemblies",
            "ArtifactDB",
            "StateCache",
        ],
        note: "Unity library cache",
    },
    // ---- Terraform ----
    Rule {
        name: ".terraform",
        category: Category::Terraform,
        sibling_any: &[],
        sibling_ext: &[],
        child_any: &[],
        note: "Terraform plugins/modules",
    },
    // ---- Misc ----
    Rule {
        name: ".cache",
        category: Category::Misc,
        sibling_any: &["package.json"],
        sibling_ext: &[],
        child_any: &[],
        note: "build cache",
    },
    Rule {
        name: "dist",
        category: Category::Python,
        sibling_any: &["setup.py", "pyproject.toml"],
        sibling_ext: &[],
        child_any: &[],
        note: "python sdist/wheel output",
    },
];

/// A detected cache directory.
pub struct CacheHit {
    pub node_idx: usize,
    pub path: PathBuf,
    pub size: u64,
    pub category: Category,
    pub note: &'static str,
}

/// A cache detected by scanning the persistent index rather than a live tree.
pub struct IndexHit {
    pub path: PathBuf,
    pub size: u64,
    pub files: u64,
    pub category: Category,
    pub note: &'static str,
}

/// Find caches from a cached index, confirming each candidate against the
/// filesystem.
///
/// The index collapses small files, so the marker files rules depend on
/// (`Cargo.toml`, `package.json`, …) are not in it. Rather than bloat the index
/// with them, candidates are matched by directory name — which the index does
/// have — and then confirmed with [`classify_path`]. That also drops anything
/// that has been deleted since the scan, so the list is never stale.
pub fn detect_in_index(idx: &crate::index::Index) -> Vec<IndexHit> {
    let mut matched: Vec<(u32, Category, &'static str)> = Vec::new();
    let mut matchset: HashSet<u32> = HashSet::new();

    for (i, d) in idx.dirs.iter().enumerate() {
        let i = i as u32;
        // Never flag the scan root itself — that would offer to delete the
        // folder being analyzed.
        if i == idx.root_ix || d.subtree_bytes == 0 {
            continue;
        }
        if !RULES.iter().any(|r| name_matches(r, &d.name)) {
            continue;
        }
        if let Some((category, note)) = classify_path(&idx.path_of(i)) {
            matched.push((i, category, note));
            matchset.insert(i);
        }
    }

    let mut hits = Vec::new();
    for (i, category, note) in matched {
        // Skip anything nested inside another hit, so totals don't double-count.
        let mut anc = idx.dirs[i as usize].parent;
        let mut nested = false;
        while let Some(p) = anc {
            if matchset.contains(&p) {
                nested = true;
                break;
            }
            anc = idx.dirs[p as usize].parent;
        }
        if nested {
            continue;
        }
        hits.push(IndexHit {
            path: idx.path_of(i),
            size: idx.dirs[i as usize].subtree_bytes,
            files: idx.dirs[i as usize].subtree_files,
            category,
            note,
        });
    }
    hits.sort_by(|a, b| b.size.cmp(&a.size));
    hits
}

/// Re-check a path against the cache rules using the filesystem directly, with
/// no reliance on a previously scanned tree.
///
/// This is the guard used immediately before deleting anything: an index can be
/// minutes or days old, and in that time a `target/` may have stopped being a
/// Rust build directory. Matching against what is on disk *right now* means a
/// stale index can never cause the wrong thing to be trashed.
pub fn classify_path(path: &Path) -> Option<(Category, &'static str)> {
    let md = std::fs::symlink_metadata(path).ok()?;
    if !md.is_dir() {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    let parent = path.parent();
    for rule in RULES {
        if !name_matches(rule, name) {
            continue;
        }
        let sib_ok = if rule.sibling_any.is_empty() && rule.sibling_ext.is_empty() {
            true
        } else if let Some(p) = parent {
            rule.sibling_any.iter().any(|s| p.join(s).exists())
                || (!rule.sibling_ext.is_empty()
                    && std::fs::read_dir(p).is_ok_and(|rd| {
                        rd.flatten().any(|e| {
                            let n = e.file_name();
                            let n = n.to_string_lossy();
                            rule.sibling_ext.iter().any(|ext| n.ends_with(ext))
                        })
                    }))
        } else {
            false
        };
        if !sib_ok {
            continue;
        }
        let child_ok =
            rule.child_any.is_empty() || rule.child_any.iter().any(|c| path.join(c).exists());
        if child_ok {
            return Some((rule.category, rule.note));
        }
    }
    None
}

fn name_matches(rule: &Rule, name: &str) -> bool {
    if let Some(prefix) = rule.name.strip_suffix('*') {
        name.starts_with(prefix)
    } else {
        name == rule.name
    }
}

fn context_ok(rule: &Rule, tree: &Tree, idx: usize) -> bool {
    let node = &tree.nodes[idx];
    let sib_ok = if rule.sibling_any.is_empty() && rule.sibling_ext.is_empty() {
        true
    } else if let Some(p) = node.parent {
        tree.nodes[p].children.iter().any(|&c| {
            let cn = &tree.nodes[c].name;
            rule.sibling_any.iter().any(|m| cn == m)
                || rule.sibling_ext.iter().any(|e| cn.ends_with(e))
        })
    } else {
        false
    };
    if !sib_ok {
        return false;
    }
    if rule.child_any.is_empty() {
        true
    } else {
        node.children
            .iter()
            .any(|&c| rule.child_any.iter().any(|m| &tree.nodes[c].name == m))
    }
}

/// Scan the tree for cache directories, returning top-most hits sorted by size.
pub fn detect(tree: &Tree) -> Vec<CacheHit> {
    let mut matched: Vec<(usize, &'static Rule)> = Vec::new();
    let mut matchset: HashSet<usize> = HashSet::new();

    for i in 0..tree.nodes.len() {
        // Never flag the scan root itself — that would offer to delete the folder
        // the user is analyzing.
        if i == tree.root {
            continue;
        }
        let n = &tree.nodes[i];
        if n.removed || !n.is_dir {
            continue;
        }
        for rule in RULES {
            if name_matches(rule, &n.name) && context_ok(rule, tree, i) {
                matched.push((i, rule));
                matchset.insert(i);
                break;
            }
        }
    }

    let mut hits = Vec::new();
    for (i, rule) in matched {
        // Skip if an ancestor is also a cache hit (avoid double-counting nested caches).
        let mut anc = tree.nodes[i].parent;
        let mut nested = false;
        while let Some(p) = anc {
            if matchset.contains(&p) {
                nested = true;
                break;
            }
            anc = tree.nodes[p].parent;
        }
        if nested {
            continue;
        }
        if tree.nodes[i].size == 0 {
            continue;
        }
        hits.push(CacheHit {
            node_idx: i,
            path: tree.path(i),
            size: tree.nodes[i].size,
            category: rule.category,
            note: rule.note,
        });
    }

    hits.sort_by(|a, b| b.size.cmp(&a.size));
    hits
}
