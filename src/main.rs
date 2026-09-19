//! rust-explorer: a Windows Explorer style file browser with a VS Code style
//! folder tree in the sidebar and an optional AI chat on the right.
//! Left pane is a lazily expanded directory tree, centre is a sortable details
//! list for the current directory. Menus cover file operations, settings hold
//! the chat backend (Claude Code CLI or any OpenAI-compatible endpoint).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod chat;
mod config;
mod icons;
mod theme;

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use config::{Backend, Config, Theme};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Rust Explorer",
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
    roots: Vec<PathBuf>,
    show_tree: bool,
    status: String,
    chat: chat::ChatPanel,
    settings_open: bool,
    /// Internal clipboard: paths and whether the paste should move them.
    clipboard: Option<(Vec<PathBuf>, bool)>,
    modal: Option<Modal>,
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
            roots: drive_roots(),
            show_tree: true,
            status: String::new(),
            settings_open: false,
            clipboard: None,
            modal: None,
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
        self.reload();
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

    fn open_entry(&mut self, e: &Entry) {
        if e.is_dir {
            self.navigate(e.path.clone());
        } else if let Err(err) = open::that_detached(&e.path) {
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
        let cwd = self.cwd.clone();
        #[cfg(windows)]
        let r = Command::new("wt.exe").arg("-d").arg(&cwd).spawn().or_else(|_| {
            Command::new("cmd").args(["/c", "start", "cmd", "/K"]).current_dir(&cwd).spawn()
        });
        #[cfg(target_os = "macos")]
        let r = Command::new("open").arg("-a").arg("Terminal").arg(&cwd).spawn();
        #[cfg(all(unix, not(target_os = "macos")))]
        let r = Command::new("x-terminal-emulator").current_dir(&cwd).spawn();
        if let Err(e) = r {
            self.status = format!("Terminal failed: {e}");
        }
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
                fs::rename(&path, &dst).map_err(|e| e.to_string())
            }
            Modal::Delete { path } => {
                if path.is_dir() {
                    fs::remove_dir_all(&path).map_err(|e| e.to_string())
                } else {
                    fs::remove_file(&path).map_err(|e| e.to_string())
                }
            }
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
            let resp = ui.add(egui::Label::new(job).sense(egui::Sense::click()).truncate());
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
        let mut ctx_action: Option<&'static str> = None;

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
                    if r.double_clicked() {
                        action = Some((i, true));
                    } else if r.clicked() {
                        action = Some((i, false));
                    } else if r.secondary_clicked() {
                        ctx_action = Some("select");
                        action = Some((i, false));
                    }
                    r.context_menu(|ui| {
                        for (label, key) in [
                            ("Open", "open"),
                            ("Copy", "copy"),
                            ("Cut", "cut"),
                            ("Rename", "rename"),
                            ("Delete", "delete"),
                            ("Copy path", "path"),
                        ] {
                            if ui.button(label).clicked() {
                                action = Some((i, false));
                                ctx_action = Some(key);
                                ui.close_menu();
                            }
                        }
                    });
                });
            });

        if let Some(k) = sort {
            self.toggle_sort(k);
        }
        if let Some((i, double)) = action {
            let e = entries[i].clone();
            self.selected = Some(e.path.clone());
            // Folders open on single left click (tree reveals them); files need a double.
            if ctx_action.is_none() && (double || e.is_dir) {
                self.open_entry(&e);
            }
            match ctx_action {
                Some("open") => self.open_entry(&e),
                Some("copy") => self.copy_selected(false),
                Some("cut") => self.copy_selected(true),
                Some("rename") => self.modal = Some(Modal::Rename { path: e.path, name: e.name }),
                Some("delete") => self.modal = Some(Modal::Delete { path: e.path }),
                Some("path") => ui.ctx().copy_text(e.path.display().to_string()),
                _ => {}
            }
        }
        // Right-click on empty space: paste / new.
        let bg = ui.interact(ui.max_rect(), ui.id().with("bg"), egui::Sense::click());
        bg.context_menu(|ui| {
            if ui.add_enabled(self.clipboard.is_some(), egui::Button::new("Paste")).clicked() {
                self.paste();
                ui.close_menu();
            }
            if ui.button("New folder").clicked() {
                self.modal = Some(Modal::NewFolder { name: "New folder".into() });
                ui.close_menu();
            }
            if ui.button("New file").clicked() {
                self.modal = Some(Modal::NewFile { name: "untitled.txt".into() });
                ui.close_menu();
            }
            if ui.button("Open terminal here").clicked() {
                self.open_terminal();
                ui.close_menu();
            }
        });
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
                        ui.label(format!("Permanently delete {}?", path.display()));
                        ui.small("This does not go to the recycle bin.");
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
        if i.key_pressed(egui::Key::Enter) {
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
    }
}

impl eframe::App for ExplorerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.shortcuts(ctx);

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
        egui::CentralPanel::default().show(ctx, |ui| self.details_panel(ui));

        self.modal_window(ctx);
        self.settings_window(ctx);
    }
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
