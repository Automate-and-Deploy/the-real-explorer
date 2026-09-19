//! rust-explorer: a Windows Explorer style file browser with a VS Code style
//! folder tree in the sidebar. Left pane is a lazily expanded directory tree,
//! right pane is a sortable details list for the current directory.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use eframe::egui;
use egui_extras::{Column, TableBuilder};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 760.0])
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Rust Explorer",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(ExplorerApp::new()))
        }),
    )
}

/// One row in the details list.
#[derive(Clone)]
struct Entry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    size: u64,
    modified: Option<SystemTime>,
}

#[derive(Clone, Copy, PartialEq)]
enum SortKey {
    Name,
    Size,
    Modified,
}

struct ExplorerApp {
    cwd: PathBuf,
    entries: Vec<Entry>,
    /// Navigation history; `history[..=hist_pos]` is back stack.
    history: Vec<PathBuf>,
    hist_pos: usize,
    address: String,
    selected: Option<PathBuf>,
    sort_key: SortKey,
    sort_asc: bool,
    /// Tree state: which directories are expanded, and cached children per dir.
    expanded: BTreeSet<PathBuf>,
    tree_children: HashMap<PathBuf, Vec<PathBuf>>,
    roots: Vec<PathBuf>,
    show_hidden: bool,
    status: String,
}

impl ExplorerApp {
    fn new() -> Self {
        let start = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("C:\\"));
        let mut app = Self {
            cwd: start.clone(),
            entries: Vec::new(),
            history: vec![start.clone()],
            hist_pos: 0,
            address: start.display().to_string(),
            selected: None,
            sort_key: SortKey::Name,
            sort_asc: true,
            expanded: BTreeSet::new(),
            tree_children: HashMap::new(),
            roots: drive_roots(),
            show_hidden: false,
            status: String::new(),
        };
        app.reload();
        app.expand_ancestors(&start);
        app
    }

    /// Navigate to `path`, pushing onto history.
    fn navigate(&mut self, path: PathBuf) {
        if !path.is_dir() {
            self.status = format!("Not a directory: {}", path.display());
            return;
        }
        self.history.truncate(self.hist_pos + 1);
        self.history.push(path.clone());
        self.hist_pos = self.history.len() - 1;
        self.set_cwd(path);
    }

    fn set_cwd(&mut self, path: PathBuf) {
        self.cwd = path;
        self.address = self.cwd.display().to_string();
        self.selected = None;
        self.reload();
        let cwd = self.cwd.clone();
        self.expand_ancestors(&cwd);
    }

    fn go_back(&mut self) {
        if self.hist_pos > 0 {
            self.hist_pos -= 1;
            self.set_cwd(self.history[self.hist_pos].clone());
        }
    }

    fn go_forward(&mut self) {
        if self.hist_pos + 1 < self.history.len() {
            self.hist_pos += 1;
            self.set_cwd(self.history[self.hist_pos].clone());
        }
    }

    fn go_up(&mut self) {
        if let Some(p) = self.cwd.parent() {
            self.navigate(p.to_path_buf());
        }
    }

    /// Re-read the current directory into `entries` and sort.
    fn reload(&mut self) {
        self.entries.clear();
        match fs::read_dir(&self.cwd) {
            Ok(rd) => {
                for de in rd.flatten() {
                    let name = de.file_name().to_string_lossy().into_owned();
                    if !self.show_hidden && is_hidden(&name, &de.path()) {
                        continue;
                    }
                    let md = de.metadata().ok();
                    self.entries.push(Entry {
                        name,
                        path: de.path(),
                        is_dir: md.as_ref().map(|m| m.is_dir()).unwrap_or(false),
                        size: md.as_ref().map(|m| m.len()).unwrap_or(0),
                        modified: md.and_then(|m| m.modified().ok()),
                    });
                }
                self.status = format!("{} items", self.entries.len());
            }
            Err(e) => self.status = format!("Cannot read {}: {e}", self.cwd.display()),
        }
        self.sort_entries();
    }

    fn sort_entries(&mut self) {
        let key = self.sort_key;
        let asc = self.sort_asc;
        self.entries.sort_by(|a, b| {
            // Folders always first, like Explorer.
            let dir_ord = b.is_dir.cmp(&a.is_dir);
            if dir_ord != std::cmp::Ordering::Equal {
                return dir_ord;
            }
            let ord = match key {
                SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortKey::Size => a.size.cmp(&b.size),
                SortKey::Modified => a.modified.cmp(&b.modified),
            };
            if asc { ord } else { ord.reverse() }
        });
    }

    fn toggle_sort(&mut self, key: SortKey) {
        if self.sort_key == key {
            self.sort_asc = !self.sort_asc;
        } else {
            self.sort_key = key;
            self.sort_asc = true;
        }
        self.sort_entries();
    }

    /// Expand every ancestor of `path` in the tree so the cwd is visible.
    fn expand_ancestors(&mut self, path: &Path) {
        let mut p = path.to_path_buf();
        loop {
            self.expanded.insert(p.clone());
            match p.parent() {
                Some(parent) if parent != p => p = parent.to_path_buf(),
                _ => break,
            }
        }
    }

    /// Subdirectories of `dir`, cached after first read.
    fn tree_children(&mut self, dir: &Path) -> Vec<PathBuf> {
        if let Some(c) = self.tree_children.get(dir) {
            return c.clone();
        }
        let mut kids: Vec<PathBuf> = fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|de| de.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .map(|de| de.path())
                    .filter(|p| {
                        self.show_hidden
                            || !is_hidden(&p.file_name().unwrap_or_default().to_string_lossy(), p)
                    })
                    .collect()
            })
            .unwrap_or_default();
        kids.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
        self.tree_children.insert(dir.to_path_buf(), kids.clone());
        kids
    }

    fn open_entry(&mut self, e: &Entry) {
        if e.is_dir {
            self.navigate(e.path.clone());
        } else if let Err(err) = open::that_detached(&e.path) {
            self.status = format!("Open failed: {err}");
        }
    }

    // ---------- UI ----------

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.add_enabled(self.hist_pos > 0, egui::Button::new("◀")).clicked() {
                self.go_back();
            }
            if ui
                .add_enabled(self.hist_pos + 1 < self.history.len(), egui::Button::new("▶"))
                .clicked()
            {
                self.go_forward();
            }
            if ui.add_enabled(self.cwd.parent().is_some(), egui::Button::new("⬆")).clicked() {
                self.go_up();
            }
            if ui.button("⟳").clicked() {
                self.tree_children.clear();
                self.reload();
            }
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.address)
                    .desired_width(ui.available_width() - 110.0),
            );
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let p = PathBuf::from(self.address.trim());
                self.navigate(p);
            }
            if ui.checkbox(&mut self.show_hidden, "Hidden").changed() {
                self.tree_children.clear();
                self.reload();
            }
        });
    }

    fn tree_panel(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::both().auto_shrink([false; 2]).show(ui, |ui| {
            let roots = self.roots.clone();
            for root in roots {
                self.tree_node(ui, &root, 0);
            }
        });
    }

    fn tree_node(&mut self, ui: &mut egui::Ui, dir: &Path, depth: usize) {
        let is_open = self.expanded.contains(dir);
        let is_cwd = *dir == self.cwd;
        let label = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| dir.display().to_string());

        ui.horizontal(|ui| {
            ui.add_space(depth as f32 * 14.0);
            let arrow = if is_open { "⏷" } else { "⏵" };
            if ui.add(egui::Label::new(arrow).sense(egui::Sense::click())).clicked() {
                if is_open {
                    self.expanded.remove(dir);
                } else {
                    self.expanded.insert(dir.to_path_buf());
                }
            }
            let text = egui::RichText::new(format!("▪ {label}"));
            let text = if is_cwd { text.strong().color(egui::Color32::LIGHT_BLUE) } else { text };
            let resp = ui.add(egui::Label::new(text).sense(egui::Sense::click()).truncate());
            if resp.clicked() {
                self.expanded.insert(dir.to_path_buf());
                self.navigate(dir.to_path_buf());
            }
        });

        if is_open {
            for child in self.tree_children(dir) {
                self.tree_node(ui, &child, depth + 1);
            }
        }
    }

    fn details_panel(&mut self, ui: &mut egui::Ui) {
        let entries = self.entries.clone();
        let mut action: Option<(usize, bool)> = None; // (index, double)
        let mut sort: Option<SortKey> = None;

        let header = |ui: &mut egui::Ui, title: &str, key: SortKey, app: &Self| -> bool {
            let mark = if app.sort_key == key {
                if app.sort_asc { " ⬆" } else { " ⬇" }
            } else {
                ""
            };
            ui.add(egui::Label::new(egui::RichText::new(format!("{title}{mark}")).strong())
                .sense(egui::Sense::click()))
                .clicked()
        };

        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::remainder().at_least(200.0).clip(true))
            .column(Column::initial(90.0).at_least(60.0))
            .column(Column::initial(150.0).at_least(100.0))
            .column(Column::initial(90.0).at_least(60.0))
            .header(22.0, |mut h| {
                h.col(|ui| if header(ui, "Name", SortKey::Name, self) { sort = Some(SortKey::Name) });
                h.col(|ui| if header(ui, "Size", SortKey::Size, self) { sort = Some(SortKey::Size) });
                h.col(|ui| if header(ui, "Modified", SortKey::Modified, self) { sort = Some(SortKey::Modified) });
                h.col(|ui| { ui.strong("Type"); });
            })
            .body(|body| {
                body.rows(20.0, entries.len(), |mut row| {
                    let i = row.index();
                    let e = &entries[i];
                    row.set_selected(self.selected.as_ref() == Some(&e.path));
                    row.col(|ui| {
                        let icon = if e.is_dir { "▪" } else { "·" };
                        ui.add(egui::Label::new(format!("{icon} {}", e.name)).truncate());
                    });
                    row.col(|ui| {
                        if !e.is_dir {
                            ui.label(human_size(e.size));
                        }
                    });
                    row.col(|ui| {
                        ui.label(e.modified.map(fmt_time).unwrap_or_default());
                    });
                    row.col(|ui| {
                        ui.label(if e.is_dir { "Folder".to_string() } else { ext_type(&e.name) });
                    });
                    let r = row.response();
                    if r.double_clicked() {
                        action = Some((i, true));
                    } else if r.clicked() {
                        action = Some((i, false));
                    }
                });
            });

        if let Some(k) = sort {
            self.toggle_sort(k);
        }
        if let Some((i, double)) = action {
            let e = entries[i].clone();
            self.selected = Some(e.path.clone());
            if double {
                self.open_entry(&e);
            }
        }
    }
}

impl eframe::App for ExplorerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Keyboard: Backspace = up, Alt+Left/Right = back/forward, Enter = open selected.
        let (bs, alt_l, alt_r, enter) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Backspace) && !i.modifiers.any(),
                i.modifiers.alt && i.key_pressed(egui::Key::ArrowLeft),
                i.modifiers.alt && i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::Enter),
            )
        });
        let address_focused = ctx.memory(|m| m.focused().is_some());
        if !address_focused {
            if bs { self.go_up(); }
            if alt_l { self.go_back(); }
            if alt_r { self.go_forward(); }
            if enter {
                if let Some(sel) = self.selected.clone() {
                    if let Some(e) = self.entries.iter().find(|e| e.path == sel).cloned() {
                        self.open_entry(&e);
                    }
                }
            }
        }

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            self.toolbar(ui);
            ui.add_space(4.0);
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.label(&self.status);
        });
        egui::SidePanel::left("tree")
            .resizable(true)
            .default_width(260.0)
            .min_width(140.0)
            .show(ctx, |ui| self.tree_panel(ui));
        egui::CentralPanel::default().show(ctx, |ui| self.details_panel(ui));
    }
}

// ---------- helpers ----------

/// Drive roots on Windows (probes A: through Z:), `/` elsewhere.
fn drive_roots() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        (b'A'..=b'Z')
            .map(|c| PathBuf::from(format!("{}:\\", c as char)))
            .filter(|p| p.exists())
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec![PathBuf::from("/")]
    }
}

/// Hidden = dot-prefixed, or Windows hidden/system attribute.
fn is_hidden(name: &str, path: &Path) -> bool {
    if name.starts_with('.') {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const HIDDEN: u32 = 0x2;
        const SYSTEM: u32 = 0x4;
        if let Ok(md) = fs::symlink_metadata(path) {
            return md.file_attributes() & (HIDDEN | SYSTEM) != 0;
        }
    }
    let _ = path;
    false
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

fn fmt_time(t: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Local> = t.into();
    dt.format("%Y-%m-%d %H:%M").to_string()
}

fn ext_type(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() => format!("{} file", ext.to_uppercase()),
        _ => "File".to_string(),
    }
}
