//! DiskScour — a fast macOS disk-usage analyzer with dev-cache cleanup.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod caches;
mod cleanup;
mod cli;
mod engine;
mod fsevents;
mod index;
mod mcp;
mod scan;
mod treemap;
mod util;

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Instant;

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Layout, Rect, RichText, Sense, Stroke,
    StrokeKind,
};

use caches::CacheHit;
use scan::{ScanProgress, Tree};

fn main() -> eframe::Result {
    // Headless modes. `diskscour` with no arguments opens the window.
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("mcp") => {
            if let Err(e) = mcp::serve() {
                eprintln!("diskscour: mcp server: {e}");
                std::process::exit(1);
            }
            return Ok(());
        }
        Some("-h" | "--help" | "help") => {
            println!("{}", cli::USAGE);
            return Ok(());
        }
        Some("-V" | "--version") => {
            println!("diskscour {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some(cmd @ ("scan" | "caches" | "status" | "trash")) => {
            std::process::exit(cli::run(cmd, &args[1..]));
        }
        Some(other) if other.starts_with('-') => {
            eprintln!("diskscour: unknown flag: {other}\n\n{}", cli::USAGE);
            std::process::exit(2);
        }
        Some(other) => {
            eprintln!("diskscour: unknown command: {other}\n\n{}", cli::USAGE);
            std::process::exit(2);
        }
        None => {}
    }

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("DiskScour"),
        ..Default::default()
    };
    eframe::run_native(
        "DiskScour",
        native_options,
        Box::new(|_cc| {
            let mut app = DiskScourApp::default();
            if let Some(home) = home_dir() {
                app.root_input = home.display().to_string();
                // Show the last scan of this folder immediately, if we have one,
                // so the window is useful before any walking starts.
                app.load_cached(&home);
            }
            Ok(Box::new(app) as Box<dyn eframe::App>)
        }),
    )
}

/// Compact relative age for the status line.
fn human_age(secs: u64) -> String {
    if secs < 90 {
        format!("{secs}s")
    } else if secs < 5400 {
        format!("{}m", secs / 60)
    } else if secs < 172_800 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[derive(PartialEq, Eq, Clone, Copy, Default)]
enum View {
    #[default]
    Tree,
    Treemap,
    Caches,
}

/// A queued trash operation awaiting user confirmation.
struct Pending {
    items: Vec<(usize, PathBuf, u64)>, // (node idx, path, size)
}

/// Streamed output from the scan thread: periodic partial snapshots while the
/// walk is in progress, then a final complete tree.
enum ScanMsg {
    Partial(Tree),
    /// Boxed: the finished refresh carries the tree plus the index and the
    /// explanation of which strategy was used, which is much larger than a
    /// partial snapshot.
    Done(Box<engine::Refreshed>),
}

/// Deferred UI events, applied after the frame is rendered to keep borrows simple.
enum Action {
    Toggle(usize),
    Select(usize),
    MapRoot(usize),
    Reveal(PathBuf),
    OpenPath(PathBuf),
    RequestTrash(Vec<(usize, PathBuf, u64)>),
    ToggleCache(usize),
    CacheSelectAll,
    CacheSelectNone,
    RequestTrashCaches,
    ConfirmTrash,
    CancelTrash,
    StartScan(PathBuf),
}

#[derive(Default)]
struct DiskScourApp {
    root_input: String,
    tree: Option<Tree>,
    scanning: bool,
    rx: Option<Receiver<ScanMsg>>,
    progress: Arc<ScanProgress>,
    scan_started: Option<Instant>,
    scan_secs: f32,
    disk: Option<(u64, u64)>,

    view: View,
    expanded: HashSet<usize>,
    selected: Option<usize>,
    map_root: Option<usize>,

    cache_hits: Vec<CacheHit>,
    cache_selected: HashSet<usize>,

    pending: Option<Pending>,
    trashing: bool,
    trash_rx: Option<Receiver<Vec<(usize, bool, u64)>>>, // (node idx, ok, size)
    /// Nodes trashed during this scan, by (stable) index. Re-applied to each new
    /// snapshot so trashed items don't reappear when the scan refreshes the tree.
    trashed: HashSet<usize>,
    status: String,
    /// How the currently displayed tree was produced, shown in the status line so
    /// the numbers are never presented without their provenance.
    scan_mode: Option<index::Mode>,
    scan_note: String,
}

impl eframe::App for DiskScourApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_scan();
        self.poll_trash();
        if self.scanning || self.trashing {
            ctx.request_repaint();
        }

        // Move the tree out so panel closures never alias `self.tree`.
        let tree = self.tree.take();
        let has_tree = tree.is_some();
        let actions: RefCell<Vec<Action>> = RefCell::new(Vec::new());

        egui::TopBottomPanel::top("top").show(ctx, |ui| self.ui_top(ui, has_tree, &actions));
        egui::TopBottomPanel::bottom("bottom")
            .show(ctx, |ui| self.ui_bottom(ui, tree.as_ref(), &actions));
        egui::CentralPanel::default().show(ctx, |ui| self.ui_central(ui, tree.as_ref(), &actions));
        if self.pending.is_some() {
            self.ui_confirm(ctx, &actions);
        }

        self.tree = tree;
        for a in actions.into_inner() {
            self.apply(a);
        }
    }
}

impl DiskScourApp {
    // ---- scanning lifecycle -------------------------------------------------

    fn poll_scan(&mut self) {
        let Some(rx) = self.rx.take() else {
            return;
        };
        // Drain everything queued this frame and keep only the newest tree; older
        // snapshots are superseded so there's no point installing each one.
        let mut latest: Option<Tree> = None;
        let mut done = false;
        let mut alive = true;
        loop {
            match rx.try_recv() {
                Ok(ScanMsg::Partial(t)) => latest = Some(t),
                Ok(ScanMsg::Done(r)) => {
                    self.scan_mode = Some(r.mode);
                    self.scan_note = r.reason.clone();
                    latest = Some(r.tree);
                    done = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    alive = false;
                    break;
                }
            }
        }
        if let Some(tree) = latest {
            self.install_scan_tree(tree, done);
        }
        if done {
            // scanning flag already cleared by install_scan_tree.
        } else if !alive {
            // Thread ended without a Done message — treat as failure unless we
            // already have something to show.
            self.scanning = false;
            if self.tree.is_none() {
                self.status = "Scan failed.".into();
            }
        } else {
            self.rx = Some(rx); // still scanning; keep listening next frame
        }
    }

    /// Install a scanned tree (partial or final). Preserves the user's browsing
    /// state across partial refreshes and re-applies any trashing done so far.
    fn install_scan_tree(&mut self, mut tree: Tree, done: bool) {
        let first = self.selected.is_none();
        // Re-hide anything trashed during this scan (indices are stable).
        for &idx in &self.trashed {
            if idx < tree.nodes.len() {
                tree.remove(idx);
            }
        }
        self.cache_hits = caches::detect(&tree);
        if first {
            self.selected = Some(tree.root);
            self.map_root = Some(tree.root);
            self.expanded.insert(tree.root);
        }
        let total = tree.nodes[tree.root].size;
        let files = tree.nodes[tree.root].file_count;
        let recl: u64 = self.cache_hits.iter().map(|h| h.size).sum();
        self.tree = Some(tree);
        self.fix_refs();
        if done {
            self.scanning = false;
            self.scan_secs = self
                .scan_started
                .map(|s| s.elapsed().as_secs_f32())
                .unwrap_or(0.0);
            self.status = format!(
                "{} across {} files in {:.1}s · {} reclaimable in dev caches{}",
                util::human(total),
                files,
                self.scan_secs,
                util::human(recl),
                match self.scan_mode {
                    Some(index::Mode::Full) | None => String::new(),
                    Some(m) => format!(" · {} refresh ({})", m.as_str(), self.scan_note),
                }
            );
        } else {
            self.status = format!(
                "Scanning… {} across {} files · {} reclaimable so far",
                util::human(total),
                files,
                util::human(recl)
            );
        }
    }

    /// Show the previous scan of `root` straight away, if one is cached.
    /// Purely a head start — a refresh still runs and replaces this.
    fn load_cached(&mut self, root: &Path) {
        let Some(idx) = engine::cached(root) else {
            return;
        };
        let tree = index::to_tree(&idx);
        let total = tree.nodes[tree.root].size;
        self.disk = util::disk_usage(root);
        self.scan_mode = Some(idx.mode);
        self.scan_note = format!("cached, {} old", human_age(idx.age_secs()));
        self.install_scan_tree(tree, false);
        self.status = format!(
            "{} from the last scan ({} ago) — refreshing…",
            util::human(total),
            human_age(idx.age_secs())
        );
    }

    fn start_scan(&mut self, root: PathBuf) {
        if self.scanning {
            return;
        }
        self.progress = Arc::new(ScanProgress::default());
        self.cache_hits.clear();
        self.cache_selected.clear();
        self.trashed.clear();
        self.selected = None;
        self.map_root = None;
        self.expanded.clear();
        self.pending = None;
        self.disk = util::disk_usage(&root);

        let prog = self.progress.clone();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let snap_tx = tx.clone();
            let result = engine::refresh(root, engine::Freshness::Auto, prog, move |snap| {
                let _ = snap_tx.send(ScanMsg::Partial(snap));
            });
            let _ = tx.send(ScanMsg::Done(Box::new(result)));
        });
        self.rx = Some(rx);
        self.scanning = true;
        self.scan_started = Some(Instant::now());
        self.status = "Scanning…".into();
    }

    // ---- deferred actions ---------------------------------------------------

    fn apply(&mut self, a: Action) {
        match a {
            Action::Toggle(i) => {
                if !self.expanded.remove(&i) {
                    self.expanded.insert(i);
                }
            }
            Action::Select(i) => {
                self.selected = Some(i);
                if let Some(t) = &self.tree
                    && t.nodes[i].is_dir
                {
                    self.map_root = Some(i);
                }
            }
            Action::MapRoot(i) => {
                self.map_root = Some(i);
                self.selected = Some(i);
            }
            Action::Reveal(p) => util::reveal_in_finder(&p),
            Action::OpenPath(p) => util::open_path(&p),
            Action::RequestTrash(items) => self.pending = Some(Pending { items }),
            Action::ToggleCache(i) => {
                if !self.cache_selected.remove(&i) {
                    self.cache_selected.insert(i);
                }
            }
            Action::CacheSelectAll => {
                self.cache_selected = self.cache_hits.iter().map(|h| h.node_idx).collect();
            }
            Action::CacheSelectNone => self.cache_selected.clear(),
            Action::RequestTrashCaches => {
                let mut items: Vec<(usize, PathBuf, u64)> = self
                    .cache_hits
                    .iter()
                    .filter(|h| self.cache_selected.contains(&h.node_idx))
                    .map(|h| (h.node_idx, h.path.clone(), h.size))
                    .collect();
                items.sort_by(|a, b| b.2.cmp(&a.2));
                if !items.is_empty() {
                    self.pending = Some(Pending { items });
                }
            }
            Action::ConfirmTrash => self.start_trash(),
            Action::CancelTrash => self.pending = None,
            Action::StartScan(p) => self.start_scan(p),
        }
    }

    /// Move the pending items to Trash on a background thread (trashing a huge
    /// directory can take a moment; we don't want to freeze the UI).
    fn start_trash(&mut self) {
        if self.trashing {
            return;
        }
        let Some(p) = self.pending.take() else {
            return;
        };
        // Deleting goes through the same guarded path the CLI and MCP server use,
        // so the structural checks (never the scan root, never outside it, never
        // through a symlink that escapes it) hold here too. `allow_any` is on
        // because a GUI selection is an explicit, confirmed choice of a specific
        // path rather than a rule match.
        let Some(root) = self.tree.as_ref().map(|t| t.root_path.clone()) else {
            return;
        };
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let items: Vec<cleanup::Item> = p
                .items
                .iter()
                .map(|(_, path, size)| cleanup::Item {
                    path: path.clone(),
                    bytes: *size,
                    category: None,
                    note: None,
                })
                .collect();
            let outcomes = cleanup::execute(&root, &items, true);
            let out: Vec<(usize, bool, u64)> = p
                .items
                .iter()
                .zip(outcomes)
                .map(|((idx, _, size), o)| (*idx, o.trashed, *size))
                .collect();
            let _ = tx.send(out);
        });
        self.trash_rx = Some(rx);
        self.trashing = true;
        self.status = "Moving to Trash…".into();
    }

    fn poll_trash(&mut self) {
        if let Some(rx) = self.trash_rx.take() {
            match rx.try_recv() {
                Ok(out) => {
                    let mut ok = 0usize;
                    let mut failed = 0usize;
                    let mut freed = 0u64;
                    for (idx, success, size) in out {
                        if success {
                            ok += 1;
                            freed += size;
                            self.trashed.insert(idx);
                            if let Some(t) = &mut self.tree {
                                t.remove(idx);
                            }
                        } else {
                            failed += 1;
                        }
                    }
                    if let Some(t) = &self.tree {
                        self.cache_hits = caches::detect(t);
                    }
                    self.cache_selected.clear();
                    self.fix_refs();
                    self.trashing = false;
                    self.status = format!(
                        "Moved {ok} item(s) to Trash, freed {}{}",
                        util::human(freed),
                        if failed > 0 {
                            format!(" — {failed} failed")
                        } else {
                            String::new()
                        }
                    );
                }
                Err(TryRecvError::Empty) => self.trash_rx = Some(rx),
                Err(TryRecvError::Disconnected) => {
                    self.trashing = false;
                    self.status = "Trash failed.".into();
                }
            }
        }
    }

    fn fix_refs(&mut self) {
        if let Some(t) = &self.tree {
            let dead = |o: Option<usize>| o.is_none_or(|i| t.nodes[i].removed);
            if dead(self.selected) {
                self.selected = Some(t.root);
            }
            if dead(self.map_root) {
                self.map_root = Some(t.root);
            }
        }
    }

    // ---- panels -------------------------------------------------------------

    fn ui_top(&mut self, ui: &mut egui::Ui, has_tree: bool, actions: &RefCell<Vec<Action>>) {
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.heading("DiskScour");
            ui.separator();
            ui.label("Folder:");
            ui.add(
                egui::TextEdit::singleline(&mut self.root_input)
                    .desired_width(360.0)
                    .hint_text("/path/to/scan"),
            );
            if ui.button("Choose…").clicked()
                && let Some(p) = util::pick_folder()
            {
                self.root_input = p.display().to_string();
            }
            let can_scan = !self.scanning && !self.root_input.trim().is_empty();
            if ui
                .add_enabled(can_scan, egui::Button::new("Scan"))
                .clicked()
            {
                actions
                    .borrow_mut()
                    .push(Action::StartScan(PathBuf::from(self.root_input.trim())));
            }
            if ui.button("Home").clicked()
                && let Some(h) = home_dir()
            {
                self.root_input = h.display().to_string();
                actions.borrow_mut().push(Action::StartScan(h));
            }
            if self.scanning {
                ui.spinner();
                let f = self.progress.files.load(Ordering::Relaxed);
                let b = self.progress.bytes.load(Ordering::Relaxed);
                ui.label(format!("scanning… {f} files · {}", util::human(b)));
            }
        });
        if has_tree {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.view, View::Tree, "Tree");
                ui.selectable_value(&mut self.view, View::Treemap, "Treemap");
                ui.selectable_value(
                    &mut self.view,
                    View::Caches,
                    format!("Dev caches ({})", self.cache_hits.len()),
                );
            });
        }
        ui.add_space(2.0);
    }

    fn ui_bottom(
        &mut self,
        ui: &mut egui::Ui,
        tree: Option<&Tree>,
        actions: &RefCell<Vec<Action>>,
    ) {
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(&self.status);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some((total, avail)) = self.disk {
                    ui.label(format!(
                        "Disk: {} free of {}",
                        util::human(avail),
                        util::human(total)
                    ));
                }
            });
        });

        if let (Some(tree), Some(sel)) = (tree, self.selected)
            && !tree.nodes[sel].removed
        {
            let node = &tree.nodes[sel];
            let path = tree.path(sel);
            let size = node.size;
            // A synthetic row stands in for a directory's collapsed small files;
            // it has no path of its own, so none of these actions apply to it.
            let is_root = sel == tree.root || node.synthetic;
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new(util::human(size)).strong());
                ui.label(path.display().to_string());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if !is_root && ui.button("Trash…").clicked() {
                        actions.borrow_mut().push(Action::RequestTrash(vec![(
                            sel,
                            path.clone(),
                            size,
                        )]));
                    }
                    if !node.synthetic {
                        if ui.button("Open").clicked() {
                            actions.borrow_mut().push(Action::OpenPath(path.clone()));
                        }
                        if ui.button("Reveal").clicked() {
                            actions.borrow_mut().push(Action::Reveal(path.clone()));
                        }
                    }
                });
            });
        }
        ui.add_space(2.0);
    }

    fn ui_central(
        &mut self,
        ui: &mut egui::Ui,
        tree: Option<&Tree>,
        actions: &RefCell<Vec<Action>>,
    ) {
        let Some(tree) = tree else {
            ui.centered_and_justified(|ui| {
                ui.label(if self.scanning {
                    "Scanning…"
                } else {
                    "Choose a folder and press Scan to begin."
                });
            });
            return;
        };
        match self.view {
            View::Tree => self.ui_tree(ui, tree, actions),
            View::Treemap => self.ui_treemap(ui, tree, actions),
            View::Caches => self.ui_caches(ui, tree, actions),
        }
    }

    // ---- tree view ----------------------------------------------------------

    fn ui_tree(&mut self, ui: &mut egui::Ui, tree: &Tree, actions: &RefCell<Vec<Action>>) {
        let root = tree.root;
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                self.tree_row(ui, tree, root, 0, actions);
            });
    }

    fn tree_row(
        &mut self,
        ui: &mut egui::Ui,
        tree: &Tree,
        idx: usize,
        depth: usize,
        actions: &RefCell<Vec<Action>>,
    ) {
        let node = &tree.nodes[idx];
        if node.removed {
            return;
        }
        let is_dir = node.is_dir;
        let synthetic = node.synthetic;
        let has_children = !node.children.is_empty();
        let name = node.name.clone();
        let path = tree.path(idx);
        let size = node.size;
        let is_root = idx == tree.root;
        let parent_size = node
            .parent
            .map(|p| tree.nodes[p].size)
            .unwrap_or(size)
            .max(1);
        let expanded = self.expanded.contains(&idx);
        let selected = self.selected == Some(idx);
        let frac = (size as f32 / parent_size as f32).clamp(0.0, 1.0);

        ui.push_id(idx, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(depth as f32 * 14.0);
                if is_dir && has_children {
                    let sym = if expanded { "v" } else { ">" };
                    if ui
                        .add(
                            egui::Button::new(sym)
                                .frame(false)
                                .min_size(egui::vec2(16.0, 16.0)),
                        )
                        .clicked()
                    {
                        actions.borrow_mut().push(Action::Toggle(idx));
                    }
                } else {
                    ui.add_space(16.0);
                }
                let label = format!("{} {}", if is_dir { "[D]" } else { "   " }, name);
                let r = ui.selectable_label(selected, label);
                if r.clicked() {
                    actions.borrow_mut().push(Action::Select(idx));
                }
                if r.double_clicked() && is_dir {
                    actions.borrow_mut().push(Action::Toggle(idx));
                }
                r.context_menu(|ui| {
                    if synthetic {
                        ui.label("Summary of this folder's small files — no path to act on.");
                        return;
                    }
                    if ui.button("Reveal in Finder").clicked() {
                        actions.borrow_mut().push(Action::Reveal(path.clone()));
                        ui.close();
                    }
                    if ui.button("Open").clicked() {
                        actions.borrow_mut().push(Action::OpenPath(path.clone()));
                        ui.close();
                    }
                    if is_dir && ui.button("Show in Treemap").clicked() {
                        actions.borrow_mut().push(Action::MapRoot(idx));
                        ui.close();
                    }
                    if !is_root {
                        ui.separator();
                        if ui.button("Move to Trash…").clicked() {
                            actions.borrow_mut().push(Action::RequestTrash(vec![(
                                idx,
                                path.clone(),
                                size,
                            )]));
                            ui.close();
                        }
                    }
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add(
                        egui::ProgressBar::new(frac)
                            .desired_width(130.0)
                            .text(util::human(size)),
                    );
                });
            });
        });

        if is_dir && expanded {
            const CAP: usize = 300;
            let len = tree.nodes[idx].children.len();
            for k in 0..len {
                if k >= CAP {
                    ui.horizontal(|ui| {
                        ui.add_space((depth as f32 + 1.0) * 14.0 + 16.0);
                        ui.weak(format!("… and {} more", len - CAP));
                    });
                    break;
                }
                let ch = tree.nodes[idx].children[k];
                self.tree_row(ui, tree, ch, depth + 1, actions);
            }
        }
    }

    // ---- treemap view -------------------------------------------------------

    fn ui_treemap(&mut self, ui: &mut egui::Ui, tree: &Tree, actions: &RefCell<Vec<Action>>) {
        let mut map_root = self.map_root.unwrap_or(tree.root);
        if tree.nodes[map_root].removed {
            map_root = tree.root;
        }

        ui.horizontal(|ui| {
            let at_root = map_root == tree.root;
            if ui.add_enabled(!at_root, egui::Button::new("Up")).clicked()
                && let Some(p) = tree.nodes[map_root].parent
            {
                actions.borrow_mut().push(Action::MapRoot(p));
            }
            let chain = tree.ancestry(map_root);
            for (k, &idx) in chain.iter().enumerate() {
                if k > 0 {
                    ui.label("›");
                }
                let nm = if tree.nodes[idx].name.is_empty() {
                    "/"
                } else {
                    &tree.nodes[idx].name
                };
                if ui.link(nm).clicked() {
                    actions.borrow_mut().push(Action::MapRoot(idx));
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(util::human(tree.nodes[map_root].size)).strong());
            });
        });
        ui.separator();

        let avail = ui.available_size();
        let (rect, resp) = ui.allocate_exact_size(avail, Sense::click());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::ZERO, ui.visuals().extreme_bg_color);

        let kids: Vec<(usize, u64)> = tree.nodes[map_root]
            .children
            .iter()
            .filter(|&&c| !tree.nodes[c].removed && tree.nodes[c].size > 0)
            .map(|&c| (c, tree.nodes[c].size))
            .collect();

        if kids.is_empty() {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "(empty)",
                FontId::proportional(13.0),
                ui.visuals().weak_text_color(),
            );
            return;
        }

        let tiles = treemap::squarified(&kids, rect.shrink(2.0));
        let pointer = resp.hover_pos();
        let mut hovered: Option<usize> = None;

        for (idx, r) in &tiles {
            if r.width() < 1.0 || r.height() < 1.0 {
                continue;
            }
            let node = &tree.nodes[*idx];
            let col = util::color_from_str(&node.name, node.is_dir);
            let tile = r.shrink(1.0);
            painter.rect_filled(tile, CornerRadius::same(2), col);
            if tile.width() > 46.0 && tile.height() > 18.0 {
                let txt = format!("{}  {}", node.name, util::human(node.size));
                painter.text(
                    tile.min + egui::vec2(4.0, 3.0),
                    Align2::LEFT_TOP,
                    txt,
                    FontId::proportional(11.0),
                    util::text_on(col),
                );
            }
            if let Some(pp) = pointer
                && tile.contains(pp)
            {
                hovered = Some(*idx);
            }
        }

        if let Some(h) = hovered {
            if let Some((_, r)) = tiles.iter().find(|(i, _)| *i == h) {
                painter.rect_stroke(
                    r.shrink(1.0),
                    CornerRadius::same(2),
                    Stroke::new(2.0, Color32::WHITE),
                    StrokeKind::Inside,
                );
            }
            let node = &tree.nodes[h];
            if let Some(pp) = pointer {
                let text = format!(
                    "{}\n{} · {} files",
                    node.name,
                    util::human(node.size),
                    node.file_count
                );
                let galley =
                    painter.layout(text, FontId::proportional(12.0), Color32::WHITE, 320.0);
                let pad = egui::vec2(6.0, 4.0);
                let bsize = galley.size() + pad * 2.0;
                // Keep the tooltip box inside the treemap area near right/bottom edges.
                let mut min = pp + egui::vec2(12.0, 12.0);
                if min.x + bsize.x > rect.max.x {
                    min.x = (pp.x - 12.0 - bsize.x).max(rect.min.x);
                }
                if min.y + bsize.y > rect.max.y {
                    min.y = (pp.y - 12.0 - bsize.y).max(rect.min.y);
                }
                let box_rect = Rect::from_min_size(min, bsize);
                painter.rect_filled(
                    box_rect,
                    CornerRadius::same(4),
                    Color32::from_black_alpha(225),
                );
                painter.galley(box_rect.min + pad, galley, Color32::WHITE);
            }
            if resp.clicked() {
                if tree.nodes[h].is_dir {
                    actions.borrow_mut().push(Action::MapRoot(h));
                } else {
                    actions.borrow_mut().push(Action::Select(h));
                }
            }
        }
    }

    // ---- dev caches view ----------------------------------------------------

    fn ui_caches(&mut self, ui: &mut egui::Ui, tree: &Tree, actions: &RefCell<Vec<Action>>) {
        let total_all: u64 = self.cache_hits.iter().map(|h| h.size).sum();
        let total_sel: u64 = self
            .cache_hits
            .iter()
            .filter(|h| self.cache_selected.contains(&h.node_idx))
            .map(|h| h.size)
            .sum();

        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} reclaimable", util::human(total_all))).strong());
            ui.separator();
            if ui.button("Select all").clicked() {
                actions.borrow_mut().push(Action::CacheSelectAll);
            }
            if ui.button("Select none").clicked() {
                actions.borrow_mut().push(Action::CacheSelectNone);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let label = format!("Move {} selected to Trash", util::human(total_sel));
                if ui
                    .add_enabled(total_sel > 0, egui::Button::new(label))
                    .clicked()
                {
                    actions.borrow_mut().push(Action::RequestTrashCaches);
                }
            });
        });
        ui.separator();

        if self.cache_hits.is_empty() {
            ui.weak("No known dev/build caches found in this folder.");
            return;
        }

        let root_path = tree.root_path.clone();

        // Group hits by category, ordered by total size descending.
        let mut groups: Vec<(caches::Category, Vec<usize>, u64)> = Vec::new();
        for (i, h) in self.cache_hits.iter().enumerate() {
            if let Some(g) = groups.iter_mut().find(|g| g.0 == h.category) {
                g.1.push(i);
                g.2 += h.size;
            } else {
                groups.push((h.category, vec![i], h.size));
            }
        }
        groups.sort_by(|a, b| b.2.cmp(&a.2));

        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                for (cat, idxs, tot) in &groups {
                    let header =
                        format!("{}  —  {} ({})", cat.label(), util::human(*tot), idxs.len());
                    egui::CollapsingHeader::new(header)
                        .id_salt(cat.label())
                        .default_open(true)
                        .show(ui, |ui| {
                            for &i in idxs {
                                let h = &self.cache_hits[i];
                                let node_idx = h.node_idx;
                                let rel = h
                                    .path
                                    .strip_prefix(&root_path)
                                    .unwrap_or(&h.path)
                                    .display()
                                    .to_string();
                                let checked = self.cache_selected.contains(&node_idx);
                                ui.horizontal(|ui| {
                                    let mut c = checked;
                                    if ui.checkbox(&mut c, "").changed() {
                                        actions.borrow_mut().push(Action::ToggleCache(node_idx));
                                    }
                                    let (chip, _) = ui.allocate_exact_size(
                                        egui::vec2(10.0, 10.0),
                                        Sense::hover(),
                                    );
                                    ui.painter().rect_filled(
                                        chip,
                                        CornerRadius::same(2),
                                        cat.color(),
                                    );
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(util::human(h.size)).monospace(),
                                        )
                                        .selectable(false),
                                    );
                                    ui.label(if rel.is_empty() { ".".to_string() } else { rel });
                                    ui.weak(h.note);
                                });
                            }
                        });
                }
            });
    }

    // ---- confirm dialog -----------------------------------------------------

    fn ui_confirm(&mut self, ctx: &egui::Context, actions: &RefCell<Vec<Action>>) {
        let Some(p) = &self.pending else {
            return;
        };
        let total: u64 = p.items.iter().map(|(_, _, s)| *s).sum();
        let count = p.items.len();

        egui::Window::new("Move to Trash")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(format!(
                    "Move {count} item(s) totaling {} to the Trash?",
                    util::human(total)
                ));
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .max_height(180.0)
                    .show(ui, |ui| {
                        for (_, path, size) in &p.items {
                            ui.horizontal(|ui| {
                                ui.monospace(util::human(*size));
                                ui.label(path.display().to_string());
                            });
                        }
                    });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        actions.borrow_mut().push(Action::CancelTrash);
                    }
                    let btn = egui::Button::new(
                        RichText::new(format!("Move {count} to Trash")).color(Color32::WHITE),
                    )
                    .fill(Color32::from_rgb(0xC0, 0x39, 0x2B));
                    if ui.add(btn).clicked() {
                        actions.borrow_mut().push(Action::ConfirmTrash);
                    }
                });
                ui.weak("Items go to the macOS Trash — restore them from there if needed.");
            });
    }
}
