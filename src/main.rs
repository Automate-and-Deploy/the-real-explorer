//! The Real Explorer: a Windows Explorer style file browser with a VS Code style
//! folder tree in the sidebar and an optional AI chat on the right.
//! Left pane is a lazily expanded directory tree, centre is a sortable details
//! list for the current directory. Menus cover file operations, settings hold
//! the chat backend (Claude Code CLI or any OpenAI-compatible endpoint).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod attach;
mod chat;
mod config;
mod editor;
mod harness;
mod icons;
mod lsp;
mod platform;
mod theme;
mod titlebar;
mod trash_ops;

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use config::{Backend, Config, Theme};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([640.0, 400.0])
            .with_decorations(false)
            .with_title("The Real Explorer"),
        ..Default::default()
    };
    eframe::run_native(
        "The Real Explorer",
        options,
        Box::new(|cc| {
            icons::install(&cc.egui_ctx);
            let app = ExplorerApp::new();
            theme::apply(&cc.egui_ctx, app.cfg.theme);
            Ok(Box::new(app))
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
    Type,
}

/// Result of the Properties window's background scan.
struct Props {
    path: PathBuf,
    is_dir: bool,
    created: Option<SystemTime>,
    modified: Option<SystemTime>,
    /// (bytes, files, folders) once the scan finishes.
    totals: std::sync::Arc<std::sync::Mutex<Option<(u64, u64, u64)>>>,
}

/// Reversible actions for Ctrl+Z.
enum Undo {
    /// (current path, previous path)
    Rename(PathBuf, PathBuf),
    /// Original path of an item moved to the recycle bin.
    Delete(PathBuf),
}

/// Which view the centre panel shows.
#[derive(Clone, Copy, PartialEq)]
enum Body {
    Explorer,
    Ide,
}

/// A pending text or confirm dialog.
enum Modal {
    NewFolder { name: String },
    NewFile { name: String },
    Rename { path: PathBuf, name: String },
    Delete { path: PathBuf },
}

struct ExplorerApp {
    cfg: Config,
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
    /// Files per directory, shown in the tree only while the IDE tab is active.
    tree_files: HashMap<PathBuf, Vec<PathBuf>>,
    roots: Vec<PathBuf>,
    show_tree: bool,
    status: String,
    chat: chat::ChatPanel,
    settings_open: bool,
    /// Internal clipboard: paths and whether the paste should move them.
    clipboard: Option<(Vec<PathBuf>, bool)>,
    modal: Option<Modal>,
    /// Last undoable action.
    undo: Option<Undo>,
    compact: bool,
    group_by_type: bool,
    props: Option<Props>,
    /// Row index the open context menu refers to; None means empty space.
    menu_row: Option<usize>,
    body: Body,
    editor: editor::Editor,
}

impl ExplorerApp {
    fn new() -> Self {
        let cfg = Config::load();
        let start = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("C:\\"));
        let mut app = Self {
            chat: chat::ChatPanel::new(cfg.chat_open),
            cfg,
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
            tree_files: HashMap::new(),
            roots: drive_roots(),
            show_tree: true,
            status: String::new(),
            settings_open: false,
            clipboard: None,
            modal: None,
            undo: None,
            compact: false,
            group_by_type: false,
            props: None,
            menu_row: None,
            body: Body::Explorer,
            editor: editor::Editor::new(),
        };
        app.reload();
        app.expand_ancestors(&start);
        app
    }

    fn save_cfg(&mut self) {
        self.cfg.chat_open = self.chat.open;
        if let Err(e) = self.cfg.save() {
            self.status = format!("Settings not saved: {e}");
        }
    }

    // ---------- navigation ----------

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
                    if !self.cfg.show_hidden && is_hidden(&name, &de.path()) {
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

    /// Reload the list and drop the tree cache so new or renamed folders show.
    fn refresh_all(&mut self) {
        self.tree_children.clear();
        self.tree_files.clear();
        self.reload();
    }

    fn sort_entries(&mut self) {
        let key = self.sort_key;
        let asc = self.sort_asc;
        let group = self.group_by_type;
        self.entries.sort_by(|a, b| {
            // Folders always first, like Explorer.
            let dir_ord = b.is_dir.cmp(&a.is_dir);
            if dir_ord != std::cmp::Ordering::Equal {
                return dir_ord;
            }
            if group {
                let g = ext_type(&a.name).cmp(&ext_type(&b.name));
                if g != std::cmp::Ordering::Equal {
                    return g;
                }
            }
            let ord = match key {
                SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortKey::Size => a.size.cmp(&b.size),
                SortKey::Modified => a.modified.cmp(&b.modified),
                SortKey::Type => ext_type(&a.name)
                    .cmp(&ext_type(&b.name))
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
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
        let show_hidden = self.cfg.show_hidden;
        let mut kids: Vec<PathBuf> = fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|de| de.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .map(|de| de.path())
                    .filter(|p| {
                        show_hidden
                            || !is_hidden(&p.file_name().unwrap_or_default().to_string_lossy(), p)
                    })
                    .collect()
            })
            .unwrap_or_default();
        kids.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
        self.tree_children.insert(dir.to_path_buf(), kids.clone());
        kids
    }

    /// Files directly inside `dir`, cached; used by the tree in IDE mode.
    fn tree_files(&mut self, dir: &Path) -> Vec<PathBuf> {
        if let Some(c) = self.tree_files.get(dir) {
            return c.clone();
        }
        let show_hidden = self.cfg.show_hidden;
        let mut files: Vec<PathBuf> = fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|de| de.file_type().map(|t| !t.is_dir()).unwrap_or(false))
                    .map(|de| de.path())
                    .filter(|p| {
                        show_hidden
                            || !is_hidden(&p.file_name().unwrap_or_default().to_string_lossy(), p)
                    })
                    .collect()
            })
            .unwrap_or_default();
        files.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
        self.tree_files.insert(dir.to_path_buf(), files.clone());
        files
    }

    fn open_entry(&mut self, e: &Entry) {
        if e.is_dir {
            self.navigate(e.path.clone());
        } else {
            self.open_in_ide(&e.path);
        }
    }

    fn open_in_ide(&mut self, path: &Path) {
        let root = self.cwd.clone();
        self.editor.open(path, &root, &self.cfg.lsp_servers);
        self.body = Body::Ide;
    }

    fn open_with_system(&mut self, path: &Path) {
        if let Err(err) = open::that_detached(path) {
            self.status = format!("Open failed: {err}");
        }
    }

    // ---------- file operations ----------

    fn selected_entry(&self) -> Option<Entry> {
        let sel = self.selected.as_ref()?;
        self.entries.iter().find(|e| &e.path == sel).cloned()
    }

    fn copy_selected(&mut self, cut: bool) {
        if let Some(p) = self.selected.clone() {
            self.clipboard = Some((vec![p], cut));
            self.status = if cut { "Cut 1 item".into() } else { "Copied 1 item".into() };
        }
    }

    fn paste(&mut self) {
        let Some((paths, cut)) = self.clipboard.clone() else { return };
        for src in &paths {
            let Some(name) = src.file_name() else { continue };
            let mut dst = self.cwd.join(name);
            if dst == *src {
                dst = unique_name(&dst);
            }
            let r = if cut { move_path(src, &dst) } else { copy_path(src, &dst) };
            if let Err(e) = r {
                self.status = format!("Paste failed: {e}");
                return;
            }
        }
        if cut {
            self.clipboard = None;
        }
        self.refresh_all();
        self.status = format!("Pasted {} item(s)", paths.len());
    }

    fn open_terminal(&mut self) {
        if let Err(e) = platform::open_terminal(&self.cwd) {
            self.status = format!("Terminal failed: {e}");
        }
    }

    fn open_in_code(&mut self, path: &Path) {
        if let Err(e) = platform::open_in_code(path) {
            self.status = format!("VS Code failed: {e}");
        }
    }

    fn reveal(&mut self, path: &Path) {
        if let Err(e) = platform::reveal(path) {
            self.status = format!("{} failed: {e}", platform::file_manager_name());
        }
    }

    fn undo_label(&self) -> &'static str {
        match self.undo {
            Some(Undo::Rename(..)) => "Undo Rename	Ctrl+Z",
            Some(Undo::Delete(..)) => "Undo Delete	Ctrl+Z",
            None => "Undo	Ctrl+Z",
        }
    }

    fn undo(&mut self) {
        let r = match self.undo.take() {
            Some(Undo::Rename(cur, prev)) => fs::rename(&cur, &prev)
                .map(|()| format!("Restored {}", prev.display()))
                .map_err(|e| e.to_string()),
            Some(Undo::Delete(orig)) => match trash_ops::restore(&orig) {
                Ok(true) => Ok(format!("Restored {}", orig.display())),
                Ok(false) => Err(format!("{} not found in {}", orig.display(), trash_ops::bin_name())),
                Err(e) => Err(e),
            },
            None => return,
        };
        match r {
            Ok(msg) => {
                self.status = msg;
                self.refresh_all();
            }
            Err(e) => self.status = format!("Undo failed: {e}"),
        }
    }

    /// Open the Properties window and start a background size scan.
    fn show_properties(&mut self, path: PathBuf) {
        let md = fs::metadata(&path).ok();
        let is_dir = md.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let totals = std::sync::Arc::new(std::sync::Mutex::new(None));
        let t2 = totals.clone();
        let p2 = path.clone();
        std::thread::spawn(move || {
            fn walk(p: &Path, acc: &mut (u64, u64, u64)) {
                if let Ok(rd) = fs::read_dir(p) {
                    for de in rd.flatten() {
                        let Ok(ft) = de.file_type() else { continue };
                        if ft.is_dir() {
                            acc.2 += 1;
                            walk(&de.path(), acc);
                        } else {
                            acc.1 += 1;
                            acc.0 += de.metadata().map(|m| m.len()).unwrap_or(0);
                        }
                    }
                }
            }
            let mut acc = (0u64, 0u64, 0u64);
            if p2.is_dir() {
                walk(&p2, &mut acc);
            } else {
                acc = (fs::metadata(&p2).map(|m| m.len()).unwrap_or(0), 1, 0);
            }
            *t2.lock().unwrap() = Some(acc);
        });
        self.props = Some(Props {
            path,
            is_dir,
            created: md.as_ref().and_then(|m| m.created().ok()),
            modified: md.and_then(|m| m.modified().ok()),
            totals,
        });
    }

    /// Apply a confirmed modal action.
    fn run_modal(&mut self, m: Modal) {
        let r: Result<(), String> = match m {
            Modal::NewFolder { name } => {
                fs::create_dir(self.cwd.join(name.trim())).map_err(|e| e.to_string())
            }
            Modal::NewFile { name } => {
                let p = self.cwd.join(name.trim());
                if p.exists() {
                    Err("already exists".into())
                } else {
                    fs::write(&p, b"").map_err(|e| e.to_string())
                }
            }
            Modal::Rename { path, name } => {
                let dst = path.with_file_name(name.trim());
                fs::rename(&path, &dst).map_err(|e| e.to_string()).map(|()| {
                    self.undo = Some(Undo::Rename(dst, path));
                })
            }
            Modal::Delete { path } => trash_ops::delete_to_trash(&path).map(|()| {
                self.undo = Some(Undo::Delete(path));
            }),
        };
        match r {
            Ok(()) => self.refresh_all(),
            Err(e) => self.status = format!("Failed: {e}"),
        }
    }

    // ---------- UI ----------

    fn menu_bar(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let has_sel = self.selected.is_some();
        egui::menu::bar(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("New folder\tCtrl+Shift+N").clicked() {
                    self.modal = Some(Modal::NewFolder { name: "New folder".into() });
                    ui.close_menu();
                }
                if ui.button("New file").clicked() {
                    self.modal = Some(Modal::NewFile { name: "untitled.txt".into() });
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Open terminal here").clicked() {
                    self.open_terminal();
                    ui.close_menu();
                }
                if ui.button("Settings\tCtrl+,").clicked() {
                    self.settings_open = true;
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Exit").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Edit", |ui| {
                if ui.add_enabled(self.body == Body::Ide, egui::Button::new("Format document	Ctrl+Shift+F")).clicked() {
                    match self.editor.format_active() {
                        Ok(()) => self.status = "Formatted".into(),
                        Err(e) => self.status = e,
                    }
                    ui.close_menu();
                }
                ui.separator();
                if ui.add_enabled(self.undo.is_some(), egui::Button::new(self.undo_label())).clicked() {
                    self.undo();
                    ui.close_menu();
                }
                ui.separator();
                if ui.add_enabled(has_sel, egui::Button::new("Copy\tCtrl+C")).clicked() {
                    self.copy_selected(false);
                    ui.close_menu();
                }
                if ui.add_enabled(has_sel, egui::Button::new("Cut\tCtrl+X")).clicked() {
                    self.copy_selected(true);
                    ui.close_menu();
                }
                if ui.add_enabled(self.clipboard.is_some(), egui::Button::new("Paste\tCtrl+V")).clicked() {
                    self.paste();
                    ui.close_menu();
                }
                ui.separator();
                if ui.add_enabled(has_sel, egui::Button::new("Rename\tF2")).clicked() {
                    if let Some(e) = self.selected_entry() {
                        self.modal = Some(Modal::Rename { path: e.path, name: e.name });
                    }
                    ui.close_menu();
                }
                if ui.add_enabled(has_sel, egui::Button::new("Delete\tDel")).clicked() {
                    if let Some(p) = self.selected.clone() {
                        self.modal = Some(Modal::Delete { path: p });
                    }
                    ui.close_menu();
                }
                ui.separator();
                if ui.add_enabled(has_sel, egui::Button::new("Copy path")).clicked() {
                    if let Some(p) = &self.selected {
                        ctx.copy_text(p.display().to_string());
                    }
                    ui.close_menu();
                }
            });
            ui.menu_button("View", |ui| {
                if ui.checkbox(&mut self.show_tree, "Folder tree").clicked() {
                    ui.close_menu();
                }
                if ui.checkbox(&mut self.chat.open, "Chat panel").clicked() {
                    self.save_cfg();
                    ui.close_menu();
                }
                if ui.checkbox(&mut self.cfg.show_hidden, "Hidden files").clicked() {
                    self.refresh_all();
                    self.save_cfg();
                    ui.close_menu();
                }
                ui.separator();
                ui.menu_button("Theme", |ui| {
                    let mut changed = false;
                    for (t, label) in [
                        (Theme::System, "System"),
                        (Theme::Light, "Light"),
                        (Theme::Dark, "Dark"),
                        (Theme::Omarchy, "Omarchy"),
                    ] {
                        changed |= ui.radio_value(&mut self.cfg.theme, t, label).clicked();
                    }
                    if changed {
                        theme::apply(ctx, self.cfg.theme);
                        self.save_cfg();
                        ui.close_menu();
                    }
                });
                ui.separator();
                if ui.button("Refresh\tF5").clicked() {
                    self.refresh_all();
                    ui.close_menu();
                }
            });
        });
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.add_enabled(self.hist_pos > 0, egui::Button::new(icons::BACK)).clicked() {
                self.go_back();
            }
            if ui
                .add_enabled(self.hist_pos + 1 < self.history.len(), egui::Button::new(icons::FORWARD))
                .clicked()
            {
                self.go_forward();
            }
            if ui.add_enabled(self.cwd.parent().is_some(), egui::Button::new(icons::UP)).clicked() {
                self.go_up();
            }
            if ui.button(icons::REFRESH).clicked() {
                self.refresh_all();
            }
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.address)
                    .desired_width(ui.available_width() - 120.0),
            );
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let p = PathBuf::from(self.address.trim());
                self.navigate(p);
            }
            if ui.selectable_label(self.chat.open, format!("{} Chat", icons::CHAT)).clicked() {
                self.chat.open = !self.chat.open;
                self.save_cfg();
            }
            if ui.button(icons::SETTINGS).on_hover_text("Settings").clicked() {
                self.settings_open = true;
            }
        });
    }

    fn tree_panel(&mut self, ui: &mut egui::Ui) {
        // Labels here are navigation, not prose; a drag must not sweep a text selection.
        ui.style_mut().interaction.selectable_labels = false;
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
            let arrow = if is_open { icons::CARET_OPEN } else { icons::CARET_CLOSED };
            if ui.add(egui::Label::new(arrow).sense(egui::Sense::click())).clicked() {
                if is_open {
                    self.expanded.remove(dir);
                } else {
                    self.expanded.insert(dir.to_path_buf());
                }
            }
            let dark = ui.visuals().dark_mode;
            let (glyph, color) = if depth == 0 {
                (icons::DRIVE, ui.visuals().text_color())
            } else {
                let (g, c) = icons::folder(is_open || is_cwd);
                (g, icons::tint(c, dark))
            };
            let mut job = egui::text::LayoutJob::default();
            let font = egui::TextStyle::Body.resolve(ui.style());
            job.append(&format!("{glyph} "), 0.0, egui::TextFormat { font_id: font.clone(), color, ..Default::default() });
            let name_color = if is_cwd { ui.visuals().selection.stroke.color } else { ui.visuals().text_color() };
            job.append(&label, 0.0, egui::TextFormat { font_id: font, color: name_color, ..Default::default() });
            let resp = ui.add(egui::Label::new(job).sense(egui::Sense::click_and_drag()).truncate());
            resp.dnd_set_drag_payload(attach::DragPaths(vec![dir.to_path_buf()]));
            if resp.clicked() {
                self.expanded.insert(dir.to_path_buf());
                self.navigate(dir.to_path_buf());
            }
        });

        if is_open {
            for child in self.tree_children(dir) {
                self.tree_node(ui, &child, depth + 1);
            }
            if self.body == Body::Ide {
                let open_path = self.editor.docs.get(self.editor.active).map(|d| d.path.clone());
                for file in self.tree_files(dir) {
                    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let (glyph, color) = icons::file(&name);
                    let color = icons::tint(color, ui.visuals().dark_mode);
                    let is_active = open_path.as_ref() == Some(&file);
                    ui.horizontal(|ui| {
                        ui.add_space((depth + 1) as f32 * 14.0 + 16.0);
                        let font = egui::TextStyle::Body.resolve(ui.style());
                        let mut job = egui::text::LayoutJob::default();
                        job.append(&format!("{glyph} "), 0.0, egui::TextFormat { font_id: font.clone(), color, ..Default::default() });
                        let name_color = if is_active { ui.visuals().selection.stroke.color } else { ui.visuals().text_color() };
                        job.append(&name, 0.0, egui::TextFormat { font_id: font, color: name_color, ..Default::default() });
                        let r = ui.add(egui::Label::new(job).sense(egui::Sense::click_and_drag()).truncate());
                        r.dnd_set_drag_payload(attach::DragPaths(vec![file.clone()]));
                        if r.clicked() {
                            self.open_in_ide(&file);
                        }
                    });
                }
            }
        }
    }

    fn details_panel(&mut self, ui: &mut egui::Ui) {
        ui.style_mut().interaction.selectable_labels = false;
        let entries = self.entries.clone();
        let mut action: Option<(usize, bool)> = None; // (index, double)
        let mut sort: Option<SortKey> = None;

        let header = |ui: &mut egui::Ui, title: &str, key: SortKey, app: &Self| -> bool {
            let mark = if app.sort_key == key {
                if app.sort_asc { " ▴" } else { " ▾" }
            } else {
                ""
            };
            ui.add(egui::Label::new(egui::RichText::new(format!("{title}{mark}")).strong())
                .sense(egui::Sense::click()))
                .clicked()
        };

        let mut hovered_row: Option<usize> = None;
        // Only the list area, not the tab strip above it.
        let pane = ui.available_rect_before_wrap();

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
                body.rows(if self.compact { 18.0 } else { 24.0 }, entries.len(), |mut row| {
                    let i = row.index();
                    let e = &entries[i];
                    row.set_selected(self.selected.as_ref() == Some(&e.path));
                    row.col(|ui| {
                        let (glyph, color) = if e.is_dir { icons::folder(false) } else { icons::file(&e.name) };
                        let color = icons::tint(color, ui.visuals().dark_mode);
                        let font = egui::TextStyle::Body.resolve(ui.style());
                        let mut job = egui::text::LayoutJob::default();
                        job.append(&format!("{glyph} "), 0.0, egui::TextFormat { font_id: font.clone(), color, ..Default::default() });
                        job.append(&e.name, 0.0, egui::TextFormat { font_id: font, color: ui.visuals().text_color(), ..Default::default() });
                        ui.add(egui::Label::new(job).truncate());
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
                    if r.contains_pointer() {
                        hovered_row = Some(i);
                    }
                });
            });

        if let Some(k) = sort {
            self.toggle_sort(k);
        }
        // Right-click on empty space: mirrors the Windows 11 Explorer menu.
        let mut sort: Option<SortKey> = None;
        let mut asc: Option<bool> = None;
        // One catch-all interact over the whole pane, registered after the rows so it
        // is the topmost widget and owns the single context menu. Which items it shows
        // depends on the row under the pointer when the right button went down.
        let bg = ui.interact(pane, ui.id().with("bg"), egui::Sense::click());
        if let Some(i) = hovered_row {
            if bg.double_clicked() {
                action = Some((i, true));
            } else if bg.clicked() {
                action = Some((i, false));
            }
        }
        if bg.secondary_clicked() {
            self.menu_row = hovered_row;
            if let Some(i) = hovered_row {
                self.selected = Some(entries[i].path.clone());
            }
        }
        if let Some((i, double)) = action {
            let e = entries[i].clone();
            self.selected = Some(e.path.clone());
            // Folders open on single left click (tree reveals them); files need a double.
            if double || e.is_dir {
                self.open_entry(&e);
            }
        }
        if let Some(i) = self.menu_row.filter(|i| *i < entries.len()) {
            let e = entries[i].clone();
            let mut pick: Option<&'static str> = None;
            bg.context_menu(|ui| {
                let reveal_label = if cfg!(windows) {
                    "Show in Explorer"
                } else if cfg!(target_os = "macos") {
                    "Reveal in Finder"
                } else {
                    "Show in file manager"
                };
                let items: &[(&str, &str)] = &[
                    ("Open", "open"),
                    ("Open with default app", "system"),
                    ("Open with Code", "code"),
                    (reveal_label, "reveal"),
                    ("", ""),
                    ("Cut\tCtrl+X", "cut"),
                    ("Copy\tCtrl+C", "copy"),
                    ("Copy path", "path"),
                    ("", ""),
                    ("Rename\tF2", "rename"),
                    ("Delete\tDel", "delete"),
                    ("", ""),
                    ("Properties\tAlt+Enter", "props"),
                ];
                for (label, key) in items {
                    if label.is_empty() {
                        ui.separator();
                    } else if ui.button(*label).clicked() {
                        pick = Some(key);
                        ui.close_menu();
                    }
                }
            });
            match pick {
                Some("open") => self.open_entry(&e),
                Some("system") => self.open_with_system(&e.path),
                Some("code") => self.open_in_code(&e.path),
                Some("reveal") => self.reveal(&e.path),
                Some("cut") => self.copy_selected(true),
                Some("copy") => self.copy_selected(false),
                Some("path") => ui.ctx().copy_text(e.path.display().to_string()),
                Some("rename") => self.modal = Some(Modal::Rename { path: e.path, name: e.name }),
                Some("delete") => self.modal = Some(Modal::Delete { path: e.path }),
                Some("props") => self.show_properties(e.path),
                _ => {}
            }
            return;
        }
        bg.context_menu(|ui| {
            ui.menu_button("View", |ui| {
                if ui.checkbox(&mut self.compact, "Compact rows").clicked() {
                    ui.close_menu();
                }
                if ui.checkbox(&mut self.show_tree, "Folder tree").clicked() {
                    ui.close_menu();
                }
                if ui.checkbox(&mut self.chat.open, "Assistant panel").clicked() {
                    self.save_cfg();
                    ui.close_menu();
                }
                if ui.checkbox(&mut self.cfg.show_hidden, "Hidden files").clicked() {
                    self.refresh_all();
                    self.save_cfg();
                    ui.close_menu();
                }
            });
            ui.menu_button("Sort by", |ui| {
                for (k, label) in [
                    (SortKey::Name, "Name"),
                    (SortKey::Size, "Size"),
                    (SortKey::Modified, "Date modified"),
                    (SortKey::Type, "Type"),
                ] {
                    if ui.radio(self.sort_key == k, label).clicked() {
                        sort = Some(k);
                        ui.close_menu();
                    }
                }
                ui.separator();
                if ui.radio(self.sort_asc, "Ascending").clicked() {
                    asc = Some(true);
                    ui.close_menu();
                }
                if ui.radio(!self.sort_asc, "Descending").clicked() {
                    asc = Some(false);
                    ui.close_menu();
                }
            });
            ui.menu_button("Group by", |ui| {
                if ui.radio(!self.group_by_type, "None").clicked() {
                    self.group_by_type = false;
                    self.sort_entries();
                    ui.close_menu();
                }
                if ui.radio(self.group_by_type, "Type").clicked() {
                    self.group_by_type = true;
                    self.sort_entries();
                    ui.close_menu();
                }
            });
            ui.separator();
            if ui.add_enabled(self.undo.is_some(), egui::Button::new(self.undo_label())).clicked() {
                self.undo();
                ui.close_menu();
            }
            if ui.add_enabled(self.clipboard.is_some(), egui::Button::new("Paste\tCtrl+V")).clicked() {
                self.paste();
                ui.close_menu();
            }
            ui.menu_button("New", |ui| {
                if ui.button("Folder").clicked() {
                    self.modal = Some(Modal::NewFolder { name: "New folder".into() });
                    ui.close_menu();
                }
                if ui.button("Text document").clicked() {
                    self.modal = Some(Modal::NewFile { name: "New Text Document.txt".into() });
                    ui.close_menu();
                }
            });
            if ui.button("Properties\tAlt+Enter").clicked() {
                let p = self.cwd.clone();
                self.show_properties(p);
                ui.close_menu();
            }
            ui.separator();
            if ui.button("Open in Terminal").clicked() {
                self.open_terminal();
                ui.close_menu();
            }
            if ui.button("Open with Code").clicked() {
                let p = self.cwd.clone();
                self.open_in_code(&p);
                ui.close_menu();
            }
            if ui.button(format!("Open in {}", platform::file_manager_name())).clicked() {
                let p = self.cwd.clone();
                self.reveal(&p);
                ui.close_menu();
            }
        });
        if let Some(k) = sort {
            if self.sort_key != k {
                self.sort_key = k;
                self.sort_asc = true;
                self.sort_entries();
            }
        }
        if let Some(a) = asc {
            self.sort_asc = a;
            self.sort_entries();
        }
    }

    fn properties_window(&mut self, ctx: &egui::Context) {
        let Some(p) = &self.props else { return };
        let mut open = true;
        let name = p
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.path.display().to_string());
        let totals = *p.totals.lock().unwrap();
        egui::Window::new(format!("{name} Properties"))
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                egui::Grid::new("props").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.label("Type");
                    ui.label(if p.is_dir { "Folder".to_string() } else { ext_type(&name) });
                    ui.end_row();
                    ui.label("Location");
                    ui.add(egui::Label::new(p.path.parent().map(|x| x.display().to_string()).unwrap_or_default()).wrap());
                    ui.end_row();
                    ui.label("Size");
                    match totals {
                        Some((bytes, _, _)) => {
                            ui.label(format!("{} ({bytes} bytes)", human_size(bytes)));
                        }
                        None => {
                            ui.spinner();
                        }
                    }
                    ui.end_row();
                    if p.is_dir {
                        ui.label("Contains");
                        match totals {
                            Some((_, files, dirs)) => {
                                ui.label(format!("{files} files, {dirs} folders"));
                            }
                            None => {
                                ui.label("scanning");
                            }
                        }
                        ui.end_row();
                    }
                    ui.label("Created");
                    ui.label(p.created.map(fmt_time).unwrap_or_default());
                    ui.end_row();
                    ui.label("Modified");
                    ui.label(p.modified.map(fmt_time).unwrap_or_default());
                    ui.end_row();
                });
                if totals.is_none() {
                    ctx.request_repaint_after(std::time::Duration::from_millis(200));
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Copy path").clicked() {
                        ctx.copy_text(p.path.display().to_string());
                    }
                    if cfg!(windows) && ui.button("Windows properties").clicked() {
                        let _ = platform::native_properties(&p.path);
                    }
                });
            });
        if !open {
            self.props = None;
        }
    }

    fn modal_window(&mut self, ctx: &egui::Context) {
        let Some(mut m) = self.modal.take() else { return };
        let mut confirm = false;
        let mut cancel = false;
        let title = match &m {
            Modal::NewFolder { .. } => "New folder",
            Modal::NewFile { .. } => "New file",
            Modal::Rename { .. } => "Rename",
            Modal::Delete { .. } => "Delete",
        };
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                match &mut m {
                    Modal::NewFolder { name } | Modal::NewFile { name } | Modal::Rename { name, .. } => {
                        let r = ui.add(egui::TextEdit::singleline(name).desired_width(320.0));
                        r.request_focus();
                        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            confirm = true;
                        }
                    }
                    Modal::Delete { path } => {
                        ui.label(format!("Move {} to the {}?", path.display(), trash_ops::bin_name()));
                    }
                }
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        confirm = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    cancel = true;
                }
            });
        if confirm {
            self.run_modal(m);
        } else if !cancel {
            self.modal = Some(m);
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        let mut changed = false;
        let mut reload = false;
        egui::Window::new("Settings")
            .open(&mut open)
            .default_width(460.0)
            .show(ctx, |ui| {
                ui.heading("Assistant backend");
                changed |= ui
                    .radio_value(&mut self.cfg.backend, Backend::ClaudeCode, "Claude Code CLI")
                    .changed();
                changed |= ui
                    .radio_value(
                        &mut self.cfg.backend,
                        Backend::OpenAiCompatible,
                        "OpenAI-compatible endpoint",
                    )
                    .changed();
                ui.add_space(8.0);
                match self.cfg.backend {
                    Backend::ClaudeCode => {
                        ui.horizontal(|ui| {
                            ui.label("Permission mode");
                            egui::ComboBox::from_id_salt("perm")
                                .selected_text(&self.cfg.claude_permission_mode)
                                .show_ui(ui, |ui| {
                                    for m in ["default", "acceptEdits", "plan", "bypassPermissions"] {
                                        changed |= ui
                                            .selectable_value(&mut self.cfg.claude_permission_mode, m.into(), m)
                                            .changed();
                                    }
                                });
                        });
                        ui.small("'default' stalls on any tool prompt; this panel has no approval UI yet.");
                    }
                    Backend::OpenAiCompatible => {
                        egui::Grid::new("openai").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                            ui.label("Base URL");
                            changed |= ui.add(egui::TextEdit::singleline(&mut self.cfg.base_url).desired_width(320.0)).changed();
                            ui.end_row();
                            ui.label("API key");
                            changed |= ui
                                .add(egui::TextEdit::singleline(&mut self.cfg.api_key).password(true).desired_width(320.0))
                                .changed();
                            ui.end_row();
                            ui.label("Model");
                            changed |= ui.add(egui::TextEdit::singleline(&mut self.cfg.model).desired_width(320.0)).changed();
                            ui.end_row();
                        });
                        ui.horizontal(|ui| {
                            ui.small("Presets:");
                            for (name, url, model) in [
                                ("Ollama", "http://localhost:11434/v1", "qwen2.5-coder:32b"),
                                ("LM Studio", "http://localhost:1234/v1", "local-model"),
                                ("OpenRouter", "https://openrouter.ai/api/v1", "anthropic/claude-sonnet-5"),
                                ("OpenAI", "https://api.openai.com/v1", "gpt-5"),
                            ] {
                                if ui.small_button(name).clicked() {
                                    self.cfg.base_url = url.into();
                                    self.cfg.model = model.into();
                                    changed = true;
                                }
                            }
                        });
                        ui.small(format!("Key is stored in plaintext at {}", Config::path().display()));
                    }
                }
                ui.add_space(8.0);
                ui.heading("Language servers");
                ui.small("Command per file type. Servers start on first open; missing binaries are reported in the status bar.");
                let mut remove: Option<usize> = None;
                egui::Grid::new("lsp").num_columns(3).spacing([8.0, 4.0]).show(ui, |ui| {
                    for (i, srv) in self.cfg.lsp_servers.iter_mut().enumerate() {
                        let mut exts = srv.extensions.join(",");
                        if ui.add(egui::TextEdit::singleline(&mut exts).desired_width(110.0)).changed() {
                            srv.extensions = exts.split(',').map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty()).collect();
                            changed = true;
                        }
                        let mut cmdline = if srv.args.is_empty() { srv.command.clone() } else { format!("{} {}", srv.command, srv.args.join(" ")) };
                        if ui.add(egui::TextEdit::singleline(&mut cmdline).desired_width(260.0)).changed() {
                            let mut parts = cmdline.split_whitespace();
                            srv.command = parts.next().unwrap_or("").to_string();
                            srv.args = parts.map(|a| a.to_string()).collect();
                            changed = true;
                        }
                        if ui.small_button(icons::CLOSE).clicked() {
                            remove = Some(i);
                        }
                        ui.end_row();
                    }
                });
                if let Some(i) = remove {
                    self.cfg.lsp_servers.remove(i);
                    changed = true;
                }
                ui.horizontal(|ui| {
                    if ui.small_button("Add").clicked() {
                        self.cfg.lsp_servers.push(lsp::ServerDef {
                            extensions: vec!["ext".into()],
                            language_id: "plaintext".into(),
                            command: "server".into(),
                            args: vec![],
                        });
                        changed = true;
                    }
                    if ui.small_button("Reset defaults").clicked() {
                        self.cfg.lsp_servers = lsp::default_servers();
                        changed = true;
                    }
                });
                changed |= ui.checkbox(&mut self.cfg.format_json_on_save, "Format JSON on save").changed();
                ui.add_space(8.0);
                ui.heading("Appearance");
                ui.horizontal(|ui| {
                    for (t, label) in [
                        (Theme::System, "System"),
                        (Theme::Light, "Light"),
                        (Theme::Dark, "Dark"),
                        (Theme::Omarchy, "Omarchy"),
                    ] {
                        if ui.radio_value(&mut self.cfg.theme, t, label).clicked() {
                            theme::apply(ctx, self.cfg.theme);
                            changed = true;
                        }
                    }
                });
                if ui.checkbox(&mut self.cfg.show_hidden, "Show hidden files").changed() {
                    reload = true;
                    changed = true;
                }
            });
        if reload {
            self.refresh_all();
        }
        if changed {
            self.save_cfg();
        }
        self.settings_open = open;
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        // `command` is Ctrl on Windows/Linux and Cmd on macOS.
        let text_focused = ctx.memory(|m| m.focused().is_some());
        if text_focused || self.modal.is_some() {
            return;
        }
        let i = ctx.input(|i| i.clone());
        let cmd = i.modifiers.command;
        if i.key_pressed(egui::Key::Backspace) && !i.modifiers.any() {
            self.go_up();
        }
        if i.modifiers.alt && i.key_pressed(egui::Key::ArrowLeft) {
            self.go_back();
        }
        if i.modifiers.alt && i.key_pressed(egui::Key::ArrowRight) {
            self.go_forward();
        }
        if i.key_pressed(egui::Key::Enter) && !i.modifiers.alt {
            if let Some(e) = self.selected_entry() {
                self.open_entry(&e);
            }
        }
        if i.key_pressed(egui::Key::F5) {
            self.refresh_all();
        }
        if i.key_pressed(egui::Key::F2) {
            if let Some(e) = self.selected_entry() {
                self.modal = Some(Modal::Rename { path: e.path, name: e.name });
            }
        }
        if i.key_pressed(egui::Key::Delete) {
            if let Some(p) = self.selected.clone() {
                self.modal = Some(Modal::Delete { path: p });
            }
        }
        if cmd && i.key_pressed(egui::Key::C) {
            self.copy_selected(false);
        }
        if cmd && i.key_pressed(egui::Key::X) {
            self.copy_selected(true);
        }
        if cmd && i.key_pressed(egui::Key::V) {
            self.paste();
        }
        if cmd && i.modifiers.shift && i.key_pressed(egui::Key::N) {
            self.modal = Some(Modal::NewFolder { name: "New folder".into() });
        }
        if cmd && i.key_pressed(egui::Key::Comma) {
            self.settings_open = true;
        }
        if cmd && i.key_pressed(egui::Key::Z) {
            self.undo();
        }
        if i.modifiers.alt && i.key_pressed(egui::Key::Enter) {
            let p = self.selected.clone().unwrap_or_else(|| self.cwd.clone());
            self.show_properties(p);
        }
    }
}

impl eframe::App for ExplorerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.shortcuts(ctx);

        drag_preview(ctx);
        titlebar::show(ctx, "The Real Explorer");
        egui::TopBottomPanel::top("menu").show(ctx, |ui| self.menu_bar(ctx, ui));
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            self.toolbar(ui);
            ui.add_space(4.0);
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.label(&self.status);
        });
        if self.show_tree {
            egui::SidePanel::left("tree")
                .resizable(true)
                .default_width(260.0)
                .min_width(140.0)
                .show(ctx, |ui| self.tree_panel(ui));
        }
        let cwd = self.cwd.clone();
        let cfg = self.cfg.clone();
        let was_open = self.chat.open;
        self.chat.show(ctx, &cwd, &cfg);
        if was_open != self.chat.open {
            self.save_cfg();
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.selectable_label(self.body == Body::Explorer, format!("{} Explorer", icons::EXPLORER)).clicked() {
                    self.body = Body::Explorer;
                }
                let ide_label = if self.editor.docs.iter().any(|d| d.dirty) {
                    format!("{} IDE •", icons::IDE)
                } else {
                    format!("{} IDE", icons::IDE)
                };
                if ui.selectable_label(self.body == Body::Ide, ide_label).clicked() {
                    self.body = Body::Ide;
                }
            });
            ui.separator();
            match self.body {
                Body::Explorer => self.details_panel(ui),
                Body::Ide => {
                    let servers = self.cfg.lsp_servers.clone();
                    self.editor.show(ui, &servers, self.cfg.format_json_on_save);
                    if !self.editor.status.is_empty() {
                        self.status = std::mem::take(&mut self.editor.status);
                    }
                }
            }
        });

        self.modal_window(ctx);
        self.settings_window(ctx);
        self.properties_window(ctx);
        titlebar::resize_handles(ctx);
    }
}

/// Follow the cursor with the dragged item's name while a drag is in flight.
fn drag_preview(ctx: &egui::Context) {
    let Some(p) = egui::DragAndDrop::payload::<attach::DragPaths>(ctx) else { return };
    let Some(pos) = ctx.pointer_interact_pos() else { return };
    let text = match p.0.as_slice() {
        [one] => one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| one.display().to_string()),
        many => format!("{} items", many.len()),
    };
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("drag_preview")));
    let font = egui::FontId::proportional(13.0);
    let galley = painter.layout_no_wrap(text, font, egui::Color32::WHITE);
    let rect = egui::Rect::from_min_size(pos + egui::vec2(12.0, 12.0), galley.size() + egui::vec2(12.0, 6.0));
    painter.rect_filled(rect, 4.0, egui::Color32::from_black_alpha(200));
    painter.galley(rect.min + egui::vec2(6.0, 3.0), galley, egui::Color32::WHITE);
}

// ---------- helpers ----------

/// Drive roots on Windows (probes A: through Z:), `/`, home and `/Volumes`
/// on macOS, `/` and home elsewhere.
fn drive_roots() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        (b'A'..=b'Z')
            .map(|c| PathBuf::from(format!("{}:\\", c as char)))
            .filter(|p| p.exists())
            .collect()
    }
    #[cfg(target_os = "macos")]
    {
        let mut v = vec![PathBuf::from("/")];
        if let Some(h) = dirs::home_dir() {
            v.push(h);
        }
        v.push(PathBuf::from("/Volumes"));
        v
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut v = vec![PathBuf::from("/")];
        if let Some(h) = dirs::home_dir() {
            v.push(h);
        }
        v
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

/// `foo.txt` -> `foo - Copy.txt`, then `foo - Copy (2).txt`, until unused.
fn unique_name(p: &Path) -> PathBuf {
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let dir = p.parent().unwrap_or(Path::new("."));
    let mut n = 1;
    loop {
        let name = if n == 1 { format!("{stem} - Copy{ext}") } else { format!("{stem} - Copy ({n}){ext}") };
        let cand = dir.join(name);
        if !cand.exists() {
            return cand;
        }
        n += 1;
    }
}

fn copy_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        fs::create_dir_all(dst)?;
        for de in fs::read_dir(src)? {
            let de = de?;
            copy_path(&de.path(), &dst.join(de.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(src, dst).map(|_| ())
    }
}

/// Rename, falling back to copy+delete across volumes.
fn move_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    if fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    copy_path(src, dst)?;
    if src.is_dir() { fs::remove_dir_all(src) } else { fs::remove_file(src) }
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
