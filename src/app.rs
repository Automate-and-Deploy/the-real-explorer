//! The application shell: the Explorer tab (folder tree, details list, menus,
//! settings) and the frame that hosts the editor and assistant panels.
//!
//! This was `main.rs` until the crate grew a library target; the binary in
//! `src/main.rs` is now only `fn main`, and everything it used to hold lives
//! here so integration tests and examples can reach it through the library.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use eframe::egui;
use egui_extras::{Column, TableBuilder};

// Every module the shell touches, named so the paths below read the same as
// they did when this file was the crate root.
use crate::{attach, chat, editor, harness, harness_ui, icons, lsp, meta, platform, theme, titlebar, trash_ops};

use crate::config::{Backend, Config, ProjectSettings, Theme};

/// Window chrome differs by platform.
///
/// Windows and Linux get an undecorated window and the app draws the whole
/// title bar. macOS keeps its real title bar so the traffic lights, native
/// drag, edge resize and full screen all work, and hides only its background
/// and text so the app can draw underneath: an undecorated window there has
/// no traffic lights at all, and `drag_resize_window` is unsupported, so the
/// custom resize handles would be dead while still swallowing the pointer.
fn viewport() -> egui::ViewportBuilder {
    let b = egui::ViewportBuilder::default()
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([640.0, 400.0])
        .with_title("The Real Explorer");
    #[cfg(target_os = "macos")]
    {
        b.with_fullsize_content_view(true).with_titlebar_shown(false).with_title_shown(false)
    }
    #[cfg(not(target_os = "macos"))]
    {
        b.with_decorations(false)
    }
}

/// Builds the window and runs the event loop. The binary's `main` is a
/// one-line call to this; keeping the body in the library means a test or an
/// example can link the same code without duplicating the eframe setup.
pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: viewport(),
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
    /// Note from this folder's `.folder-meta.json`, empty when there is none.
    note: String,
    /// Tags from the same place, already lowercased.
    tags: Vec<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum SortKey {
    Name,
    Size,
    Modified,
    Type,
}

impl SortKey {
    fn as_str(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Size => "size",
            SortKey::Modified => "modified",
            SortKey::Type => "type",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "size" => SortKey::Size,
            "modified" => SortKey::Modified,
            "type" => SortKey::Type,
            _ => SortKey::Name,
        }
    }
}

/// Result of the Properties window's background scan, and the note editor.
struct Props {
    path: PathBuf,
    is_dir: bool,
    created: Option<SystemTime>,
    modified: Option<SystemTime>,
    /// (bytes, files, folders) once the scan finishes.
    totals: std::sync::Arc<std::sync::Mutex<Option<(u64, u64, u64)>>>,
    /// The folder whose sidecar holds this item's note, and the name it is
    /// keyed by. `None` for a drive root, which has no parent to hold one.
    home: Option<(PathBuf, String)>,
    /// Note as typed. Saved on Save, never per keystroke.
    note: String,
    /// Tags as typed, comma separated.
    tags: String,
    /// `(note, tags)` as loaded, for Revert and for spotting an unsaved edit.
    loaded: (String, String),
    /// The sidecar was written by a newer build: shown, never written over.
    read_only: bool,
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

/// What to do about a destination that already exists.
#[derive(Clone, Copy, PartialEq)]
enum Collision {
    Ask,
    Replace,
    Skip,
    KeepBoth,
}

/// A paste interrupted by an existing destination, waiting on the user.
struct PendingPaste {
    /// Sources still to process, including the one being asked about.
    remaining: Vec<PathBuf>,
    cut: bool,
    /// Destination that already exists.
    dst: PathBuf,
    /// Applies to every later collision in this paste when set.
    apply_to_all: bool,
    done: usize,
}

/// A pending text or confirm dialog.
enum Modal {
    NewFolder { name: String },
    NewFile { name: String },
    Rename { path: PathBuf, name: String },
    Delete { paths: Vec<PathBuf> },
    /// Delete an agent file or a whole skill folder via the harness window.
    HarnessDelete { item: harness::Item },
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
    harness: harness_ui::HarnessWindow,
    /// True once an in-app drag has been handed to the OS this gesture, so the
    /// hand-off happens once per drag rather than every frame.
    os_drag_handed_off: bool,
    /// Files with unsaved changes that stopped the window closing.
    exit_prompt: Option<Vec<PathBuf>>,
    /// Theme asked for by a project's `.code/settings.json`, applied on the
    /// next frame because loading happens where there is no egui context.
    pending_theme: Option<Theme>,
    /// In-flight agent and skill scan; see `refresh_harness`.
    harness_rx: Option<std::sync::mpsc::Receiver<harness::Catalog>>,
    /// Row the pointer went down on in the details list, so a drag that starts
    /// there carries that file even after the pointer has moved off the row.
    drag_row: Option<usize>,
    /// A paste waiting on a Replace / Skip / Keep both answer.
    pending_paste: Option<PendingPaste>,
    /// Every selected row, in addition to `selected`, which stays the anchor
    /// and the target of single-item actions.
    marked: BTreeSet<PathBuf>,
    /// Row index a shift-click ranges from.
    anchor_row: Option<usize>,
    /// Substring filter over the details list; empty shows everything.
    filter: String,
    /// True while the filter box should take the next keystroke.
    filter_focus: bool,
    /// Set when the selection moved by keyboard, so the list scrolls to it.
    scroll_to_selection: bool,
    /// Parsed notes sidecar for `meta_dir`. `reload()` runs on every filter
    /// keystroke, so the file is read once per folder and not once per key.
    meta: Option<meta::FolderMeta>,
    /// Folder `meta` was read from. `None` forces the next `reload()` to read
    /// again; that is how F5 and a save invalidate the cache.
    meta_dir: Option<PathBuf>,
    /// Why the sidecar could not be read, if it could not. Repeated on the
    /// status line rather than swallowed, because the alternative is a folder
    /// whose notes have silently vanished.
    meta_error: Option<String>,
}

impl ExplorerApp {
    fn new() -> Self {
        let (cfg, cfg_note) = Config::load();
        // Reopen where the user left off. A remembered folder that has since
        // gone (an unplugged drive) falls back rather than showing an error.
        let start = cfg
            .last_dir
            .clone()
            .filter(|p| p.is_dir())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"));
        let view = (
            SortKey::from_str(&cfg.sort_key),
            cfg.sort_asc,
            cfg.show_tree,
            cfg.compact,
            cfg.group_by_type,
            cfg.chat_open,
        );
        let mut app = Self {
            chat: chat::ChatPanel::new(view.5),
            cfg,
            cwd: start.clone(),
            entries: Vec::new(),
            history: vec![start.clone()],
            hist_pos: 0,
            address: start.display().to_string(),
            selected: None,
            sort_key: view.0,
            sort_asc: view.1,
            expanded: BTreeSet::new(),
            tree_children: HashMap::new(),
            tree_files: HashMap::new(),
            roots: drive_roots(),
            show_tree: view.2,
            status: String::new(),
            settings_open: false,
            clipboard: None,
            modal: None,
            undo: None,
            compact: view.3,
            group_by_type: view.4,
            props: None,
            menu_row: None,
            body: Body::Explorer,
            editor: editor::Editor::new(),
            harness: harness_ui::HarnessWindow::new(),
            os_drag_handed_off: false,
            pending_theme: None,
            harness_rx: None,
            drag_row: None,
            pending_paste: None,
            exit_prompt: None,
            marked: BTreeSet::new(),
            anchor_row: None,
            filter: String::new(),
            filter_focus: false,
            scroll_to_selection: false,
            meta: None,
            meta_dir: None,
            meta_error: None,
        };
        app.reload();
        app.expand_ancestors(&start);
        app.refresh_harness();
        app.apply_project_settings();
        if let Some(note) = cfg_note {
            app.status = note;
        }
        app
    }

    /// Read `<cwd>/.code/settings.json`, if present, over the global settings.
    /// The file is authoritative for the keys it names and silent about the rest.
    fn apply_project_settings(&mut self) {
        let Some(ps) = ProjectSettings::load(&self.cwd) else { return };
        let before = self.cfg.theme;
        ps.apply_to(&mut self.cfg);
        if self.cfg.theme != before {
            self.pending_theme = Some(self.cfg.theme);
        }
        self.status = format!("Applied {}", ProjectSettings::path(&self.cwd).display());
        self.reload();
    }

    /// Rescan agents and skills for the current folder and push the snapshot
    /// into the chat pickers.
    /// Start a scan on a worker thread and return immediately.
    ///
    /// This walks `.claude` in the project and in the home directory, and one
    /// slow entry there is enough to stop the window ever appearing if it runs
    /// on the main thread: on a Mac whose `~/.claude/skills` holds symlinks
    /// onto an external volume, the scan blocked in the kernel during app
    /// creation and the app launched to nothing. A network share, a stale
    /// mount or an offloaded cloud folder would do the same anywhere.
    fn refresh_harness(&mut self) {
        // One at a time: the harness window asks for a refresh on every frame
        // until a catalog arrives, so without this a slow scan spawns a thread
        // per frame against the very filesystem that is already slow.
        if self.harness_rx.is_some() {
            return;
        }
        let cwd = self.cwd.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(harness::scan(&cwd));
        });
        self.harness_rx = Some(rx);
    }

    /// Take a finished scan, if one has arrived. Called once per frame.
    fn poll_harness(&mut self) {
        let Some(rx) = &self.harness_rx else { return };
        let Ok(cat) = rx.try_recv() else { return };
        self.harness_rx = None;
        self.chat.agents = cat.agents.iter().map(|a| (a.name.clone(), a.scope.badge(), a.description.clone())).collect();
        self.chat.skills = cat.skills.iter().map(|s| (s.name.clone(), s.description.clone())).collect();
        if let Some(a) = &self.chat.agent {
            if !cat.agents.iter().any(|x| &x.name == a) {
                self.chat.agent = None;
            }
        }
        self.harness.catalog = Some(cat);
    }

    /// Record the view so the next run opens the same way.
    fn save_view(&mut self) {
        self.cfg.last_dir = Some(self.cwd.clone());
        self.cfg.sort_key = self.sort_key.as_str().into();
        self.cfg.sort_asc = self.sort_asc;
        self.cfg.group_by_type = self.group_by_type;
        self.cfg.compact = self.compact;
        self.cfg.show_tree = self.show_tree;
        self.save_cfg();
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
        self.refresh_harness();
        self.apply_project_settings();
        let cwd = self.cwd.clone();
        self.expand_ancestors(&cwd);
        self.save_view();
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

    /// Read the notes sidecar for the current folder unless it is already
    /// cached. Called from `reload()`, which runs on every filter keystroke.
    fn load_meta(&mut self) {
        if self.meta_dir.as_deref() == Some(self.cwd.as_path()) {
            return;
        }
        match meta::load(&self.cwd) {
            Ok(m) => {
                self.meta = m;
                self.meta_error = None;
            }
            Err(e) => {
                self.meta = None;
                self.meta_error = Some(e);
            }
        }
        self.meta_dir = Some(self.cwd.clone());
    }

    /// Forget the cached sidecar, so the next `reload()` re-reads it.
    fn invalidate_meta(&mut self) {
        self.meta_dir = None;
    }

    /// True when this folder has at least one annotated entry, which is what
    /// decides whether the details list shows a Note column at all. Read from
    /// the sidecar rather than from `entries` so the column does not appear
    /// and disappear as the filter narrows the list.
    fn has_notes(&self) -> bool {
        self.meta
            .as_ref()
            .map(|m| m.items.values().any(|i| !i.note.is_empty() || !i.tags.is_empty()))
            .unwrap_or(false)
    }

    /// Re-read the current directory into `entries` and sort.
    fn reload(&mut self) {
        self.load_meta();
        self.entries.clear();
        match fs::read_dir(&self.cwd) {
            Ok(rd) => {
                for de in rd.flatten() {
                    let name = de.file_name().to_string_lossy().into_owned();
                    if !self.cfg.show_hidden && is_hidden(&name, &de.path()) {
                        continue;
                    }
                    let md = de.metadata().ok();
                    let item = self.meta.as_ref().and_then(|m| m.items.get(&name));
                    self.entries.push(Entry {
                        name,
                        path: de.path(),
                        is_dir: md.as_ref().map(|m| m.is_dir()).unwrap_or(false),
                        size: md.as_ref().map(|m| m.len()).unwrap_or(0),
                        modified: md.and_then(|m| m.modified().ok()),
                        note: item.map(|i| i.note.clone()).unwrap_or_default(),
                        tags: item.map(|i| i.tags.clone()).unwrap_or_default(),
                    });
                }
                if !self.filter.is_empty() {
                    // Name, note or any tag: typing "tax" finds both the file
                    // called tax and the one you annotated with it. Lowercase
                    // the needle once here rather than once per row.
                    let needle = self.filter.to_lowercase();
                    self.entries.retain(|e| meta::matches(&needle, &e.name, &e.note, &e.tags));
                }
                self.status = match &self.meta_error {
                    Some(e) => format!("{} items — {e}", self.entries.len()),
                    None => format!("{} items", self.entries.len()),
                };
            }
            Err(e) => self.status = format!("Cannot read {}: {e}", self.cwd.display()),
        }
        let present: BTreeSet<PathBuf> = self.entries.iter().map(|e| e.path.clone()).collect();
        self.marked.retain(|p| present.contains(p));
        if self.selected.as_ref().map(|p| !present.contains(p)).unwrap_or(false) {
            self.selected = None;
        }
        self.anchor_row = None;
        self.sort_entries();
    }

    /// Reload the list and drop the tree cache so new or renamed folders show.
    /// This is F5, and F5 is what the user presses after editing a sidecar by
    /// hand, so the notes cache goes too.
    fn refresh_all(&mut self) {
        self.tree_children.clear();
        self.tree_files.clear();
        self.invalidate_meta();
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
        self.save_view();
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
        // A refused open (too large, unreadable) leaves the body where it
        // was; switching to an empty editor made the refusal look like a hang.
        if self.editor.open(path, &root, &self.cfg.lsp_servers) {
            self.body = Body::Ide;
        } else {
            // The editor wrote its reason to its own status line, which only
            // the IDE tab shows; repeat it here so the refusal is visible.
            self.status = self.editor.status.clone();
        }
    }

    fn open_with_system(&mut self, path: &Path) {
        if let Err(err) = open::that_detached(path) {
            self.status = format!("Open failed: {err}");
        }
    }

    // ---------- file operations ----------

    /// Every path the next action applies to: the marked set when there is
    /// one, otherwise the single selection.
    fn action_paths(&self) -> Vec<PathBuf> {
        if !self.marked.is_empty() {
            self.marked.iter().cloned().collect()
        } else {
            self.selected.iter().cloned().collect()
        }
    }

    fn select_only(&mut self, i: usize) {
        if let Some(e) = self.entries.get(i) {
            self.selected = Some(e.path.clone());
            self.marked.clear();
            self.anchor_row = Some(i);
        }
    }

    /// Extend the selection from the anchor to `i`, as shift-click does.
    fn select_range_to(&mut self, i: usize) {
        let from = self.anchor_row.unwrap_or(i);
        let (lo, hi) = if from <= i { (from, i) } else { (i, from) };
        self.marked = self.entries[lo..=hi.min(self.entries.len().saturating_sub(1))]
            .iter()
            .map(|e| e.path.clone())
            .collect();
        self.selected = self.entries.get(i).map(|e| e.path.clone());
    }

    fn toggle_mark(&mut self, i: usize) {
        let Some(e) = self.entries.get(i) else { return };
        if self.marked.is_empty() {
            if let Some(sel) = self.selected.clone() {
                self.marked.insert(sel);
            }
        }
        if !self.marked.remove(&e.path) {
            self.marked.insert(e.path.clone());
        }
        self.selected = Some(e.path.clone());
        self.anchor_row = Some(i);
    }

    fn selected_row(&self) -> Option<usize> {
        let sel = self.selected.as_ref()?;
        self.entries.iter().position(|e| &e.path == sel)
    }

    /// Move the selection by `delta` rows and keep it on screen.
    fn move_selection(&mut self, delta: isize, extend: bool) {
        if self.entries.is_empty() {
            return;
        }
        let cur = self.selected_row().map(|i| i as isize).unwrap_or(-1);
        let next = (cur + delta).clamp(0, self.entries.len() as isize - 1) as usize;
        if extend {
            self.select_range_to(next);
        } else {
            self.select_only(next);
        }
        self.scroll_to_selection = true;
    }

    fn selected_entry(&self) -> Option<Entry> {
        let sel = self.selected.as_ref()?;
        self.entries.iter().find(|e| &e.path == sel).cloned()
    }

    fn copy_selected(&mut self, cut: bool) {
        let paths = self.action_paths();
        if paths.is_empty() {
            return;
        }
        let n = paths.len();
        self.clipboard = Some((paths, cut));
        self.status = format!("{} {n} item(s)", if cut { "Cut" } else { "Copied" });
    }

    fn paste(&mut self) {
        let Some((paths, cut)) = self.clipboard.clone() else { return };
        self.paste_run(paths, cut, Collision::Ask, false, 0);
    }

    /// Paste `remaining` into the current folder, stopping at the first
    /// destination that already exists unless `policy` says what to do.
    ///
    /// Overwriting was silent before: pasting into a folder that already held
    /// a file of the same name replaced it with no prompt and no undo. Only a
    /// paste back into the source folder was handled, by renaming a copy.
    fn paste_run(&mut self, remaining: Vec<PathBuf>, cut: bool, policy: Collision, sticky: bool, done: usize) {
        let mut done = done;
        let mut queue = remaining;
        let mut policy = policy;
        while let Some(src) = queue.first().cloned() {
            let Some(name) = src.file_name() else {
                queue.remove(0);
                continue;
            };
            let mut dst = self.cwd.join(name);
            // Pasting into the source folder is a duplicate, not a collision.
            let mut duplicate = false;
            if dst == src {
                dst = unique_name(&dst);
                duplicate = true;
            } else if dst.exists() {
                match policy {
                    Collision::Ask => {
                        self.pending_paste = Some(PendingPaste {
                            remaining: queue,
                            cut,
                            dst,
                            apply_to_all: false,
                            done,
                        });
                        return;
                    }
                    Collision::Skip => {
                        queue.remove(0);
                        if !sticky {
                            policy = Collision::Ask;
                        }
                        continue;
                    }
                    Collision::KeepBoth => dst = unique_name(&dst),
                    Collision::Replace => {}
                }
                if !sticky {
                    policy = Collision::Ask;
                }
            }
            queue.remove(0);
            let r = if cut { move_path(&src, &dst) } else { copy_path(&src, &dst) };
            if let Err(e) = r {
                self.status = format!("Paste failed: {e}");
                self.refresh_all();
                return;
            }
            if duplicate {
                self.meta_copy(&src, &dst);
            }
            done += 1;
        }
        if cut {
            self.clipboard = None;
        }
        self.refresh_all();
        self.status = format!("Pasted {done} item(s)");
    }

    /// The Replace / Skip / Keep both prompt for one colliding destination.
    /// Refuse a close that would drop unsaved editor buffers, and ask.
    ///
    /// The window close button, Alt+F4 and File > Exit all end here, so this
    /// is the only place the guard has to live. Without it every unsaved
    /// buffer went out with the process and nothing was said.
    fn exit_guard(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) && self.exit_prompt.is_none() {
            let dirty = self.editor.dirty_paths();
            if !dirty.is_empty() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.exit_prompt = Some(dirty);
            }
        }
        let Some(dirty) = self.exit_prompt.clone() else { return };
        let mut decision: Option<&'static str> = None;
        egui::Window::new("Unsaved changes")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!("{} file(s) have unsaved changes:", dirty.len()));
                for p in dirty.iter().take(8) {
                    ui.small(p.display().to_string());
                }
                if dirty.len() > 8 {
                    ui.small(format!("and {} more", dirty.len() - 8));
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Save all and exit").clicked() {
                        decision = Some("save");
                    }
                    if ui.button("Discard and exit").clicked() {
                        decision = Some("discard");
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some("cancel");
                    }
                });
            });
        match decision {
            Some("save") => {
                self.editor.save_all(self.cfg.format_json_on_save);
                let still = self.editor.dirty_paths();
                if still.is_empty() {
                    self.exit_prompt = None;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                } else {
                    // A failed write must not take the file down with the app.
                    self.exit_prompt = Some(still);
                    self.status = "Some files could not be saved; exit cancelled".into();
                }
            }
            Some("discard") => {
                self.exit_prompt = None;
                self.editor.discard_all_dirty();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Some("cancel") => self.exit_prompt = None,
            _ => {}
        }
    }

    fn paste_conflict_window(&mut self, ctx: &egui::Context) {
        let Some(p) = &mut self.pending_paste else { return };
        let dst = p.dst.clone();
        let cut = p.cut;
        let mut choice: Option<Collision> = None;
        let mut cancel = false;
        let mut all = p.apply_to_all;
        egui::Window::new("File already exists")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!("{} already exists here.", dst.display()));
                if let Ok(md) = fs::metadata(&dst) {
                    ui.small(format!("existing: {}, modified {}", human_size(md.len()), md.modified().map(fmt_time).unwrap_or_default()));
                }
                ui.checkbox(&mut all, "Apply to the rest of this paste");
                ui.horizontal(|ui| {
                    if ui.button("Replace").clicked() {
                        choice = Some(Collision::Replace);
                    }
                    if ui.button("Keep both").clicked() {
                        choice = Some(Collision::KeepBoth);
                    }
                    if ui.button("Skip").clicked() {
                        choice = Some(Collision::Skip);
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if let Some(p) = &mut self.pending_paste {
            p.apply_to_all = all;
        }
        if cancel {
            self.pending_paste = None;
            self.status = "Paste cancelled".into();
            return;
        }
        if let Some(c) = choice {
            let p = self.pending_paste.take().expect("checked above");
            self.paste_run(p.remaining, cut, c, p.apply_to_all, p.done);
        }
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
            Some(Undo::Rename(cur, prev)) => match fs::rename(&cur, &prev) {
                Ok(()) => {
                    // Undoing a rename is a rename, and the note follows it
                    // back the same way it followed it out.
                    self.meta_rename(&cur, &prev);
                    Ok(format!("Restored {}", prev.display()))
                }
                Err(e) => Err(e.to_string()),
            },
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
        // The note lives in the parent folder's sidecar under this name, so
        // the window reads it from there rather than from the cached listing:
        // Properties is reachable from the tree and from the address bar too,
        // where the item is not a row in the current folder at all.
        let home = path
            .parent()
            .zip(path.file_name())
            .map(|(d, n)| (d.to_path_buf(), n.to_string_lossy().into_owned()));
        let mut note = String::new();
        let mut tags = String::new();
        let mut read_only = false;
        if let Some((dir, key)) = &home {
            match meta::load(dir) {
                Ok(Some(m)) => {
                    read_only = m.is_read_only();
                    if let Some(i) = m.items.get(key) {
                        note = i.note.clone();
                        tags = meta::tags_line(&i.tags);
                    }
                }
                Ok(None) => {}
                Err(e) => self.status = e,
            }
        }
        self.props = Some(Props {
            path,
            is_dir,
            created: md.as_ref().and_then(|m| m.created().ok()),
            modified: md.and_then(|m| m.modified().ok()),
            totals,
            home,
            loaded: (note.clone(), tags.clone()),
            note,
            tags,
            read_only,
        });
    }

    /// Load the sidecar for `dir`, let `edit` change it, and write it back
    /// when `edit` reports a change.
    ///
    /// Entries whose name is no longer in the folder are dropped at the same
    /// time, but only when the folder could actually be listed: pruning
    /// against a transient read error would delete every note in it.
    ///
    /// `create` is false for the rename, delete and paste hooks. Renaming a
    /// file in a folder nobody has annotated must not conjure a sidecar into
    /// it, and it must not cost a write either.
    fn edit_meta(
        &mut self,
        dir: &Path,
        create: bool,
        edit: impl FnOnce(&mut meta::FolderMeta) -> bool,
    ) -> Result<bool, String> {
        let mut m = match meta::load(dir)? {
            Some(m) => m,
            None if create => meta::FolderMeta::default(),
            None => return Ok(false),
        };
        if !edit(&mut m) {
            return Ok(false);
        }
        if let Ok(rd) = fs::read_dir(dir) {
            let present: HashSet<String> =
                rd.flatten().map(|de| de.file_name().to_string_lossy().into_owned()).collect();
            m.prune(&present);
        }
        meta::save(dir, &m)?;
        self.invalidate_meta();
        Ok(true)
    }

    /// Move an entry's note with the file after a rename in place.
    ///
    /// A rename that lands in a different folder is a move, and a note stays
    /// in the folder that holds it, so nothing is carried across.
    fn meta_rename(&mut self, from: &Path, to: &Path) {
        let (Some(dir), Some(old), Some(new)) = (from.parent(), from.file_name(), to.file_name()) else {
            return;
        };
        if to.parent() != Some(dir) {
            return;
        }
        let (dir, old, new) = (
            dir.to_path_buf(),
            old.to_string_lossy().into_owned(),
            new.to_string_lossy().into_owned(),
        );
        if let Err(e) = self.edit_meta(&dir, false, |m| m.rename_entry(&old, &new)) {
            self.status = format!("Renamed, but the note did not follow: {e}");
        }
    }

    /// Give a duplicate the original's note, after a paste into the folder the
    /// item already lives in.
    fn meta_copy(&mut self, from: &Path, to: &Path) {
        let (Some(dir), Some(old), Some(new)) = (from.parent(), from.file_name(), to.file_name()) else {
            return;
        };
        if to.parent() != Some(dir) {
            return;
        }
        let (dir, old, new) = (
            dir.to_path_buf(),
            old.to_string_lossy().into_owned(),
            new.to_string_lossy().into_owned(),
        );
        if let Err(e) = self.edit_meta(&dir, false, |m| m.copy_entry(&old, &new)) {
            self.status = format!("Copied, but the note did not: {e}");
        }
    }

    /// Drop the entries for items that have gone to the recycle bin, so a
    /// sidecar does not grow for ever and a restore does not resurrect a note
    /// onto whatever now has that name.
    fn meta_removed(&mut self, paths: &[PathBuf]) {
        let mut by_dir: HashMap<PathBuf, Vec<String>> = HashMap::new();
        for p in paths {
            if let (Some(dir), Some(name)) = (p.parent(), p.file_name()) {
                by_dir
                    .entry(dir.to_path_buf())
                    .or_default()
                    .push(name.to_string_lossy().into_owned());
            }
        }
        for (dir, names) in by_dir {
            let r = self.edit_meta(&dir, false, |m| {
                let mut changed = false;
                for n in &names {
                    changed |= m.remove_entry(n);
                }
                changed
            });
            if let Err(e) = r {
                self.status = format!("Deleted, but the notes file was not updated: {e}");
            }
        }
    }

    /// Write the Properties window's note and tags into the sidecar.
    ///
    /// A failure leaves the typed text exactly where it is: the window stays
    /// open with the note still in it and the status line says why, because
    /// the folder being unwritable is not the user's mistake to lose work to.
    fn save_item_meta(&mut self, p: &mut Props) {
        let Some((dir, key)) = p.home.clone() else { return };
        let note = meta::normalize_note(&p.note);
        let tags = meta::normalize_tags(&p.tags);
        let (n, t) = (note.clone(), tags.clone());
        let k = key.clone();
        let r = self.edit_meta(&dir, true, move |m| {
            let drop_it = {
                let item = m.items.entry(k.clone()).or_default();
                item.note = n;
                item.tags = t;
                item.updated = meta::now();
                // Clearing both fields removes the entry rather than leaving
                // an empty record, unless another writer's keys are on it.
                item.is_empty()
            };
            if drop_it {
                m.items.remove(&k);
            }
            true
        });
        match r {
            Ok(_) => {
                p.tags = meta::tags_line(&tags);
                p.note = note;
                p.loaded = (p.note.clone(), p.tags.clone());
                self.reload();
                self.status = format!("Note saved in {}", meta::path(&dir).display());
            }
            Err(e) => self.status = format!("Note not saved: {e}"),
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
                // fs::rename replaces the destination without a word. New file
                // already refuses an existing name; rename has to as well.
                if dst != path && dst.exists() {
                    Err(format!("{} already exists", dst.display()))
                } else {
                    match fs::rename(&path, &dst) {
                        Ok(()) => {
                            self.meta_rename(&path, &dst);
                            self.undo = Some(Undo::Rename(dst, path));
                            Ok(())
                        }
                        Err(e) => Err(e.to_string()),
                    }
                }
            }
            Modal::Delete { paths } => {
                let n = paths.len();
                trash_ops::delete_many_to_trash(&paths).map(|()| {
                    self.meta_removed(&paths);
                    // Undo restores the first one; the bin holds the rest.
                    if let Some(first) = paths.into_iter().next() {
                        self.undo = Some(Undo::Delete(first));
                    }
                    self.status = format!("Moved {n} item(s) to the {}", trash_ops::bin_name());
                })
            }
            Modal::HarnessDelete { item } => harness::delete(&item).map(|()| {
                self.undo = Some(Undo::Delete(harness::delete_target(&item)));
                self.refresh_harness();
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
                if ui.button("New file\tCtrl+N").clicked() {
                    self.modal = Some(Modal::NewFile { name: "untitled.txt".into() });
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Open terminal here").clicked() {
                    self.open_terminal();
                    ui.close_menu();
                }
                if ui.button("Save project settings (.code)").clicked() {
                    match ProjectSettings::save(&self.cwd, &self.cfg) {
                        Ok(p) => self.status = format!("Wrote {}", p.display()),
                        Err(e) => self.status = format!("Failed: {e}"),
                    }
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
                ui.small("Text editing undo is Ctrl+Z inside the editor.");
                if ui.add_enabled(self.body == Body::Ide, egui::Button::new("Format document	Ctrl+Shift+F")).clicked() {
                    // format_current picks JSON or the language server and
                    // reports through the editor's own status line.
                    self.editor.format_current();
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
                        self.modal = Some(Modal::Delete { paths: vec![p] });
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
                    self.save_view();
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
                if ui.button("Agents, skills and hooks").clicked() {
                    self.harness.open = true;
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
            ui.menu_button("Agents", |ui| {
                if ui.button("Open agents window").clicked() {
                    self.harness.open = true;
                    ui.close_menu();
                }
                if ui.button("Refresh catalog").clicked() {
                    self.refresh_harness();
                    ui.close_menu();
                }
                ui.separator();
                ui.label(egui::RichText::new("Assistant agent").small());
                if ui.radio(self.chat.agent.is_none(), "(default)").clicked() {
                    self.chat.agent = None;
                    self.chat.new_session();
                    ui.close_menu();
                }
                // Snapshot first: the loop needs &self.chat.agents while assigning to it.
                let agents: Vec<(String, &'static str)> =
                    self.chat.agents.iter().map(|(n, b, _)| (n.clone(), *b)).collect();
                egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                    for (name, badge) in agents {
                        let on = self.chat.agent.as_deref() == Some(name.as_str());
                        if ui.radio(on, format!("{badge} {name}")).clicked() {
                            self.chat.agent = Some(name);
                            self.chat.new_session();
                            ui.close_menu();
                        }
                    }
                });
            });
        });
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.hist_pos > 0, egui::Button::new(icons::BACK))
                .on_hover_text("Back (Alt+Left)")
                .clicked()
            {
                self.go_back();
            }
            if ui
                .add_enabled(self.hist_pos + 1 < self.history.len(), egui::Button::new(icons::FORWARD))
                .on_hover_text("Forward (Alt+Right)")
                .clicked()
            {
                self.go_forward();
            }
            if ui
                .add_enabled(self.cwd.parent().is_some(), egui::Button::new(icons::UP))
                .on_hover_text("Up one folder (Backspace)")
                .clicked()
            {
                self.go_up();
            }
            if ui.button(icons::REFRESH).on_hover_text("Refresh (F5)").clicked() {
                self.refresh_all();
            }
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.address)
                    .desired_width(ui.available_width() - 320.0),
            );
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let p = PathBuf::from(self.address.trim());
                self.navigate(p);
            }
            let filter = ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .desired_width(150.0)
                    .hint_text("Filter (Ctrl+F)"),
            );
            if self.filter_focus {
                filter.request_focus();
                self.filter_focus = false;
            }
            if filter.changed() {
                self.reload();
            }
            if ui.selectable_label(self.chat.open, format!("{} Chat", icons::CHAT)).clicked() {
                self.chat.open = !self.chat.open;
                self.save_cfg();
            }
            if ui.button(icons::SETTINGS).on_hover_text("Settings").clicked() {
                self.settings_open = true;
            }
            if ui.button(icons::HARNESS).on_hover_text("Agents, skills and hooks").clicked() {
                self.harness.open = true;
            }
        });
    }

    /// Clickable path segments under the toolbar.
    ///
    /// The address bar is a text field, which is right for typing and wrong
    /// for jumping two folders up, which is the common case.
    fn breadcrumbs(&mut self, ui: &mut egui::Ui) {
        let mut jump: Option<PathBuf> = None;
        egui::ScrollArea::horizontal().id_salt("crumbs").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let mut acc = PathBuf::new();
                let parts: Vec<_> = self.cwd.components().collect();
                for (i, c) in parts.iter().enumerate() {
                    acc.push(c.as_os_str());
                    let label = match c {
                        std::path::Component::RootDir => continue,
                        std::path::Component::Prefix(p) => p.as_os_str().to_string_lossy().into_owned(),
                        other => other.as_os_str().to_string_lossy().into_owned(),
                    };
                    if i > 0 {
                        ui.label(egui::RichText::new("›").weak());
                    }
                    let last = i + 1 == parts.len();
                    let text = if last {
                        egui::RichText::new(label).strong()
                    } else {
                        egui::RichText::new(label)
                    };
                    if ui.add(egui::Label::new(text).sense(egui::Sense::click())).clicked() {
                        jump = Some(acc.clone());
                    }
                }
            });
        });
        if let Some(p) = jump {
            self.navigate(p);
        }
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
            let resp = ui.add(egui::Label::new(job).sense(egui::Sense::click()).truncate());
            attach::drag_source(&resp, || vec![dir.to_path_buf()]);
            if resp.clicked() {
                self.expanded.insert(dir.to_path_buf());
                self.navigate(dir.to_path_buf());
            }
            // The list has a context menu and the tree did not, which reads as
            // broken rather than deliberate one pane over.
            let d = dir.to_path_buf();
            resp.context_menu(|ui| {
                if ui.button("Open").clicked() {
                    self.navigate(d.clone());
                    ui.close_menu();
                }
                if ui.button("Open in Terminal").clicked() {
                    let keep = self.cwd.clone();
                    self.cwd = d.clone();
                    self.open_terminal();
                    self.cwd = keep;
                    ui.close_menu();
                }
                if ui.button(format!("Open in {}", platform::file_manager_name())).clicked() {
                    self.reveal(&d);
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Copy path").clicked() {
                    ui.ctx().copy_text(d.display().to_string());
                    ui.close_menu();
                }
                if ui.button("Rename").clicked() {
                    let name = d.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    self.modal = Some(Modal::Rename { path: d.clone(), name });
                    ui.close_menu();
                }
                if ui.button("Delete").clicked() {
                    self.modal = Some(Modal::Delete { paths: vec![d.clone()] });
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("New folder here").clicked() {
                    self.navigate(d.clone());
                    self.modal = Some(Modal::NewFolder { name: "New folder".into() });
                    ui.close_menu();
                }
                if ui.button("Properties").clicked() {
                    self.show_properties(d.clone());
                    ui.close_menu();
                }
            });
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
                        let r = ui.add(egui::Label::new(job).sense(egui::Sense::click()).truncate());
                        attach::drag_source(&r, || vec![file.clone()]);
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

        // A folder nobody has annotated looks exactly as it did before.
        let show_note = self.has_notes();
        let mut table = TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::remainder().at_least(200.0).clip(true))
            .column(Column::initial(90.0).at_least(60.0))
            .column(Column::initial(150.0).at_least(100.0))
            .column(Column::initial(90.0).at_least(60.0));
        if show_note {
            table = table.column(Column::initial(200.0).at_least(80.0).clip(true));
        }
        table
            .header(22.0, |mut h| {
                h.col(|ui| if header(ui, "Name", SortKey::Name, self) { sort = Some(SortKey::Name) });
                h.col(|ui| if header(ui, "Size", SortKey::Size, self) { sort = Some(SortKey::Size) });
                h.col(|ui| if header(ui, "Modified", SortKey::Modified, self) { sort = Some(SortKey::Modified) });
                h.col(|ui| { ui.strong("Type"); });
                if show_note {
                    h.col(|ui| { ui.strong("Note"); });
                }
            })
            .body(|body| {
                body.rows(if self.compact { 18.0 } else { 24.0 }, entries.len(), |mut row| {
                    let i = row.index();
                    let e = &entries[i];
                    row.set_selected(self.selected.as_ref() == Some(&e.path) || self.marked.contains(&e.path));
                    row.col(|ui| {
                        let (glyph, color) = if e.is_dir { icons::folder(false) } else { icons::file(&e.name) };
                        let color = icons::tint(color, ui.visuals().dark_mode);
                        let font = egui::TextStyle::Body.resolve(ui.style());
                        let mut job = egui::text::LayoutJob::default();
                        job.append(&format!("{glyph} "), 0.0, egui::TextFormat { font_id: font.clone(), color, ..Default::default() });
                        job.append(&e.name, 0.0, egui::TextFormat { font_id: font, color: ui.visuals().text_color(), ..Default::default() });
                        let resp = ui.add(egui::Label::new(job).truncate());
                        // `contains_pointer`, not `hovered`: the catch-all
                        // interact registered over the whole pane after the
                        // rows is the topmost widget, so nothing inside a row
                        // is ever the hovered one.
                        if !e.note.is_empty() && resp.contains_pointer() {
                            egui::show_tooltip_at_pointer(
                                ui.ctx(),
                                ui.layer_id(),
                                egui::Id::new("note-tooltip"),
                                |ui| ui.label(&e.note),
                            );
                        }
                    });
                    row.col(|ui| {
                        if e.is_dir {
                            // Blank read as "zero bytes"; the real number needs
                            // a recursive walk, which Properties does.
                            ui.label(egui::RichText::new("folder").weak().small());
                        } else {
                            ui.label(human_size(e.size));
                        }
                    });
                    row.col(|ui| {
                        ui.label(e.modified.map(fmt_time).unwrap_or_default());
                    });
                    row.col(|ui| {
                        ui.label(if e.is_dir { "Folder".to_string() } else { ext_type(&e.name) });
                    });
                    if show_note {
                        row.col(|ui| {
                            if e.note.is_empty() && e.tags.is_empty() {
                                return;
                            }
                            let font = egui::TextStyle::Body.resolve(ui.style());
                            let mut job = egui::text::LayoutJob::default();
                            // A multi-line note would push the row past its
                            // fixed height, so the cell shows it on one line
                            // and the hover on the name shows the whole thing.
                            let one_line = e.note.replace(['\r', '\n'], " ");
                            job.append(&one_line, 0.0, egui::TextFormat { font_id: font.clone(), color: ui.visuals().text_color(), ..Default::default() });
                            if !e.tags.is_empty() {
                                let tags: Vec<String> = e.tags.iter().map(|t| format!("#{t}")).collect();
                                job.append(
                                    &format!(" {}", tags.join(" ")),
                                    0.0,
                                    egui::TextFormat { font_id: font, color: ui.visuals().weak_text_color(), ..Default::default() },
                                );
                            }
                            ui.add(egui::Label::new(job).truncate());
                        });
                    }
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
        // Dragging a row to the assistant panel. The catch-all keeps click
        // sense, and the drag starts by hand past egui's click distance, for
        // the same reason the tree labels do: `click_and_drag` would postpone
        // every click decision and make single clicks unreliable.
        let (pressed, down) = ui.input(|i| (i.pointer.primary_pressed(), i.pointer.primary_down()));
        if pressed && hovered_row.is_some() {
            self.drag_row = hovered_row;
        }
        if !down {
            self.drag_row = None;
        }
        if let Some(i) = self.drag_row.filter(|i| *i < entries.len()) {
            let path = entries[i].path.clone();
            attach::drag_source(&bg, || vec![path]);
        }
        if bg.secondary_clicked() {
            self.menu_row = hovered_row;
            if let Some(i) = hovered_row {
                self.selected = Some(entries[i].path.clone());
            }
        }
        if let Some((i, double)) = action {
            let e = entries[i].clone();
            let (ctrl, shift) = ui.input(|i| (i.modifiers.command, i.modifiers.shift));
            if ctrl {
                self.toggle_mark(i);
            } else if shift {
                self.select_range_to(i);
            } else {
                self.select_only(i);
                // Folders open on single left click (the tree reveals them);
                // files need a double click.
                if double || e.is_dir {
                    self.open_entry(&e);
                }
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
                    ("Open with...", "openwith"),
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
                Some("openwith") => {
                    if let Err(err) = platform::open_with(&e.path) {
                        self.status = format!("Open with failed: {err}");
                    }
                }
                Some("reveal") => self.reveal(&e.path),
                Some("cut") => self.copy_selected(true),
                Some("copy") => self.copy_selected(false),
                Some("path") => ui.ctx().copy_text(e.path.display().to_string()),
                Some("rename") => self.modal = Some(Modal::Rename { path: e.path, name: e.name }),
                Some("delete") => {
                    // A right-click on a marked row deletes the whole selection.
                    let mut paths = self.action_paths();
                    if !paths.contains(&e.path) {
                        paths = vec![e.path];
                    }
                    self.modal = Some(Modal::Delete { paths });
                }
                Some("props") => self.show_properties(e.path),
                _ => {}
            }
            return;
        }
        bg.context_menu(|ui| {
            ui.menu_button("View", |ui| {
                if ui.checkbox(&mut self.compact, "Compact rows").clicked() {
                    self.save_view();
                    ui.close_menu();
                }
                if ui.checkbox(&mut self.show_tree, "Folder tree").clicked() {
                    self.save_view();
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
                    self.save_view();
                    ui.close_menu();
                }
                if ui.radio(self.group_by_type, "Type").clicked() {
                    self.group_by_type = true;
                    self.sort_entries();
                    self.save_view();
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

    /// The Properties window, which also edits the note and tags.
    ///
    /// `Props` is taken out of `self` for the duration: the text fields need
    /// `&mut` on it while the closure runs, and Save needs `&mut self`, which
    /// the closure cannot have. Both are applied after the closure has
    /// returned, the same way the paste-conflict and exit-guard windows defer
    /// their decisions.
    fn properties_window(&mut self, ctx: &egui::Context) {
        let Some(mut p) = self.props.take() else { return };
        let mut save = false;
        let mut revert = false;
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
                ui.separator();
                match &p.home {
                    Some((dir, _)) => {
                        let store = meta::path(dir);
                        ui.label("Note");
                        ui.add(
                            egui::TextEdit::multiline(&mut p.note)
                                .desired_width(360.0)
                                .desired_rows(3)
                                .hint_text("What this folder is for"),
                        );
                        ui.label("Tags");
                        ui.add(
                            egui::TextEdit::singleline(&mut p.tags)
                                .desired_width(360.0)
                                .hint_text("comma, separated"),
                        );
                        if p.read_only {
                            ui.small(format!(
                                "{} was written by a newer version. It is shown here but not written over.",
                                store.display()
                            ));
                        }
                        let edited = (p.note.as_str(), p.tags.as_str()) != (p.loaded.0.as_str(), p.loaded.1.as_str());
                        ui.horizontal(|ui| {
                            if ui.add_enabled(edited && !p.read_only, egui::Button::new("Save")).clicked() {
                                save = true;
                            }
                            if ui.add_enabled(edited, egui::Button::new("Revert")).clicked() {
                                revert = true;
                            }
                        });
                        ui.small(format!("Kept in {}", store.display()));
                    }
                    // Annotating a drive root would need a sidecar in a folder
                    // above it, and there is none.
                    None => {
                        ui.small("A drive root cannot carry a note.");
                    }
                }
            });
        if revert {
            p.note = p.loaded.0.clone();
            p.tags = p.loaded.1.clone();
        }
        if save {
            self.save_item_meta(&mut p);
        }
        if open {
            self.props = Some(p);
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
            Modal::HarnessDelete { .. } => "Delete",
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
                    Modal::Delete { paths } => match paths.as_slice() {
                        [one] => {
                            ui.label(format!("Move {} to the {}?", one.display(), trash_ops::bin_name()));
                        }
                        many => {
                            ui.label(format!("Move {} items to the {}?", many.len(), trash_ops::bin_name()));
                            for p in many.iter().take(8) {
                                ui.small(p.display().to_string());
                            }
                            if many.len() > 8 {
                                ui.small(format!("and {} more", many.len() - 8));
                            }
                        }
                    },
                    Modal::HarnessDelete { item } => {
                        let target = harness::delete_target(item);
                        let n = if target.is_dir() { fs::read_dir(&target).map(|rd| rd.count()).unwrap_or(0) } else { 1 };
                        ui.label(format!("Move {} ({n} file(s)) to the {}?", target.display(), trash_ops::bin_name()));
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

    /// Settings in its own OS window, like the agents window, so it can sit
    /// beside the explorer instead of covering it. Falls back to an in-app
    /// window on a backend that cannot open a second native one.
    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let id = egui::ViewportId::from_hash_of("settings-window");
        let builder = titlebar::child_chrome(
            egui::ViewportBuilder::default()
                .with_title("Settings")
                .with_inner_size([640.0, 720.0])
                .with_min_inner_size([460.0, 400.0]),
        );
        ctx.show_viewport_immediate(id, builder, |ctx, class| {
            if class == egui::ViewportClass::Embedded {
                let mut open = true;
                egui::Window::new("Settings").open(&mut open).default_width(460.0).show(ctx, |ui| {
                    self.settings_body(ui);
                });
                self.settings_open = open;
                return;
            }
            titlebar::show(ctx, "Settings");
            egui::CentralPanel::default().show(ctx, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    self.settings_body(ui);
                });
            });
            titlebar::resize_handles(ctx);
            // Escape closes as well as the window button: a child window with
            // no keyboard way out is a trap on any platform whose chrome fails.
            if ctx.input(|i| i.viewport().close_requested() || i.key_pressed(egui::Key::Escape)) {
                self.settings_open = false;
            }
        });
    }

    fn settings_body(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        let mut changed = false;
        let mut reload = false;
        {
            {
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
                ui.small("Extensions, command, and the language id sent in didOpen. Servers start on first open.");
                let mut remove: Option<usize> = None;
                // Plain rows with explicit sizes rather than a Grid: a Grid cell
                // clamps a TextEdit to the column width it measured last frame, so
                // `desired_width` is ignored and the fields collapse to a few
                // characters however wide the window is.
                for (i, srv) in self.cfg.lsp_servers.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        let mut exts = srv.extensions.join(",");
                        if ui.add_sized([120.0, 20.0], egui::TextEdit::singleline(&mut exts)).changed() {
                            srv.extensions = exts.split(',').map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty()).collect();
                            changed = true;
                        }
                        let found = lsp::resolve(&srv.command).is_some();
                        let mut cmdline = if srv.args.is_empty() { srv.command.clone() } else { format!("{} {}", srv.command, srv.args.join(" ")) };
                        if ui.add_sized([300.0, 20.0], egui::TextEdit::singleline(&mut cmdline)).changed() {
                            let mut parts = cmdline.split_whitespace();
                            srv.command = parts.next().unwrap_or("").to_string();
                            srv.args = parts.map(|a| a.to_string()).collect();
                            changed = true;
                        }
                        if ui.add_sized([90.0, 20.0], egui::TextEdit::singleline(&mut srv.language_id)).changed() {
                            changed = true;
                        }
                        if found {
                            ui.label(egui::RichText::new("installed").small().color(egui::Color32::from_rgb(0x9e, 0xce, 0x6a)));
                        } else {
                            ui.label(egui::RichText::new("not found").small().weak());
                        }
                        if ui.small_button(icons::CLOSE).clicked() {
                            remove = Some(i);
                        }
                    });
                }
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
                ui.horizontal(|ui| {
                    ui.label("Editor font size");
                    if ui.add(egui::Slider::new(&mut self.cfg.editor_font_size, 8.0..=24.0)).changed() {
                        changed = true;
                    }
                });
                if ui.checkbox(&mut self.cfg.show_hidden, "Show hidden files").changed() {
                    reload = true;
                    changed = true;
                }
            }
        }
        if reload {
            self.refresh_all();
        }
        if changed {
            self.save_cfg();
        }
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
        // List navigation. Only in the explorer body: the IDE owns these keys.
        if self.body == Body::Explorer {
            let shift = i.modifiers.shift;
            if i.key_pressed(egui::Key::ArrowDown) {
                self.move_selection(1, shift);
            }
            if i.key_pressed(egui::Key::ArrowUp) {
                self.move_selection(-1, shift);
            }
            if i.key_pressed(egui::Key::PageDown) {
                self.move_selection(20, shift);
            }
            if i.key_pressed(egui::Key::PageUp) {
                self.move_selection(-20, shift);
            }
            if i.key_pressed(egui::Key::Home) {
                self.move_selection(-(self.entries.len() as isize), shift);
            }
            if i.key_pressed(egui::Key::End) {
                self.move_selection(self.entries.len() as isize, shift);
            }
            if cmd && i.key_pressed(egui::Key::A) {
                self.marked = self.entries.iter().map(|e| e.path.clone()).collect();
                self.status = format!("Selected {} item(s)", self.marked.len());
            }
            if cmd && i.key_pressed(egui::Key::F) {
                self.filter_focus = true;
            }
            if i.key_pressed(egui::Key::Escape) && !self.filter.is_empty() {
                self.filter.clear();
                self.reload();
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
            let paths = self.action_paths();
            if !paths.is_empty() {
                self.modal = Some(Modal::Delete { paths });
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
        if cmd && !i.modifiers.shift && i.key_pressed(egui::Key::N) {
            self.modal = Some(Modal::NewFile { name: "untitled.txt".into() });
        }
        if cmd && i.key_pressed(egui::Key::S) && self.body == Body::Ide {
            self.editor.save_active(self.cfg.format_json_on_save);
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

impl ExplorerApp {
    /// Hand an in-app drag to the OS once the pointer leaves the window, so a
    /// file can be dropped on Explorer, Finder or another app. `start_drag`
    /// runs the platform's own modal drag loop and only returns on drop or
    /// cancel, so the UI stops repainting for the rest of the gesture; the
    /// shell paints the drag image in the meantime.
    #[cfg(any(windows, target_os = "macos"))]
    fn maybe_hand_drag_to_os(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        let Some(payload) = egui::DragAndDrop::payload::<attach::DragPaths>(ctx) else {
            self.os_drag_handed_off = false;
            return;
        };
        if self.os_drag_handed_off {
            return;
        }
        // The platform loop only ends when the button that started the drag is
        // released, so never enter it unless that button is genuinely held.
        // Without this the loop is entered on a stale payload and never returns,
        // leaving a window that repaints but processes nothing.
        let screen = ctx.screen_rect();
        // Without pointer capture winit stops reporting positions once the
        // cursor leaves, and egui clears `latest_pos`, so "no position while a
        // button is held" is itself the signal that the drag has left.
        let (held, outside) = ctx.input(|i| {
            (i.pointer.primary_down(), i.pointer.latest_pos().map(|p| !screen.contains(p)).unwrap_or(true))
        });
        if !held {
            return;
        }
        if !outside {
            return;
        }
        self.os_drag_handed_off = true;
        let paths = payload.0.clone();
        egui::DragAndDrop::clear_payload(ctx);
        let names: Vec<String> = paths
            .iter()
            .map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
            .collect();
        // An empty image is fine: the crate skips the drag-image helper when it
        // cannot decode one, and the shell falls back to its own icon.
        match drag::start_drag(
            frame,
            drag::DragItem::Files(paths),
            drag::Image::Raw(Vec::new()),
            |_result, _pos| {},
            drag::Options::default(),
        ) {
            Ok(()) => self.status = format!("Dragged {} to another app", names.join(", ")),
            Err(e) => self.status = format!("OS drag failed: {e}"),
        }
    }
}

impl ExplorerApp {
    /// Linux has no hand-off: the `drag` crate's GTK backend wants a
    /// `gtk::ApplicationWindow` and eframe cannot supply one, so the drag
    /// stays internal and the attempt is reported once rather than silently
    /// doing nothing.
    #[cfg(not(any(windows, target_os = "macos")))]
    fn maybe_hand_drag_to_os(&mut self, ctx: &egui::Context, _frame: &eframe::Frame) {
        let Some(_p) = egui::DragAndDrop::payload::<attach::DragPaths>(ctx) else {
            self.os_drag_handed_off = false;
            return;
        };
        if self.os_drag_handed_off {
            return;
        }
        let screen = ctx.screen_rect();
        let (held, outside) = ctx.input(|i| {
            (i.pointer.primary_down(), i.pointer.latest_pos().map(|p| !screen.contains(p)).unwrap_or(true))
        });
        if held && outside {
            self.os_drag_handed_off = true;
            self.status = "Dragging to another application is not supported on this platform".into();
        }
    }
}

impl eframe::App for ExplorerApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if let Some(t) = self.pending_theme.take() {
            theme::apply(ctx, t);
        }
        self.shortcuts(ctx);
        self.maybe_hand_drag_to_os(ctx, frame);

        drag_preview(ctx);
        titlebar::show(ctx, "The Real Explorer");
        egui::TopBottomPanel::top("menu").show(ctx, |ui| self.menu_bar(ctx, ui));
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            self.toolbar(ui);
            ui.add_space(2.0);
            self.breadcrumbs(ui);
            ui.add_space(2.0);
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
                    let size = self.cfg.editor_font_size;
                    ui.style_mut().text_styles.insert(
                        egui::TextStyle::Monospace,
                        egui::FontId::new(size, egui::FontFamily::Monospace),
                    );
                    self.editor.show(ui, &servers, self.cfg.format_json_on_save);
                    if !self.editor.status.is_empty() {
                        self.status = std::mem::take(&mut self.editor.status);
                    }
                }
            }
        });

        self.poll_harness();
        for ev in std::mem::take(&mut self.chat.hook_events) {
            self.harness.push_hook_event(ev);
        }
        let project = self.cwd.clone();
        for action in self.harness.show(ctx, &project) {
            match action {
                harness_ui::Action::OpenInIde(p) => self.open_in_ide(&p),
                harness_ui::Action::Reveal(p) => self.reveal(&p),
                harness_ui::Action::Delete(item) => self.modal = Some(Modal::HarnessDelete { item }),
                harness_ui::Action::Refresh => self.refresh_harness(),
                harness_ui::Action::Status(s) => self.status = s,
            }
        }
        if self.harness_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
        }
        self.exit_guard(ctx);
        self.paste_conflict_window(ctx);
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
        // Home first: `/` is mostly the read-only system volume and is rarely
        // what someone opening a file explorer wants to see.
        let mut v = Vec::new();
        if let Some(h) = dirs::home_dir() {
            v.push(h);
        }
        v.push(PathBuf::from("/Volumes"));
        v.push(PathBuf::from("/"));
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

/// Copy a file or a directory tree.
///
/// Symlinks are skipped rather than followed. `Path::is_dir` follows them, and
/// a link pointing back at an ancestor made this recurse until the stack ran
/// out and the process died: reachable from an ordinary paste, or from a cut
/// across volumes, which falls back to copy then delete.
fn copy_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    let md = fs::symlink_metadata(src)?;
    if md.file_type().is_symlink() {
        return Ok(());
    }
    if md.is_dir() {
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
