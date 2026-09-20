//! IDE tab: open-file strip, syntax-highlighted text editor, LSP-backed
//! completion popup and diagnostics. One `LspClient` is kept per server
//! command and shared by every open document that maps to it.
//!
//! Cursor positions flow in three forms: egui char index (chars from start),
//! LSP (zero-based line, UTF-16 column) and byte offsets for slicing. The
//! helpers at the bottom convert between them.
//!
//! Beyond the base editor this module also handles: an unsaved-changes guard
//! on tab close, detection of edits made to the file from outside the app
//! (the assistant included), Ctrl+F/H find-and-replace and Ctrl+G go-to-line
//! with cheap match highlighting via the syntax layouter, a line-number
//! gutter (source-line-per-row only; see `Editor::show`), a word-wrap
//! toggle, a size cap and lossy read-only fallback for files the editor
//! cannot safely round-trip, LSP-backed formatting for non-JSON files, hover
//! on mouse dwell as well as Ctrl+K, and BOM-safe JSON handling.
//!
//! Two rendering paths share all of that. Below [`LARGE_DOC_BYTES`] a
//! document is an editable `egui::TextEdit` laid out in one galley, which is
//! what every feature here was written against. At or above it the document
//! opens read-only into [`Editor::show_large`], a virtualised view that lays
//! out only the rows on screen: egui keeps a pre-reserved mesh for every
//! glyph in a galley and never shrinks it, about 171 bytes per character,
//! so one galley over a 5.8 MB file is 1 GB of memory that no amount of
//! clipping recovers. The split is by size rather than by a mode switch
//! because the two paths do not agree on who owns the cursor, and only the
//! editable one can answer that today.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{self, text::LayoutJob, Color32, Key, Modifiers};

use crate::highlight;
use crate::lsp::{self, CompletionItem, Diagnostic, LspClient, LspEvent, ServerDef};
use crate::textpos::{find_matches, replace_all, LineIndex};

/// Files larger than this are refused at open.
///
/// Anything at or above [`LARGE_DOC_BYTES`] opens into the virtualised
/// read-only view, whose cost is the viewport rather than the document, so
/// the ceiling is now about reading the bytes and indexing the lines rather
/// than about laying them out.
const MAX_OPEN_BYTES: u64 = 8 * 1024 * 1024;

/// At or above this size a document opens read-only into the virtualised
/// view instead of a `TextEdit`.
///
/// Measured on egui 0.29: one galley costs about 171 bytes per character and
/// an edit frame holds two of them, so 1 MiB is already 180 MB steady and
/// 360 MB per keystroke on the `TextEdit` path. Below the threshold that is
/// the price of a real editor; at or above it the document is worth more as
/// something you can open at all than as something you can type into.
/// Editing the virtualised view means the editor owning the cursor, the
/// selection and undo, which is a separate piece of work.
pub const LARGE_DOC_BYTES: usize = 1024 * 1024;

/// How much of one source line the virtualised view lays out.
///
/// Word wrap is off there, so a line is one row however long it is. Without
/// a cap a minified file with no newlines would be a single galley over the
/// whole document, which is the cost the view exists to avoid. Columns past
/// this are not drawn and the toolbar says so.
const LINE_COLUMN_CAP: usize = 4096;

/// Longest line the syntax highlighter is willing to see.
///
/// syntect parses one whole line per call, so a multi-megabyte line is an
/// unbounded regex scan with no way to yield in the middle of it. A document
/// with a line this long is shown plain rather than hanging the parser; the
/// only real files shaped like that are minified or generated.
const HIGHLIGHT_MAX_LINE_BYTES: usize = 128 * 1024;

/// Largest markdown document the preview will render. The preview is a
/// separate whole-document renderer with the same per-glyph cost as the
/// editor, and nothing else bounds it.
const PREVIEW_MAX_BYTES: usize = 512 * 1024;

/// How long the pointer must rest in one spot before hover fires on its own,
/// separate from the explicit Ctrl+K request.
const HOVER_DWELL_MS: u64 = 500;

pub struct Doc {
    pub path: PathBuf,
    pub text: String,
    /// Line start offsets in bytes and chars, so cursor, LSP and byte
    /// positions convert without walking the text. Rebuilt by
    /// `text_replaced` after every edit; stale for exactly the span between
    /// an edit and that call, which is why every splice ends with it.
    pub lines: LineIndex,
    /// Copy of `text` from last frame, to detect edits made by the widget.
    last_text: String,
    pub version: i64,
    pub dirty: bool,
    /// Command of the server handling this file, key into `Editor::servers`.
    server: Option<String>,
    pub diagnostics: Vec<Diagnostic>,
    /// Disk mtime/size recorded at open and after every save, so the editor
    /// can tell its own writes apart from someone else's.
    mtime: Option<SystemTime>,
    disk_size: u64,
    /// Set when the file changed on disk while the buffer was dirty; cleared
    /// once the user picks Reload or Keep mine.
    disk_conflict: bool,
    /// True for a lossy (non-UTF-8) read. Saving is refused so a
    /// best-effort decode can never clobber the original bytes.
    pub read_only: bool,
    /// The file had a UTF-8 BOM at open; `text` never carries it (parsers
    /// choke on it) but it is written back on save.
    has_bom: bool,
    /// Incremental syntax highlighting state for this document: per-line style
    /// runs and parse checkpoints. See `crate::highlight`.
    hl: highlight::DocHighlight,
}

impl Doc {
    /// Recompute the line index after `text` changed. One pass over the
    /// bytes; every edit path calls it so nothing reads a stale index.
    fn text_replaced(&mut self) {
        self.lines = LineIndex::build(&self.text);
    }

    /// How far the background colouring pass has got, or `None` when there is
    /// nothing outstanding. Exposed so the headless tests can wait for a
    /// quiet frame before measuring allocation.
    pub fn colouring_progress(&self) -> Option<f32> {
        self.hl.progress()
    }

    /// Whether this document's lines are short enough for syntect to parse
    /// one at a time. See [`HIGHLIGHT_MAX_LINE_BYTES`].
    fn highlightable(&self) -> bool {
        self.lines.max_line_bytes() <= HIGHLIGHT_MAX_LINE_BYTES
    }
}

struct Completion {
    items: Vec<CompletionItem>,
    selected: usize,
    /// Char index where the identifier being completed starts.
    prefix_start: usize,
}

/// Ctrl+F / Ctrl+H state. Matches are recomputed from the live text every
/// frame rather than cached: editor-sized buffers make that cheap, and it
/// can never go stale after an edit or a replace.
struct FindState {
    query: String,
    replace: String,
    show_replace: bool,
    case_insensitive: bool,
    /// Index into the current match list. Clamped fresh each frame since an
    /// edit or a replace can shrink the list out from under it.
    current: usize,
    /// Set for one frame (by Next/Prev/Replace) to ask the text area to move
    /// its selection and scroll to `current`; consumed the same frame.
    jump: bool,
    /// Grabs keyboard focus the first frame the bar is shown.
    just_opened: bool,
}

pub struct Editor {
    pub docs: Vec<Doc>,
    pub active: usize,
    servers: HashMap<String, LspClient>,
    /// Servers that failed to start, so we do not retry every frame.
    failed: HashMap<String, String>,
    completion: Option<Completion>,
    pending_completion: Option<u64>,
    /// Outstanding `textDocument/formatting` request and which doc it is for
    /// (not necessarily still the active one by the time the reply arrives).
    pending_format: Option<(u64, usize)>,
    hover: Option<(u64, String)>,
    /// Screen position to anchor the hover tooltip at: the mouse position for
    /// a dwell-triggered hover, the caret for a Ctrl+K one.
    hover_anchor: Option<egui::Pos2>,
    /// Where the pointer has been resting, and since when, for dwell hover.
    hover_watch: Option<(egui::Pos2, Instant)>,
    /// The last position a hover request was already fired for, so holding
    /// still after the reply comes back does not spam the server.
    hover_requested_for: Option<egui::Pos2>,
    pub status: String,
    cursor_char: usize,
    /// True when a file was opened this frame: focus the editor and reset cursor.
    focus_next: bool,
    /// Missing-server note, shown quietly under the tab strip instead of in
    /// the status bar, which the explorer shares for item counts.
    lsp_note: Option<String>,
    /// Render markdown instead of editing it.
    preview: bool,
    md_cache: egui_commonmark::CommonMarkCache,
    /// Off by default: keeps the existing horizontal-scroll behaviour.
    word_wrap: bool,
    find: Option<FindState>,
    /// Ctrl+G bar text, while open.
    goto_line: Option<String>,
    /// A char index a find/replace jump or a goto-line asked the text area to
    /// scroll to, consumed the next time the body is drawn.
    pending_scroll_to: Option<usize>,
    /// Index of a dirty tab pending its close confirmation.
    pending_close: Option<usize>,
    last_disk_check: Instant,
    /// Size at or above which a document uses the virtualised read-only view.
    /// A field rather than [`LARGE_DOC_BYTES`] directly so a test can put a
    /// small fixture through the same path; nothing in the app changes it.
    large_threshold: usize,
    /// Vertical scroll offset the large view reported last frame, so PageUp
    /// and PageDown have something to move relative to.
    large_offset: f32,
    /// A line on screen was longer than [`LINE_COLUMN_CAP`] last frame, so
    /// the toolbar says the view is not showing all of it.
    large_line_clipped: bool,
}

impl Editor {
    pub fn new() -> Self {
        Self {
            docs: Vec::new(),
            active: 0,
            servers: HashMap::new(),
            failed: HashMap::new(),
            completion: None,
            pending_completion: None,
            pending_format: None,
            hover: None,
            hover_anchor: None,
            hover_watch: None,
            hover_requested_for: None,
            status: String::new(),
            cursor_char: 0,
            focus_next: false,
            lsp_note: None,
            preview: false,
            md_cache: egui_commonmark::CommonMarkCache::default(),
            word_wrap: false,
            find: None,
            goto_line: None,
            pending_scroll_to: None,
            pending_close: None,
            // Backdated so the very first frame already checks once, instead
            // of waiting a full second after startup.
            last_disk_check: Instant::now() - Duration::from_secs(1),
            large_threshold: LARGE_DOC_BYTES,
            large_offset: 0.0,
            large_line_clipped: false,
        }
    }

    /// Move the size at which documents switch to the virtualised read-only
    /// view. For tests that need a small fixture on the large path (or a
    /// large one on the `TextEdit` path) without writing megabytes to disk.
    pub fn set_large_threshold(&mut self, bytes: usize) {
        self.large_threshold = bytes;
    }

    /// True when document `i` is shown through the virtualised read-only view.
    pub fn is_large(&self, i: usize) -> bool {
        self.docs.get(i).map(|d| d.text.len() >= self.large_threshold).unwrap_or(false)
    }

    /// True when document `i` accepts edits: not a lossy decode, and not big
    /// enough to be on the read-only virtualised path.
    fn editable(&self, i: usize) -> bool {
        self.docs.get(i).map(|d| !d.read_only).unwrap_or(false) && !self.is_large(i)
    }

    /// Why document `i` will not take an edit, phrased for the status bar.
    /// The lossy-decode case comes first because it is the stricter one: such
    /// a file cannot be written back safely at any size.
    fn readonly_reason(&self, i: usize) -> String {
        if self.docs.get(i).map(|d| d.read_only).unwrap_or(false) {
            "Cannot edit: file is read-only (opened as lossy UTF-8)".into()
        } else if self.is_large(i) {
            format!(
                "Cannot edit: files of {} MiB and over open read-only",
                self.large_threshold / (1024 * 1024)
            )
        } else {
            "Cannot edit this document".into()
        }
    }

    /// Open (or focus) a file. Starts the matching language server on demand,
    /// rooted at `root` (the explorer's current folder or a detected project).
    /// Open `path` in a tab, or focus its tab if it already has one.
    /// Returns false when nothing was opened (unreadable, over the size cap),
    /// so the caller does not switch to an editor with nothing in it.
    pub fn open(&mut self, path: &Path, root: &Path, servers: &[ServerDef]) -> bool {
        if let Some(i) = self.docs.iter().position(|d| d.path == path) {
            self.active = i;
            self.focus_next = true;
            return true;
        }
        let meta = match fs::metadata(path) {
            Ok(m) => m,
            Err(e) => {
                self.status = format!("Cannot open {}: {e}", path.display());
                return false;
            }
        };
        if exceeds_cap(meta.len()) {
            self.status = format!(
                "{} is {:.1} MiB, over the {} MiB editor limit; not opening",
                path.display(),
                meta.len() as f64 / (1024.0 * 1024.0),
                MAX_OPEN_BYTES / (1024 * 1024),
            );
            return false;
        }
        let (text, has_bom, read_only) = match load_file(path) {
            Ok(v) => v,
            Err(e) => {
                self.status = format!("Cannot open {}: {e}", path.display());
                return false;
            }
        };
        if read_only {
            self.status = format!("{} is not valid UTF-8; opened read-only (lossy decode)", path.display());
        }
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        let def = servers.iter().find(|s| s.extensions.iter().any(|x| *x == ext)).cloned();
        let mut doc = Doc {
            path: path.to_path_buf(),
            lines: LineIndex::build(&text),
            text: text.clone(),
            last_text: text.clone(),
            version: 0,
            dirty: false,
            server: None,
            diagnostics: Vec::new(),
            mtime: meta.modified().ok(),
            disk_size: meta.len(),
            disk_conflict: false,
            read_only,
            has_bom,
            // The grammar is resolved once, here, rather than looked up by
            // extension on every frame the way the old layouter did.
            hl: highlight::DocHighlight::new(&ext),
        };
        // A lossy read cannot safely round-trip through a language server
        // either (it would be editing bytes that do not exist in the file),
        // so read-only documents skip LSP entirely.
        if !read_only {
            if let Some(def) = def {
                let project_root = find_project_root(path, &def).unwrap_or_else(|| root.to_path_buf());
                if !self.servers.contains_key(&def.command) && !self.failed.contains_key(&def.command) {
                    // A server that is simply not installed is not news: the editor
                    // works without it and Settings lists what is missing. Only a
                    // binary that exists and then fails to start is worth saying.
                    if lsp::resolve(&def.command).is_none() {
                        self.failed.insert(def.command.clone(), format!("{} not installed", def.command));
                    } else {
                        match LspClient::start(&def, &project_root) {
                            Ok(c) => {
                                self.servers.insert(def.command.clone(), c);
                                self.status = format!("{} starting for {}", def.command, project_root.display());
                            }
                            Err(e) => {
                                self.failed.insert(def.command.clone(), e.clone());
                                self.lsp_note = Some(format!("{} failed to start: {e}", def.command));
                            }
                        }
                    }
                }
                if let Some(c) = self.servers.get_mut(&def.command) {
                    c.did_open(path, &def.language_id, &text);
                    doc.server = Some(def.command.clone());
                }
            }
        }
        self.docs.push(doc);
        self.active = self.docs.len() - 1;
        self.focus_next = true;
        true
    }

    /// Removes a tab unconditionally. Called once the caller has already
    /// decided a dirty buffer's contents may be lost (Discard, or the tab
    /// was already clean); everything reachable from the tab strip's close
    /// button instead goes through the confirm flow in `show`.
    pub fn close(&mut self, i: usize) {
        if i >= self.docs.len() {
            return;
        }
        let d = self.docs.remove(i);
        if let Some(c) = d.server.as_ref().and_then(|s| self.servers.get_mut(s)) {
            c.did_close(&d.path);
        }
        // Removing an earlier tab shifts every later index down, so the active
        // index has to follow it or the editor silently shows a different file
        // than the one the tab strip highlights.
        if i < self.active {
            self.active -= 1;
        }
        if self.active >= self.docs.len() && !self.docs.is_empty() {
            self.active = self.docs.len() - 1;
        }
        self.completion = None;
        if self.pending_close == Some(i) {
            self.pending_close = None;
        }
    }

    /// Paths of every document with unsaved changes, for the app to check
    /// before it lets the window close.
    pub fn dirty_paths(&self) -> Vec<PathBuf> {
        self.docs.iter().filter(|d| d.dirty).map(|d| d.path.clone()).collect()
    }

    /// Saves every dirty document. Meant to back an "exit anyway" prompt:
    /// call this, then check `dirty_paths()` again (a read-only doc that
    /// somehow became dirty, or a write failure, would still show up there).
    pub fn save_all(&mut self, format_json_on_save: bool) {
        for i in 0..self.docs.len() {
            if self.docs[i].dirty {
                self.save_doc(i, format_json_on_save);
            }
        }
    }

    /// Pretty-print the active document if it is JSON. Returns Err with the
    /// parse error when the text is not valid JSON, leaving it untouched.
/// JSON-only formatting. `format_current` is the entry point the UI uses;
    /// this stays for the save path and for tests.
    #[allow(dead_code)]
    pub fn format_active(&mut self) -> Result<(), String> {
        self.format_json_at(self.active)
    }

    fn format_json_at(&mut self, i: usize) -> Result<(), String> {
        if !self.editable(i) {
            return Err(self.readonly_reason(i).replace("Cannot edit", "Cannot format"));
        }
        let Some(d) = self.docs.get_mut(i) else { return Ok(()) };
        if !is_json(&d.path) {
            return Err("Format: only JSON is supported so far".into());
        }
        // Defensive: `d.text` should already be BOM-free (open/reload strip
        // it), but stripping again here means this function is correct even
        // if that invariant is ever violated, and it is what makes the "BOM"
        // unit test below meaningful without a live Editor.
        let (_, body) = strip_bom(&d.text);
        let v: serde_json::Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
        let mut out = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
        out.push('\n');
        if out != d.text {
            let line = highlight::first_changed_line(&d.text, &out);
            d.text = out.clone();
            d.text_replaced();
            d.last_text = out;
            d.hl.note_edit(line);
            d.dirty = true;
            d.version += 1;
            let (path, version, text) = (d.path.clone(), d.version, d.text.clone());
            if let Some(c) = d.server.as_ref().and_then(|s| self.servers.get_mut(s)) {
                c.did_change(&path, version, &text);
            }
        }
        Ok(())
    }

    /// Formats the active document: the built-in pretty-printer for JSON, or
    /// `textDocument/formatting` on whichever language server owns the file
    /// for anything else. The LSP path is async (see `LspEvent::Formatting`
    /// in `poll_servers`), so this only starts the request; `self.status`
    /// reports what happened either way, including a server that exists but
    /// never advertised a formatter.
/// Mark every buffer clean without writing, so a deliberate "discard and
    /// exit" does not trip the guard again on the way out.
    pub fn discard_all_dirty(&mut self) {
        for d in &mut self.docs {
            d.dirty = false;
        }
    }

    pub fn format_current(&mut self) {
        let Some(d) = self.docs.get(self.active) else { return };
        if is_json(&d.path) {
            match self.format_json_at(self.active) {
                Ok(()) => self.status = "Formatted".into(),
                Err(e) => self.status = e,
            }
            return;
        }
        if !self.editable(self.active) {
            self.status = self.readonly_reason(self.active).replace("Cannot edit", "Cannot format");
            return;
        }
        let Some(d) = self.docs.get(self.active) else { return };
        let Some(server_name) = d.server.clone() else {
            self.status = "Format: no language server for this file".into();
            return;
        };
        let path = d.path.clone();
        let idx = self.active;
        let Some(c) = self.servers.get_mut(&server_name) else { return };
        if !c.supports_formatting {
            self.status = format!("{server_name} does not advertise a document formatter");
            return;
        }
        let id = c.formatting(&path, 4, true);
        self.pending_format = Some((id, idx));
        self.status = "Formatting...".into();
    }

    fn save_doc(&mut self, i: usize, format_json_on_save: bool) {
        // A virtualised document is never dirty, so this only fires on an
        // explicit Ctrl+S; saying so beats writing the file back unchanged.
        if !self.editable(i) {
            self.status = self.readonly_reason(i).replace("Cannot edit", "Cannot save");
            return;
        }
        if format_json_on_save && self.docs.get(i).map(|d| is_json(&d.path)).unwrap_or(false) {
            if let Err(e) = self.format_json_at(i) {
                self.status = format!("Saved without formatting: {e}");
            }
        }
        let Some(d) = self.docs.get_mut(i) else { return };
        // The BOM never lives in `d.text` (parsers choke on it); it is only
        // ever added back here, right before the bytes hit disk.
        let mut bytes = Vec::with_capacity(d.text.len() + 3);
        if d.has_bom {
            bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        }
        bytes.extend_from_slice(d.text.as_bytes());
        match fs::write(&d.path, &bytes) {
            Ok(()) => {
                d.dirty = false;
                d.disk_conflict = false;
                self.status = format!("Saved {}", d.path.display());
                // Refresh the disk baseline immediately so the next
                // external-change poll does not mistake our own write for a
                // foreign edit.
                if let Ok(meta) = fs::metadata(&d.path) {
                    d.mtime = meta.modified().ok();
                    d.disk_size = meta.len();
                }
                if let Some(c) = d.server.as_ref().and_then(|s| self.servers.get_mut(s)) {
                    c.did_save(&d.path, &d.text);
                }
            }
            Err(e) => self.status = format!("Save failed: {e}"),
        }
    }

    pub fn save_active(&mut self, format_json_on_save: bool) {
        self.save_doc(self.active, format_json_on_save);
    }

    /// Once-a-second poll for edits made to an open file from outside the
    /// app (a shell command, another editor, or this app's own assistant
    /// panel editing files in the same folder). A clean buffer is reloaded
    /// silently; a dirty one only gets a flag, resolved by `show`'s conflict
    /// bar, so the user's unsaved edits are never overwritten silently.
    fn check_external_changes(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_disk_check) < Duration::from_secs(1) {
            return;
        }
        self.last_disk_check = now;
        for i in 0..self.docs.len() {
            let path = self.docs[i].path.clone();
            let Ok(meta) = fs::metadata(&path) else { continue };
            let new_mtime = meta.modified().ok();
            let new_size = meta.len();
            let (old_mtime, old_size, dirty, read_only) = {
                let d = &self.docs[i];
                (d.mtime, d.disk_size, d.dirty, d.read_only)
            };
            if read_only || !disk_changed(old_mtime, old_size, new_mtime, new_size) {
                continue;
            }
            if dirty {
                self.docs[i].disk_conflict = true;
                continue;
            }
            if exceeds_cap(new_size) {
                // Grew past the size the editor is willing to hold in memory
                // and re-lay every frame; leave the stale buffer rather than
                // stall the UI thread reading it. Move the baseline so this
                // does not re-fire every second.
                self.docs[i].mtime = new_mtime;
                self.docs[i].disk_size = new_size;
                self.status = format!("{} grew past the {} MiB limit; not auto-reloading", path.display(), MAX_OPEN_BYTES / (1024 * 1024));
                continue;
            }
            match load_file(&path) {
                Ok((text, has_bom, read_only)) => {
                    let d = &mut self.docs[i];
                    d.text = text.clone();
                    d.text_replaced();
                    d.last_text = text;
                    d.hl.reset();
                    d.has_bom = has_bom;
                    d.read_only = read_only;
                    d.version += 1;
                    d.mtime = new_mtime;
                    d.disk_size = new_size;
                    d.disk_conflict = false;
                    let (p, version, text2) = (d.path.clone(), d.version, d.text.clone());
                    if let Some(c) = d.server.as_ref().and_then(|s| self.servers.get_mut(s)) {
                        c.did_change(&p, version, &text2);
                    }
                    self.status = format!("Reloaded {} (changed on disk)", p.display());
                }
                Err(_) => {
                    // Likely a transient I/O error (mid-write by whatever
                    // touched the file); try again on the next tick.
                }
            }
        }
    }

    /// Applies the user's choice on the conflict bar for doc `i`.
    fn resolve_conflict(&mut self, i: usize, keep_mine: bool) {
        let Some(path) = self.docs.get(i).map(|d| d.path.clone()) else { return };
        if keep_mine {
            // Trust the in-memory buffer; just move the baseline so the poll
            // stops re-flagging the same disk change every second. The
            // buffer stays dirty and the next save wins normally.
            if let Ok(meta) = fs::metadata(&path) {
                if let Some(d) = self.docs.get_mut(i) {
                    d.mtime = meta.modified().ok();
                    d.disk_size = meta.len();
                }
            }
            if let Some(d) = self.docs.get_mut(i) {
                d.disk_conflict = false;
            }
            self.status = format!("Keeping your changes to {}", path.display());
            return;
        }
        match load_file(&path) {
            Ok((text, has_bom, read_only)) => {
                let meta = fs::metadata(&path).ok();
                if let Some(d) = self.docs.get_mut(i) {
                    d.text = text.clone();
                    d.text_replaced();
                    d.last_text = text.clone();
                    d.hl.reset();
                    d.has_bom = has_bom;
                    d.read_only = read_only;
                    d.dirty = false;
                    d.disk_conflict = false;
                    d.version += 1;
                    if let Some(m) = &meta {
                        d.mtime = m.modified().ok();
                        d.disk_size = m.len();
                    }
                }
                if let Some(d) = self.docs.get(i) {
                    let (p, version, t, server) = (d.path.clone(), d.version, d.text.clone(), d.server.clone());
                    if let Some(name) = server {
                        if let Some(c) = self.servers.get_mut(&name) {
                            c.did_change(&p, version, &t);
                        }
                    }
                }
                self.status = format!("Reloaded {}", path.display());
            }
            Err(e) => self.status = format!("Reload failed: {e}"),
        }
    }

    /// Drain every server's events into documents.
    fn poll_servers(&mut self) {
        let mut events = Vec::new();
        let mut dead = Vec::new();
        for (name, c) in self.servers.iter_mut() {
            for ev in c.poll() {
                if let LspEvent::Died(e) = &ev {
                    dead.push((name.clone(), e.clone()));
                }
                events.push(ev);
            }
        }
        for (name, e) in dead {
            self.servers.remove(&name);
            self.failed.insert(name.clone(), e.clone());
            self.status = format!("{name} exited: {e}");
        }
        for ev in events {
            match ev {
                LspEvent::Initialized => self.status = "Language server ready".into(),
                LspEvent::Diagnostics { uri, items } => {
                    if let Some(p) = lsp::uri_to_path(&uri) {
                        if let Some(d) = self.docs.iter_mut().find(|d| same_file(&d.path, &p)) {
                            d.diagnostics = items;
                        }
                    }
                }
                LspEvent::Completion { id, items } => {
                    if self.pending_completion == Some(id) {
                        self.pending_completion = None;
                        let prefix_start = self.prefix_start();
                        if items.is_empty() {
                            self.completion = None;
                        } else {
                            self.completion = Some(Completion { items, selected: 0, prefix_start });
                        }
                    }
                }
                LspEvent::Hover { id, text } => {
                    if self.hover.as_ref().map(|h| h.0) == Some(id) && !text.is_empty() {
                        self.hover = Some((id, text));
                    }
                }
                LspEvent::Formatting { id, edits } => {
                    if let Some((pid, idx)) = self.pending_format {
                        if pid == id {
                            self.pending_format = None;
                            if let Some(d) = self.docs.get_mut(idx) {
                                if edits.is_empty() {
                                    self.status = "Formatted (no changes)".into();
                                } else if d.read_only {
                                    self.status = "Format reply arrived but the file is read-only; discarded".into();
                                } else {
                                    let before = std::mem::take(&mut d.last_text);
                                    apply_text_edits(&mut d.text, &edits);
                                    d.text_replaced();
                                    d.hl.note_edit(highlight::first_changed_line(&before, &d.text));
                                    d.last_text = d.text.clone();
                                    d.dirty = true;
                                    d.version += 1;
                                    self.status = "Formatted".into();
                                    let (path, version, text) = (d.path.clone(), d.version, d.text.clone());
                                    if let Some(c) = d.server.as_ref().and_then(|s| self.servers.get_mut(s)) {
                                        c.did_change(&path, version, &text);
                                    }
                                }
                            }
                        }
                    }
                }
                LspEvent::Message(m) => self.status = m,
                LspEvent::Died(_) => {}
            }
        }
    }

    /// Char index where the identifier under the cursor begins.
    fn prefix_start(&self) -> usize {
        let Some(d) = self.docs.get(self.active) else { return 0 };
        let c = self.cursor_char.min(d.lines.char_count());
        let line = d.lines.line_of_char(c);
        let line_char0 = d.lines.line_char_start(line);
        let within = c - line_char0;
        let prefix: Vec<char> = d.text[d.lines.line_byte_start(line)..].chars().take(within).collect();
        let mut i = prefix.len();
        while i > 0 && (prefix[i - 1].is_alphanumeric() || prefix[i - 1] == '_') {
            i -= 1;
        }
        line_char0 + i
    }

    fn request_completion(&mut self) {
        // Nothing can be inserted into a read-only or virtualised document,
        // so asking the server would only produce a popup that refuses.
        if !self.editable(self.active) {
            return;
        }
        let Some(d) = self.docs.get(self.active) else { return };
        let Some(c) = d.server.as_ref().and_then(|s| self.servers.get_mut(s)) else { return };
        let (line, col) = d.lines.char_to_lsp(&d.text, self.cursor_char);
        let id = c.completion(&d.path, line, col);
        self.pending_completion = Some(id);
    }

    /// Insert a newline that keeps the current line's leading whitespace, and
    /// one more level after a line that ends in an opening bracket.
    fn insert_newline_with_indent(&mut self, ctx: &egui::Context, edit_id: egui::Id) {
        if !self.editable(self.active) {
            return;
        }
        let Some(d) = self.docs.get_mut(self.active) else { return };
        let at_c = self.cursor_char.min(d.lines.char_count());
        let at = d.lines.char_to_byte(&d.text, at_c);
        let line = d.lines.line_of_char(at_c);
        // `newline_indent` only looks back to the start of the current line,
        // so handing it that line's prefix gives the same answer as the whole
        // text did, without copying the whole text.
        let line0 = d.lines.line_byte_start(line);
        let line_prefix: Vec<char> = d.text[line0..at].chars().collect();
        let insert = newline_indent(&line_prefix);
        d.text.insert_str(at, &insert);
        let cursor = at_c + insert.chars().count();
        d.text_replaced();
        d.hl.note_edit(line);
        let mut state = egui::TextEdit::load_state(ctx, edit_id).unwrap_or_default();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(cursor))));
        egui::TextEdit::store_state(ctx, edit_id, state);
        self.cursor_char = cursor;
    }

    fn accept_completion(&mut self, ctx: &egui::Context, edit_id: egui::Id) {
        if !self.editable(self.active) {
            self.completion = None;
            return;
        }
        let Some(comp) = self.completion.take() else { return };
        let Some(item) = comp.items.get(comp.selected) else { return };
        let Some(d) = self.docs.get_mut(self.active) else { return };
        let start = comp.prefix_start.min(d.lines.char_count());
        let end = self.cursor_char.min(d.lines.char_count());
        // The prefix start was recorded when the list arrived. If the caret has
        // since moved before it, splicing would duplicate the text between
        // them rather than replace it.
        if end < start {
            return;
        }
        let sb = d.lines.char_to_byte(&d.text, start);
        let eb = d.lines.char_to_byte(&d.text, end);
        let line = d.lines.line_of_char(start);
        d.text.replace_range(sb..eb, &item.insert_text);
        let new_cursor = start + item.insert_text.chars().count();
        d.text_replaced();
        d.hl.note_edit(line);
        // Move the widget's cursor to the end of the inserted text.
        let mut state = egui::TextEdit::load_state(ctx, edit_id).unwrap_or_default();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(new_cursor))));
        egui::TextEdit::store_state(ctx, edit_id, state);
        self.cursor_char = new_cursor;
    }

    // ---- find / replace / goto-line ----

    fn open_find(&mut self, with_replace: bool) {
        self.goto_line = None;
        // Ctrl+H on a document that cannot take an edit still opens the bar,
        // because finding is the useful half; it just has no replace field
        // rather than one whose buttons all refuse.
        let with_replace = with_replace && self.editable(self.active);
        match &mut self.find {
            Some(f) => f.show_replace |= with_replace,
            None => {
                self.find = Some(FindState {
                    query: String::new(),
                    replace: String::new(),
                    show_replace: with_replace,
                    case_insensitive: true,
                    current: 0,
                    jump: false,
                    just_opened: true,
                });
            }
        }
    }

    fn step_match(&mut self, backward: bool) {
        let Some(active_text) = self.docs.get(self.active).map(|d| d.text.clone()) else { return };
        let Some(f) = &mut self.find else { return };
        let matches = find_matches(&active_text, &f.query, f.case_insensitive);
        if matches.is_empty() {
            return;
        }
        f.current = f.current.min(matches.len() - 1);
        f.current = if backward {
            if f.current == 0 { matches.len() - 1 } else { f.current - 1 }
        } else {
            (f.current + 1) % matches.len()
        };
        f.jump = true;
    }

    fn replace_current_match(&mut self) {
        let Some((query, replacement, ci, current)) =
            self.find.as_ref().map(|f| (f.query.clone(), f.replace.clone(), f.case_insensitive, f.current))
        else {
            return;
        };
        let active = self.active;
        if !self.editable(active) {
            self.status = self.readonly_reason(active);
            return;
        }
        let Some(d) = self.docs.get_mut(active) else { return };
        let matches = find_matches(&d.text, &query, ci);
        let Some(&(s, e)) = matches.get(current) else { return };
        let line = d.lines.line_of_byte(s);
        d.text.replace_range(s..e, &replacement);
        d.text_replaced();
        d.hl.note_edit(line);
        d.last_text = d.text.clone();
        d.dirty = true;
        d.version += 1;
        let (path, version, text) = (d.path.clone(), d.version, d.text.clone());
        if let Some(c) = d.server.as_ref().and_then(|s| self.servers.get_mut(s)) {
            c.did_change(&path, version, &text);
        }
        if let Some(f) = &mut self.find {
            // `current` now points at whatever followed the replaced match;
            // re-jumping clamps it into the (possibly shorter) new list.
            f.jump = true;
        }
        self.status = "Replaced 1 match".into();
    }

    fn replace_all_matches(&mut self) {
        let Some((query, replacement, ci)) =
            self.find.as_ref().map(|f| (f.query.clone(), f.replace.clone(), f.case_insensitive))
        else {
            return;
        };
        let active = self.active;
        if !self.editable(active) {
            self.status = self.readonly_reason(active);
            return;
        }
        let Some(d) = self.docs.get_mut(active) else { return };
        let (new_text, count) = replace_all(&d.text, &query, &replacement, ci);
        if count == 0 {
            self.status = "No matches".into();
            return;
        }
        d.hl.note_edit(highlight::first_changed_line(&d.text, &new_text));
        d.text = new_text;
        d.text_replaced();
        d.last_text = d.text.clone();
        d.dirty = true;
        d.version += 1;
        let (path, version, text) = (d.path.clone(), d.version, d.text.clone());
        if let Some(c) = d.server.as_ref().and_then(|s| self.servers.get_mut(s)) {
            c.did_change(&path, version, &text);
        }
        self.status = format!("Replaced {count} match(es)");
    }

    /// Renders the Ctrl+F / Ctrl+H bar and applies its buttons. No-op when
    /// the bar is closed. Reads and writes `self.find` through owned locals
    /// (rather than holding a `&mut FindState` across the render call) so
    /// the button handlers below can freely call other `&mut self` methods.
    fn show_find_bar(&mut self, ui: &mut egui::Ui, active: usize) {
        let Some((mut query, mut ci, mut replace, show_replace, mut current, just_opened)) = self
            .find
            .as_ref()
            .map(|f| (f.query.clone(), f.case_insensitive, f.replace.clone(), f.show_replace, f.current, f.just_opened))
        else {
            return;
        };
        // Switching to a read-only or virtualised tab with the bar already
        // open must drop the replace half too, not just refuse its buttons.
        let show_replace = show_replace && self.editable(active);
        let matches = find_matches(&self.docs[active].text, &query, ci);
        current = if matches.is_empty() { 0 } else { current.min(matches.len() - 1) };

        let mut close = false;
        let mut next = false;
        let mut prev = false;
        let mut do_replace = false;
        let mut do_replace_all = false;
        ui.horizontal(|ui| {
            let resp = ui.add(egui::TextEdit::singleline(&mut query).hint_text("Find").desired_width(180.0));
            if just_opened {
                resp.request_focus();
            }
            ui.checkbox(&mut ci, "Ignore case");
            ui.label(if matches.is_empty() { "0/0".to_string() } else { format!("{}/{}", current + 1, matches.len()) });
            if ui.small_button("Prev").clicked() {
                prev = true;
            }
            if ui.small_button("Next").clicked() {
                next = true;
            }
            if show_replace {
                ui.separator();
                ui.add(egui::TextEdit::singleline(&mut replace).hint_text("Replace with").desired_width(180.0));
                if ui.small_button("Replace").clicked() {
                    do_replace = true;
                }
                if ui.small_button("Replace All").clicked() {
                    do_replace_all = true;
                }
            }
            if ui.small_button(crate::icons::CLOSE).clicked() {
                close = true;
            }
        });

        if let Some(f) = &mut self.find {
            f.query = query;
            f.case_insensitive = ci;
            f.replace = replace;
            f.current = current;
            f.just_opened = false;
        }
        if close {
            self.find = None;
            return;
        }
        if next {
            self.step_match(false);
        }
        if prev {
            self.step_match(true);
        }
        if do_replace {
            self.replace_current_match();
        }
        if do_replace_all {
            self.replace_all_matches();
        }
    }

    /// Renders the Ctrl+G bar and jumps on Enter/Go. No-op when closed.
    fn show_goto_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, edit_id: egui::Id, active: usize) {
        let Some(mut buf) = self.goto_line.clone() else { return };
        let mut go = false;
        let mut close = false;
        ui.horizontal(|ui| {
            ui.label("Go to line:");
            let resp = ui.add(egui::TextEdit::singleline(&mut buf).desired_width(80.0));
            resp.request_focus();
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                go = true;
            }
            if ui.small_button("Go").clicked() {
                go = true;
            }
            if ui.small_button(crate::icons::CLOSE).clicked() {
                close = true;
            }
        });
        self.goto_line = Some(buf.clone());
        if go {
            match buf.trim().parse::<usize>() {
                Ok(line) if line >= 1 => {
                    let idx = self.docs[active].lines.line_char_start(line - 1);
                    let mut state = egui::TextEdit::load_state(ctx, edit_id).unwrap_or_default();
                    state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(idx))));
                    egui::TextEdit::store_state(ctx, edit_id, state);
                    self.pending_scroll_to = Some(idx);
                    self.cursor_char = idx;
                    self.status = format!("Line {line}");
                }
                _ => self.status = "Go to line: enter a line number".into(),
            }
            self.goto_line = None;
        } else if close {
            self.goto_line = None;
        }
    }

    /// Applies a find-bar jump requested this frame: moves the widget's
    /// persisted selection to the current match (so the render right after
    /// this call shows it) and records the char offset to scroll to once
    /// the galley for that render is available.
    fn apply_pending_find_jump(&mut self, ctx: &egui::Context, edit_id: egui::Id, active: usize) {
        let jump_needed = self.find.as_ref().map(|f| f.jump).unwrap_or(false);
        if !jump_needed {
            return;
        }
        let (query, ci, mut current) = {
            let f = self.find.as_mut().expect("checked above");
            f.jump = false;
            (f.query.clone(), f.case_insensitive, f.current)
        };
        let matches = find_matches(&self.docs[active].text, &query, ci);
        if matches.is_empty() {
            return;
        }
        current = current.min(matches.len() - 1);
        if let Some(f) = &mut self.find {
            f.current = current;
        }
        let (sb, eb) = matches[current];
        let d = &self.docs[active];
        let s = d.lines.byte_to_char(&d.text, sb);
        let e = d.lines.byte_to_char(&d.text, eb);
        let mut state = egui::TextEdit::load_state(ctx, edit_id).unwrap_or_default();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(s), egui::text::CCursor::new(e))));
        egui::TextEdit::store_state(ctx, edit_id, state);
        self.pending_scroll_to = Some(s);
        self.cursor_char = s;
    }

    fn show_close_confirm(&mut self, ctx: &egui::Context, i: usize, format_json_on_save: bool) {
        let Some(name) = self.docs.get(i).map(|d| d.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
        else {
            self.pending_close = None;
            return;
        };
        let mut save_it = false;
        let mut discard_it = false;
        let mut cancel_it = false;
        egui::Window::new("Unsaved changes")
            .id(egui::Id::new(("editor-close-confirm", i)))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(format!("\"{name}\" has unsaved changes."));
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        save_it = true;
                    }
                    if ui.button("Discard").clicked() {
                        discard_it = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel_it = true;
                    }
                });
            });
        if save_it {
            self.save_doc(i, format_json_on_save);
            self.close(i);
        } else if discard_it {
            self.close(i);
        } else if cancel_it {
            self.pending_close = None;
        }
    }

    fn show_conflict_bar(&mut self, ui: &mut egui::Ui, active: usize) {
        let name = self.docs[active].path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut reload = false;
        let mut keep = false;
        ui.horizontal(|ui| {
            ui.colored_label(Color32::from_rgb(0xe0, 0xaf, 0x68), format!("{name} changed on disk since it was opened."));
            if ui.button("Reload (discard mine)").clicked() {
                reload = true;
            }
            if ui.button("Keep mine").clicked() {
                keep = true;
            }
        });
        if reload {
            self.resolve_conflict(active, false);
        }
        if keep {
            self.resolve_conflict(active, true);
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, servers: &[ServerDef], format_json_on_save: bool) {
        let _ = servers;
        self.poll_servers();
        self.check_external_changes();
        let ctx = ui.ctx().clone();

        // ---- tab strip ----
        let mut close_requested: Option<usize> = None;
        ui.horizontal_wrapped(|ui| {
            for (i, d) in self.docs.iter().enumerate() {
                let name = d.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let label = if d.dirty { format!("{name} \u{2022}") } else { name };
                if ui.selectable_label(i == self.active, label).clicked() {
                    self.active = i;
                    self.completion = None;
                }
                if ui.small_button(crate::icons::CLOSE).clicked() {
                    close_requested = Some(i);
                }
                ui.separator();
            }
        });
        if let Some(i) = close_requested {
            if self.docs.get(i).map(|d| d.dirty).unwrap_or(false) {
                self.pending_close = Some(i);
            } else {
                self.close(i);
            }
        }
        if let Some(i) = self.pending_close {
            self.show_close_confirm(&ctx, i, format_json_on_save);
        }
        if self.docs.is_empty() {
            ui.centered_and_justified(|ui| ui.label("Open a file from the Explorer tab"));
            return;
        }
        let active = self.active;

        // ---- external-change bar ----
        if self.docs[active].disk_conflict {
            self.show_conflict_bar(ui, active);
        }

        // ---- preview toggle, word wrap, quiet notes ----
        let kind = preview_kind(&self.docs[active].path);
        ui.horizontal(|ui| {
            match kind {
                PreviewKind::Markdown => {
                    ui.selectable_value(&mut self.preview, false, "Source");
                    ui.selectable_value(&mut self.preview, true, "Preview");
                }
                PreviewKind::Html => {
                    // No web view is embedded, so an honest preview is the browser.
                    if ui.button("Open preview in browser").clicked() {
                        let p = self.docs[active].path.clone();
                        if self.docs[active].dirty {
                            self.status = "Save first: the browser reads the file on disk".into();
                        } else if let Err(e) = open::that_detached(&p) {
                            self.status = format!("Preview failed: {e}");
                        }
                    }
                }
                PreviewKind::None => {}
            }
            let large = self.is_large(active);
            ui.add_enabled(!large, egui::Checkbox::new(&mut self.word_wrap, "Wrap"))
                .on_disabled_hover_text("The large-file view puts one source line on one row, so there is nothing to wrap against.");
            if large {
                let mib = self.docs[active].text.len() as f64 / (1024.0 * 1024.0);
                ui.colored_label(
                    Color32::from_rgb(0xe0, 0xaf, 0x68),
                    format!("Read-only: {mib:.1} MiB, over the {} MiB editing limit", self.large_threshold / (1024 * 1024)),
                )
                .on_hover_text(
                    "Above the limit the editor lays out only the rows on screen, which is what keeps a file this size \
                     open at all. Find, go to line, diagnostics and hover work; typing, completion, replace and save do not.",
                );
                if self.large_line_clipped {
                    ui.small(egui::RichText::new(format!("lines cut at {LINE_COLUMN_CAP} columns")).weak())
                        .on_hover_text("A line on screen is longer than the view lays out. The text is all there in the file; only the drawing stops.");
                }
            }
            // A large file is no longer refused colour, it is coloured in
            // the background; say how far along that is and nothing else.
            if let Some(done) = self.docs[active].hl.progress() {
                ui.small(egui::RichText::new(format!("colouring {:.0}%", done * 100.0)).weak())
                    .on_hover_text("The first pass over a large file runs on a background thread; lines it has not reached yet are shown plain.");
            }
            if self.docs[active].read_only {
                ui.colored_label(Color32::from_rgb(0xe0, 0xaf, 0x68), "Read-only (not valid UTF-8)");
            }
            if let Some(note) = &self.lsp_note {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.small(egui::RichText::new(note).weak());
                });
            }
        });
        ui.separator();

        if self.preview && kind == PreviewKind::Markdown {
            if self.docs[active].text.len() > PREVIEW_MAX_BYTES {
                ui.label(format!(
                    "Preview is off for files over {} KiB; this one is {} KiB.",
                    PREVIEW_MAX_BYTES / 1024,
                    self.docs[active].text.len() / 1024
                ));
                return;
            }
            let text = self.docs[active].text.clone();
            egui::ScrollArea::vertical()
                .id_salt(("md-preview", active))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui_commonmark::CommonMarkViewer::new().show(ui, &mut self.md_cache, &text);
                });
            return;
        }

        // ---- keys the popup / find / goto own, consumed before the widget sees them ----
        let mut newline = false;
        let editor_focused = ctx.memory(|m| m.has_focus(ui.make_persistent_id(("editor", self.active))));
        let mut accept = false;
        let mut ctrl_space = false;
        let mut save = false;
        let mut format_now = false;
        let mut hover_req = false;
        let mut toggle_find = false;
        let mut toggle_replace = false;
        let mut find_escape = false;
        let mut toggle_goto = false;
        let mut find_next = false;
        let mut find_prev = false;
        ui.input_mut(|i| {
            if self.completion.is_some() {
                if i.consume_key(Modifiers::NONE, Key::Escape) {
                    self.completion = None;
                }
                if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                    if let Some(c) = &mut self.completion {
                        c.selected = (c.selected + 1).min(c.items.len().saturating_sub(1));
                    }
                }
                if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                    if let Some(c) = &mut self.completion {
                        c.selected = c.selected.saturating_sub(1);
                    }
                }
                if i.consume_key(Modifiers::NONE, Key::Enter) || i.consume_key(Modifiers::NONE, Key::Tab) {
                    accept = true;
                }
            } else if self.find.is_some() {
                if i.consume_key(Modifiers::NONE, Key::Escape) {
                    find_escape = true;
                }
                if i.consume_key(Modifiers::SHIFT, Key::Enter) {
                    find_prev = true;
                }
                if i.consume_key(Modifiers::NONE, Key::Enter) {
                    find_next = true;
                }
            }
            if i.consume_key(Modifiers::COMMAND, Key::Space) {
                ctrl_space = true;
            }
            if i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::F) {
                format_now = true;
            }
            if i.consume_key(Modifiers::COMMAND, Key::S) {
                save = true;
            }
            if i.consume_key(Modifiers::COMMAND, Key::K) {
                hover_req = true;
            }
            if i.consume_key(Modifiers::COMMAND, Key::F) {
                toggle_find = true;
            }
            if i.consume_key(Modifiers::COMMAND, Key::H) {
                toggle_replace = true;
            }
            if i.consume_key(Modifiers::COMMAND, Key::G) {
                toggle_goto = true;
            }
            // Auto-indent. egui inserts a bare newline and carries its own TODO
            // about this (`text_edit/builder.rs`, the Enter handler), so there
            // is no builder flag to turn on: the newline has to be written here
            // before the widget sees the key. Only when the text area itself
            // has focus, so Enter in the find bar or a dialog is untouched.
            if editor_focused && i.consume_key(Modifiers::NONE, Key::Enter) {
                newline = true;
            }
        });

        let edit_id = ui.make_persistent_id(("editor", active));
        if newline {
            self.insert_newline_with_indent(&ctx, edit_id);
        }
        if accept {
            self.accept_completion(&ctx, edit_id);
        }
        if format_now {
            self.format_current();
        }
        if save {
            self.save_active(format_json_on_save);
        }
        if toggle_find {
            self.open_find(false);
        }
        if toggle_replace {
            self.open_find(true);
        }
        if find_escape {
            self.find = None;
        }
        if toggle_goto {
            self.find = None;
            self.goto_line = Some(String::new());
        }
        if find_next {
            self.step_match(false);
        }
        if find_prev {
            self.step_match(true);
        }

        self.show_find_bar(ui, active);
        self.apply_pending_find_jump(&ctx, edit_id, active);
        self.show_goto_bar(ui, &ctx, edit_id, active);

        // ---- editor body ----
        let diag_ranges = {
            let d = &self.docs[active];
            diagnostic_ranges(&d.lines, &d.text, &d.diagnostics)
        };
        let find_matches_now: Vec<(usize, usize)> =
            self.find.as_ref().map(|f| find_matches(&self.docs[active].text, &f.query, f.case_insensitive)).unwrap_or_default();
        let find_current = self.find.as_ref().map(|f| f.current);
        // The highlighter is moved out of the document for the duration of
        // the editor body: the layouter closure needs it while `TextEdit` holds
        // `d.text` mutably, and two disjoint borrows of one `Doc` reached
        // through an index are not something the borrow checker can see. It
        // goes back below, once the closure has been dropped.
        let mut hl = std::mem::take(&mut self.docs[active].hl);
        hl.set_dark(ui.visuals().dark_mode);
        let hl_font = egui::TextStyle::Monospace.resolve(ui.style());
        let hl_plain = ui.visuals().text_color();
        // Re-parse from the dirty watermark within a per-frame budget. Work
        // left over means the document is not fully coloured yet, so ask for
        // another frame rather than stalling this one.
        // A document with a line syntect cannot parse in bounded time is left
        // uncoloured rather than parsed: `advance` is never called, so no
        // index is ever built, and every job below falls back to plain.
        if self.docs[active].highlightable() && hl.advance(&self.docs[active].text, highlight::Budget::frame()) {
            ctx.request_repaint();
        }
        // First line the widget changed this frame, handed to the highlighter
        // below once the layouter's borrow of it has ended.
        let mut edit_first_line: Option<usize> = None;

        let diag_h = if self.docs[active].diagnostics.is_empty() { 0.0 } else { 90.0 };
        let available_height = (ui.available_height() - diag_h).max(0.0);
        // A per-line gutter only stays aligned with the text when one source
        // line is exactly one visual row, i.e. with word wrap off. With wrap
        // on, a long line can spill onto several rows and the gutter would
        // drift out of sync with no honest fix short of reimplementing
        // TextEdit's line breaking, so it is hidden instead of shown wrong.
        let show_gutter = !self.word_wrap;
        let font_id = egui::TextStyle::Monospace.resolve(ui.style());
        let row_height = ui.fonts(|f| f.row_height(&font_id));
        let mut galley_pos = egui::Pos2::ZERO;
        let mut cursor_pos: Option<egui::Pos2> = None;
        let mut text_changed = false;

        if self.is_large(active) {
            self.show_large(
                ui,
                active,
                LargeView {
                    hl: &hl,
                    font_id: font_id.clone(),
                    plain: hl_plain,
                    row_height,
                    available_height,
                    diagnostics: &diag_ranges,
                    matches: &find_matches_now,
                    current_match: find_current,
                },
            );
        } else {
        let hl_ref = &hl;
        let mut layouter = |ui: &egui::Ui, text: &str, wrap_width: f32| {
            // `None` asks for the whole document: today's `TextEdit` lays out
            // every line. A virtualised view passes the visible line range.
            let mut job = hl_ref.layout_job(text, hl_font.clone(), hl_plain, None, wrap_width);
            underline_ranges(&mut job, &diag_ranges);
            highlight_find_matches(&mut job, &find_matches_now, find_current);
            ui.fonts(|f| f.layout_job(job))
        };

        egui::ScrollArea::vertical()
            .id_salt(("editor-vscroll", active))
            .auto_shrink([false, false])
            .max_height(available_height)
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    if show_gutter {
                        let line_count = self.docs[active].lines.line_count();
                        line_number_gutter(ui, line_count, row_height);
                    }
                    if self.word_wrap {
                        let desired_width = ui.available_width();
                        let d = &mut self.docs[active];
                        let out = egui::TextEdit::multiline(&mut d.text)
                            .id(edit_id)
                            .code_editor()
                            .interactive(!d.read_only)
                            .desired_width(desired_width)
                            .desired_rows(40)
                            .layouter(&mut layouter)
                            .show(ui);
                        if self.focus_next {
                            out.response.request_focus();
                            self.focus_next = false;
                        }
                        galley_pos = out.galley_pos;
                        if let Some(cr) = out.cursor_range {
                            self.cursor_char = cr.primary.ccursor.index;
                            let rect = out.galley.pos_from_cursor(&cr.primary);
                            cursor_pos = Some(galley_pos + rect.left_bottom().to_vec2());
                        }
                        let d = &mut self.docs[active];
                        if out.response.changed() {
                            text_changed = true;
                            d.dirty = true;
                            d.version += 1;
                            d.text_replaced();
                            // The widget reports no splice, so the first
                            // changed line comes from the common prefix.
                            edit_first_line = Some(highlight::first_changed_line(&d.last_text, &d.text));
                            d.last_text = d.text.clone();
                        }
                        if let Some(idx) = self.pending_scroll_to.take() {
                            let rect = out.galley.pos_from_ccursor(egui::text::CCursor::new(idx));
                            ui.scroll_to_rect(rect, Some(egui::Align::Center));
                        }
                        if let Some((pointer_pos, char_idx)) =
                            hover_dwell_fired(&out.response, &out.galley, galley_pos, &mut self.hover_watch, &mut self.hover_requested_for)
                        {
                            let d = &self.docs[active];
                            let (line, col) = d.lines.char_to_lsp(&d.text, char_idx);
                            let path = d.path.clone();
                            let server = d.server.clone();
                            if let Some(name) = server {
                                if let Some(c) = self.servers.get_mut(&name) {
                                    let id = c.hover(&path, line, col);
                                    self.hover = Some((id, String::new()));
                                    self.hover_anchor = Some(pointer_pos);
                                }
                            }
                        }
                    } else {
                        egui::ScrollArea::horizontal().id_salt(("editor-hscroll", active)).auto_shrink([false, false]).show(ui, |ui| {
                            let d = &mut self.docs[active];
                            let out = egui::TextEdit::multiline(&mut d.text)
                                .id(edit_id)
                                .code_editor()
                                .interactive(!d.read_only)
                                .desired_width(f32::INFINITY)
                                .desired_rows(40)
                                .layouter(&mut layouter)
                                .show(ui);
                            if self.focus_next {
                                out.response.request_focus();
                                self.focus_next = false;
                            }
                            galley_pos = out.galley_pos;
                            if let Some(cr) = out.cursor_range {
                                self.cursor_char = cr.primary.ccursor.index;
                                let rect = out.galley.pos_from_cursor(&cr.primary);
                                cursor_pos = Some(galley_pos + rect.left_bottom().to_vec2());
                            }
                            let d = &mut self.docs[active];
                            if out.response.changed() {
                                text_changed = true;
                                d.dirty = true;
                                d.version += 1;
                                d.text_replaced();
                                // Same as the wrapped branch above.
                                edit_first_line = Some(highlight::first_changed_line(&d.last_text, &d.text));
                                d.last_text = d.text.clone();
                            }
                            if let Some(idx) = self.pending_scroll_to.take() {
                                let rect = out.galley.pos_from_ccursor(egui::text::CCursor::new(idx));
                                ui.scroll_to_rect(rect, Some(egui::Align::Center));
                            }
                            if let Some((pointer_pos, char_idx)) =
                                hover_dwell_fired(&out.response, &out.galley, galley_pos, &mut self.hover_watch, &mut self.hover_requested_for)
                            {
                                let d = &self.docs[active];
                                let (line, col) = d.lines.char_to_lsp(&d.text, char_idx);
                                let path = d.path.clone();
                                let server = d.server.clone();
                                if let Some(name) = server {
                                    if let Some(c) = self.servers.get_mut(&name) {
                                        let id = c.hover(&path, line, col);
                                        self.hover = Some((id, String::new()));
                                        self.hover_anchor = Some(pointer_pos);
                                    }
                                }
                            }
                        });
                    }
                });
            });
        } // end of the editable `TextEdit` path

        if let Some(line) = edit_first_line {
            hl.note_edit(line);
        }
        if let Some(d) = self.docs.get_mut(active) {
            d.hl = hl;
        }

        // Built-in JSON check for documents with no language server.
        if (text_changed || self.docs[active].version == 0) && self.docs[active].server.is_none() && is_json(&self.docs[active].path) {
            let d = &mut self.docs[active];
            d.diagnostics = json_diagnostics(&d.text);
        }

        // ---- sync + completion triggers ----
        if text_changed {
            let d = &self.docs[active];
            let (path, version, text) = (d.path.clone(), d.version, d.text.clone());
            if let Some(c) = d.server.as_ref().and_then(|s| self.servers.get_mut(s)) {
                c.did_change(&path, version, &text);
            }
            let prev = self.cursor_char.checked_sub(1).map(|i| {
                let b = d.lines.char_to_byte(&text, i);
                text[b..].chars().next()
            }).flatten();
            let trigger = matches!(prev, Some('.') | Some(':'));
            let ident = prev.map(|c| c.is_alphanumeric() || c == '_').unwrap_or(false);
            if trigger || (ident && (self.completion.is_some() || self.prefix_len() >= 2)) {
                self.request_completion();
            } else if !ident {
                self.completion = None;
            }
        }
        if ctrl_space {
            self.request_completion();
        }
        if hover_req {
            let d = &self.docs[active];
            let (line, col) = d.lines.char_to_lsp(&d.text, self.cursor_char);
            let path = d.path.clone();
            if let Some(c) = d.server.as_ref().and_then(|s| self.servers.get_mut(s)) {
                let id = c.hover(&path, line, col);
                self.hover = Some((id, String::new()));
                self.hover_anchor = cursor_pos;
            }
        }

        // ---- completion popup ----
        if let (Some(comp), Some(pos)) = (&mut self.completion, cursor_pos) {
            let prefix: String = {
                let d = &self.docs[active];
                let s = comp.prefix_start.min(d.lines.char_count());
                let e = self.cursor_char.min(d.lines.char_count()).max(s);
                let sb = d.lines.char_to_byte(&d.text, s);
                let eb = d.lines.char_to_byte(&d.text, e);
                d.text[sb..eb].to_lowercase()
            };
            let visible: Vec<usize> = comp
                .items
                .iter()
                .enumerate()
                .filter(|(_, it)| prefix.is_empty() || it.label.to_lowercase().contains(&prefix))
                .map(|(i, _)| i)
                .take(12)
                .collect();
            if visible.is_empty() {
                self.completion = None;
            } else {
                if !visible.contains(&comp.selected) {
                    comp.selected = visible[0];
                }
                let mut clicked: Option<usize> = None;
                egui::Area::new(ui.id().with("completion"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(pos + egui::vec2(0.0, 2.0))
                    .show(&ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.set_min_width(260.0);
                            for i in visible {
                                let it = &comp.items[i];
                                let text = if it.detail.is_empty() {
                                    format!("{} {}", kind_glyph(it.kind), it.label)
                                } else {
                                    format!("{} {}    {}", kind_glyph(it.kind), it.label, it.detail)
                                };
                                if ui.selectable_label(i == comp.selected, text).clicked() {
                                    clicked = Some(i);
                                }
                            }
                        });
                    });
                if let Some(i) = clicked {
                    comp.selected = i;
                    self.accept_completion(&ctx, edit_id);
                }
            }
        }

        // ---- hover tooltip ----
        if let (Some((_, text)), Some(pos)) = (&self.hover, self.hover_anchor) {
            if !text.is_empty() {
                egui::Area::new(ui.id().with("hover"))
                    .order(egui::Order::Tooltip)
                    .fixed_pos(pos + egui::vec2(0.0, 2.0))
                    .show(&ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.set_max_width(520.0);
                            ui.label(egui::RichText::new(text).monospace().small());
                        });
                    });
                if ui.input(|i| i.pointer.any_pressed() || i.events.iter().any(|e| matches!(e, egui::Event::Text(_)))) {
                    self.hover = None;
                    self.hover_anchor = None;
                }
            }
        }

        // ---- diagnostics strip ----
        if diag_h > 0.0 {
            ui.separator();
            egui::ScrollArea::vertical().max_height(diag_h - 8.0).show(ui, |ui| {
                for d in &self.docs[active].diagnostics {
                    let color = match d.severity {
                        1 => Color32::from_rgb(0xf7, 0x76, 0x8e),
                        2 => Color32::from_rgb(0xe0, 0xaf, 0x68),
                        _ => ui.visuals().weak_text_color(),
                    };
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(format!("{}:{}", d.line + 1, d.col_start + 1)).color(color).small());
                        ui.add(egui::Label::new(egui::RichText::new(&d.message).small()).wrap());
                    });
                }
            });
        }
        if self.completion.is_some() || self.pending_completion.is_some() || self.hover.is_some() || self.hover_watch.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        } else if !self.servers.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }

    /// The virtualised read-only view used at or above [`LARGE_DOC_BYTES`].
    ///
    /// One `ScrollArea::show_rows` row per source line, one `LayoutJob` per
    /// visible line, painted straight into the scroll area's painter. Nothing
    /// off screen is laid out, so memory is a function of the window rather
    /// than of the file: the same 5.8 MB document that costs a `TextEdit`
    /// 1 GB of galley costs this about a megabyte.
    ///
    /// What survives from the editable path: the gutter, find highlighting
    /// and jumps, go to line, diagnostic underlines and hover on dwell, all
    /// recomputed per visible line. What does not: the cursor, selection,
    /// typing, completion, auto-indent and replace, which all live in
    /// `TextEdit` and are refused above the threshold rather than faked.
    fn show_large(&mut self, ui: &mut egui::Ui, active: usize, v: LargeView<'_>) {
        // Nothing in here takes keyboard focus, so a focus request left over
        // from opening the file has to be dropped or it outlives the tab.
        self.focus_next = false;
        self.completion = None;

        let row_height = v.row_height;
        let line_count = self.docs[active].lines.line_count();
        let digits = line_count.to_string().len().max(3);
        let char_w = ui.fonts(|f| f.glyph_width(&v.font_id, '0'));
        let gutter_w = digits as f32 * char_w + 10.0;

        // Scroll targets are resolved to an offset before the area is built:
        // the row a find jump or Ctrl+End wants is usually not laid out this
        // frame, so there is no rect to call `scroll_to_rect` with.
        let page = (v.available_height - row_height).max(row_height);
        let max_offset = (line_count as f32 * row_height - v.available_height).max(0.0);
        let mut offset: Option<f32> = None;
        ui.input_mut(|i| {
            if i.consume_key(Modifiers::COMMAND, Key::Home) {
                offset = Some(0.0);
            }
            if i.consume_key(Modifiers::COMMAND, Key::End) {
                offset = Some(max_offset);
            }
            if i.consume_key(Modifiers::NONE, Key::PageUp) {
                offset = Some((self.large_offset - page).max(0.0));
            }
            if i.consume_key(Modifiers::NONE, Key::PageDown) {
                offset = Some((self.large_offset + page).min(max_offset));
            }
        });
        if let Some(idx) = self.pending_scroll_to.take() {
            let line = self.docs[active].lines.line_of_char(idx);
            // A third of a screen above the hit rather than centred: what
            // follows a match is usually what the reader wants with it.
            offset = Some((line as f32 * row_height - v.available_height / 3.0).clamp(0.0, max_offset));
        }

        let (new_offset, rows) = ui
            .scope(|ui| {
                // `show_rows` measures in rows of `row_height + item_spacing.y`.
                // Zeroing the spacing makes one row exactly one text row, which
                // is what keeps the gutter aligned with the text and the
                // y-to-line arithmetic below exact.
                ui.spacing_mut().item_spacing.y = 0.0;
                let mut area = egui::ScrollArea::vertical()
                    .id_salt(("editor-large", active))
                    .auto_shrink([false, false])
                    .max_height(v.available_height);
                if let Some(o) = offset {
                    area = area.vertical_scroll_offset(o);
                }
                let out = area.show_rows(ui, row_height, line_count, |ui, range| {
                    self.paint_large_rows(ui, active, &v, range, gutter_w, line_count)
                });
                (out.state.offset.y, out.inner)
            })
            .inner;
        self.large_offset = new_offset;
        self.large_line_clipped = rows.clipped;

        // Hover on dwell. The row came from the pointer's y against the row
        // height; the column comes from that row's own galley, which is the
        // only galley there is to ask.
        if let Some((galley, pos, row)) = rows.hovered {
            if let Some((pointer, char_in_line)) = hover_dwell_fired(
                &rows.response,
                &galley,
                pos,
                &mut self.hover_watch,
                &mut self.hover_requested_for,
            ) {
                let d = &self.docs[active];
                let raw = &d.text[d.lines.line_bytes(row)];
                let col: u32 = raw.chars().take(char_in_line).map(|c| c.len_utf16() as u32).sum();
                let path = d.path.clone();
                let server = d.server.clone();
                if let Some(name) = server {
                    if let Some(c) = self.servers.get_mut(&name) {
                        let id = c.hover(&path, row as u32, col);
                        self.hover = Some((id, String::new()));
                        self.hover_anchor = Some(pointer);
                    }
                }
            }
        }
    }

    /// Draws the gutter and the text for the rows `show_rows` asked for.
    ///
    /// Split out of [`show_large`](Self::show_large) so the borrow of `self`
    /// inside the scroll-area closure ends before the hover handling, which
    /// needs `self.servers` mutably.
    fn paint_large_rows(
        &self,
        ui: &mut egui::Ui,
        active: usize,
        v: &LargeView<'_>,
        range: std::ops::Range<usize>,
        gutter_w: f32,
        line_count: usize,
    ) -> LargeRows {
        let row_height = v.row_height;
        let end = range.end.min(line_count);
        let shown = end.saturating_sub(range.start);
        let width = ui.available_width().max(gutter_w + 32.0);
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(width, shown as f32 * row_height), egui::Sense::hover());
        let mut out = LargeRows { response: response.clone(), hovered: None, clipped: false };
        if shown == 0 {
            return out;
        }
        let painter = ui.painter().clone();
        let weak = ui.visuals().weak_text_color();

        // One galley for every line number on screen, the same trick the
        // small path's gutter uses; a label per row was 125 MB at 90k lines.
        let mut numbers = String::with_capacity(shown * 8);
        for l in range.start..end {
            numbers.push_str(&(l + 1).to_string());
            numbers.push('\n');
        }
        let gutter = ui.fonts(|f| f.layout(numbers, v.font_id.clone(), weak, f32::INFINITY));
        painter.galley(egui::pos2(rect.left() + 4.0, rect.top()), gutter, weak);

        let text_x = rect.left() + gutter_w;
        let hovered_row = response.hover_pos().and_then(|p| {
            let r = ((p.y - rect.top()) / row_height).floor();
            (r >= 0.0).then(|| range.start + r as usize).filter(|r| *r < end)
        });

        let d = &self.docs[active];
        // Without a live index the highlighter cannot slice a line range and
        // would hand back the whole document, so those rows are laid out here.
        let indexed = v.hl.indexed_for(&d.text);
        for row in range.start..end {
            let br = d.lines.line_bytes(row);
            let raw = &d.text[br.start..br.end];
            // The newline is a row break inside a galley and every row here is
            // its own galley, so including it would double each line's height.
            let body = raw.strip_suffix('\n').unwrap_or(raw);
            let body = body.strip_suffix('\r').unwrap_or(body);
            let mut keep = body.len();
            if keep > LINE_COLUMN_CAP {
                keep = LINE_COLUMN_CAP;
                while keep > 0 && !body.is_char_boundary(keep) {
                    keep -= 1;
                }
                out.clipped = true;
            }
            let mut job = if indexed {
                v.hl.layout_job_capped(
                    &d.text,
                    v.font_id.clone(),
                    v.plain,
                    Some(row..row + 1),
                    f32::INFINITY,
                    keep,
                )
            } else {
                LayoutJob::simple(body[..keep].to_owned(), v.font_id.clone(), v.plain, f32::INFINITY)
            };

            // Diagnostics and find matches carry whole-document byte offsets;
            // this job's offsets start at the line, so both are rebased and
            // clipped to it before they are applied.
            let b0 = br.start;
            let diag: Vec<(usize, usize, Color32)> = v
                .diagnostics
                .iter()
                .filter(|(s, e, _)| *e > b0 && *s < b0 + keep)
                .map(|(s, e, c)| (s.saturating_sub(b0), (e - b0).min(keep), *c))
                .filter(|(s, e, _)| e > s)
                .collect();
            underline_ranges(&mut job, &diag);

            // Matches are sorted and non-overlapping, so the first one that
            // can touch this line is a binary search rather than a scan of
            // every hit in the document, once per row, per frame.
            let first = v.matches.partition_point(|(_, e)| *e <= b0);
            let mut local: Vec<(usize, usize)> = Vec::new();
            let mut k = first;
            while k < v.matches.len() && v.matches[k].0 < b0 + keep {
                let (s, e) = v.matches[k];
                let (ls, le) = (s.saturating_sub(b0), (e - b0).min(keep));
                if le > ls {
                    local.push((ls, le));
                }
                k += 1;
            }
            let current = v.current_match.and_then(|c| c.checked_sub(first)).filter(|c| *c < local.len());
            highlight_find_matches(&mut job, &local, current);

            let galley = ui.fonts(|f| f.layout_job(job));
            let pos = egui::pos2(text_x, rect.top() + (row - range.start) as f32 * row_height);
            if hovered_row == Some(row) {
                out.hovered = Some((galley.clone(), pos, row));
            }
            painter.galley(pos, galley, v.plain);
        }
        out
    }

    fn prefix_len(&self) -> usize {
        self.cursor_char.saturating_sub(self.prefix_start())
    }
}

/// Everything [`Editor::show_large`] needs that `Editor::show` already worked
/// out for the frame. Grouped rather than passed loose because the
/// highlighter has been moved out of the document by then and has to travel
/// as a borrow alongside the rest.
struct LargeView<'a> {
    hl: &'a highlight::DocHighlight,
    font_id: egui::FontId,
    plain: Color32,
    row_height: f32,
    available_height: f32,
    /// Diagnostic spans as whole-document byte ranges with their colour.
    diagnostics: &'a [(usize, usize, Color32)],
    /// Find hits as whole-document byte ranges, ascending and disjoint.
    matches: &'a [(usize, usize)],
    /// Index into `matches` of the hit the find bar is sitting on.
    current_match: Option<usize>,
}

/// What painting a screen of rows leaves for the caller to act on.
struct LargeRows {
    /// The whole row block, for pointer tests.
    response: egui::Response,
    /// The galley under the pointer with its origin and source line, so a
    /// dwell can be turned into a column without a document-wide galley.
    hovered: Option<(std::sync::Arc<egui::Galley>, egui::Pos2, usize)>,
    /// A visible line was longer than [`LINE_COLUMN_CAP`].
    clipped: bool,
}

/// Tracks how long the pointer has rested over `response` in roughly the
/// same spot and, once that passes `HOVER_DWELL_MS`, returns the screen
/// position and the char index under it so the caller can fire a hover
/// request there. Returns `None` otherwise, including once already fired for
/// that exact spot (tracked via `requested_for`), so holding still after the
/// reply arrives does not keep re-requesting.
///
/// A free function taking `&mut` pieces of `Editor`'s hover state, rather
/// than an `&mut self` method, on purpose: at both call sites in
/// `Editor::show` a `Doc` is still borrowed out of `self.docs` for the
/// dirty-check that runs just before this, and an `&mut self` method call
/// there would conflict with that borrow. Splitting the borrow at the
/// call site (`&mut self.hover_watch`, `&mut self.hover_requested_for`)
/// keeps this function's needs disjoint from `self.docs`.
fn hover_dwell_fired(
    response: &egui::Response,
    galley: &egui::Galley,
    galley_pos: egui::Pos2,
    watch: &mut Option<(egui::Pos2, Instant)>,
    requested_for: &mut Option<egui::Pos2>,
) -> Option<(egui::Pos2, usize)> {
    let pos = response.hover_pos()?;
    let same_spot = watch.map(|(p, _)| p.distance(pos) < 3.0).unwrap_or(false);
    if !same_spot {
        *watch = Some((pos, Instant::now()));
        *requested_for = None;
        return None;
    }
    let (_, since) = (*watch)?;
    if since.elapsed() < Duration::from_millis(HOVER_DWELL_MS) || *requested_for == Some(pos) {
        return None;
    }
    *requested_for = Some(pos);
    let ccursor = galley.cursor_from_pos(pos - galley_pos);
    Some((pos, ccursor.ccursor.index))
}

/// Line-number gutter. Only shown with word wrap off; see the caveat where
/// `show_gutter` is computed in `Editor::show`.
///
/// Reserves the full height so the scroll range matches the text, but lays
/// out one galley holding only the rows inside the clip rect. The previous
/// version added a label widget per source line every frame, and egui kept
/// every one of those galleys alive, which on a 90 000 line file was about
/// 125 MB for numbers nobody could see.
fn line_number_gutter(ui: &mut egui::Ui, line_count: usize, row_height: f32) {
    let width = (line_count.max(1).to_string().len().max(3) as f32) * 7.5 + 10.0;
    let total = egui::vec2(width, line_count as f32 * row_height);
    let (rect, _) = ui.allocate_exact_size(total, egui::Sense::hover());
    let clip = ui.clip_rect();
    let first = ((clip.top() - rect.top()) / row_height).floor().max(0.0) as usize;
    let last = ((((clip.bottom() - rect.top()) / row_height).ceil().max(0.0) as usize) + 1).min(line_count);
    if first >= last {
        return;
    }
    let mut numbers = String::with_capacity((last - first) * 8);
    for line in first + 1..=last {
        numbers.push_str(&line.to_string());
        numbers.push('\n');
    }
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let color = ui.visuals().weak_text_color();
    let galley = ui.fonts(|f| f.layout(numbers, font, color, f32::INFINITY));
    let pos = egui::pos2(rect.left() + 5.0, rect.top() + first as f32 * row_height);
    ui.painter().galley(pos, galley, color);
}

/// Walk up from the file looking for a project marker the server cares about
/// (Cargo.toml, package.json, pyproject.toml, go.mod, .git).
fn find_project_root(file: &Path, def: &ServerDef) -> Option<PathBuf> {
    let markers: &[&str] = match def.language_id.as_str() {
        "rust" => &["Cargo.toml"],
        "typescript" => &["package.json", "tsconfig.json"],
        "python" => &["pyproject.toml", "setup.py", "requirements.txt"],
        "go" => &["go.mod"],
        _ => &[],
    };
    let mut dir = file.parent()?;
    let mut fallback: Option<PathBuf> = None;
    loop {
        if markers.iter().any(|m| dir.join(m).exists()) {
            return Some(dir.to_path_buf());
        }
        if fallback.is_none() && dir.join(".git").exists() {
            fallback = Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
        if dir.parent().is_none() {
            return fallback;
        }
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    a == b || a.to_string_lossy().to_lowercase().replace('/', "\\") == b.to_string_lossy().to_lowercase().replace('/', "\\")
}

/// Char index -> (line, UTF-16 column).
/// Applies LSP `TextEdit`s to `text` in place. LSP formatting edits describe
/// positions against the *original* document, so they are applied back to
/// front: once the first (latest) splice happens, byte offsets computed
/// against the original text for anything before it are still valid,
/// whereas applying front to back would shift every later edit's offsets.
fn apply_text_edits(text: &mut String, edits: &[lsp::TextEdit]) {
    let mut ordered: Vec<&lsp::TextEdit> = edits.iter().collect();
    ordered.sort_by(|a, b| (b.start_line, b.start_col).cmp(&(a.start_line, a.start_col)));
    // Back-to-front application keeps every earlier offset valid, so one
    // index built from the original text serves every edit.
    let ix = LineIndex::build(text);
    for e in ordered {
        let s = ix.lsp_to_byte(text, e.start_line, e.start_col);
        let mut end = ix.lsp_to_byte(text, e.end_line, e.end_col);
        if end < s {
            end = s;
        }
        text.replace_range(s..end, &e.new_text);
    }
}

/// Diagnostic spans as byte ranges with their severity colour. Computed once
/// per frame from the line index; the layouter only applies them.
fn diagnostic_ranges(lines: &LineIndex, text: &str, diags: &[Diagnostic]) -> Vec<(usize, usize, Color32)> {
    diags
        .iter()
        .map(|d| {
            let s = lines.lsp_to_byte(text, d.line, d.col_start);
            let mut e = lines.lsp_to_byte(text, d.line_end, d.col_end);
            if e <= s {
                e = (s + 1).min(text.len());
            }
            let color = match d.severity {
                1 => Color32::from_rgb(0xf7, 0x76, 0x8e),
                2 => Color32::from_rgb(0xe0, 0xaf, 0x68),
                _ => Color32::from_rgb(0x7d, 0xcf, 0xff),
            };
            (s, e, color)
        })
        .collect()
}

/// Add a coloured underline to every range by splitting sections.
fn underline_ranges(job: &mut LayoutJob, ranges: &[(usize, usize, Color32)]) {
    apply_ranges(job, ranges, |fmt, color| fmt.underline = egui::Stroke::new(1.5_f32, color));
}

/// Background-highlights every find match, with the current one a stronger
/// colour. Shares the same section-splitting machinery as diagnostics, so
/// both can land on the same job without one clobbering the other.
fn highlight_find_matches(job: &mut LayoutJob, matches: &[(usize, usize)], current: Option<usize>) {
    if matches.is_empty() {
        return;
    }
    let ranges: Vec<(usize, usize, Color32)> = matches
        .iter()
        .enumerate()
        .map(|(i, (s, e))| {
            let color = if Some(i) == current {
                Color32::from_rgba_unmultiplied(0xff, 0xa5, 0x00, 140)
            } else {
                Color32::from_rgba_unmultiplied(0xff, 0xff, 0x00, 70)
            };
            (*s, *e, color)
        })
        .collect();
    apply_ranges(job, &ranges, |fmt, color| fmt.background = color);
}

/// Splits a `LayoutJob`'s sections at every range boundary and applies
/// `style` to the parts that fall inside a range. Ranges must be given as
/// byte offsets into the same text the job was built from.
fn apply_ranges(job: &mut LayoutJob, ranges: &[(usize, usize, Color32)], style: impl Fn(&mut egui::text::TextFormat, Color32)) {
    if ranges.is_empty() {
        return;
    }
    let old = std::mem::take(&mut job.sections);
    for sec in old {
        let (a, b) = (sec.byte_range.start, sec.byte_range.end);
        let mut cuts: Vec<usize> = vec![a, b];
        for (s, e, _) in ranges {
            if *s > a && *s < b {
                cuts.push(*s);
            }
            if *e > a && *e < b {
                cuts.push(*e);
            }
        }
        cuts.sort_unstable();
        cuts.dedup();
        for w in cuts.windows(2) {
            let (s, e) = (w[0], w[1]);
            let mut part = sec.clone();
            part.byte_range = s..e;
            if let Some((_, _, color)) = ranges.iter().find(|(rs, re, _)| s >= *rs && e <= *re) {
                style(&mut part.format, *color);
            }
            job.sections.push(part);
        }
    }
}

/// LSP CompletionItemKind to a short glyph.
fn kind_glyph(kind: u32) -> &'static str {
    match kind {
        2 | 3 => "\u{192}",             // method, function
        4 => "\u{2295}",                // constructor
        5 | 10 => "\u{25aa}",           // field, property
        6 => "\u{1d465}",               // variable
        7 | 8 | 22 | 23 => "\u{25c7}",  // class, interface, struct, event
        9 => "\u{25a4}",                // module
        13 | 20 => "\u{1d53c}",         // enum, enum member
        14 => "\u{2318}",               // keyword
        15 => "\u{2702}",               // snippet
        21 => "\u{3c0}",                // constant
        _ => "\u{b7}",
    }
}

fn is_json(path: &Path) -> bool {
    matches!(path.extension().map(|e| e.to_string_lossy().to_lowercase()).as_deref(), Some("json") | Some("jsonc"))
}

/// Built-in JSON syntax check used when no language server handles .json.
fn json_diagnostics(text: &str) -> Vec<Diagnostic> {
    let (_, text) = strip_bom(text);
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(_) => Vec::new(),
        Err(e) => {
            let line = (e.line().max(1) - 1) as u32;
            let col = (e.column().max(1) - 1) as u32;
            vec![Diagnostic { line, col_start: col, line_end: line, col_end: col + 1, severity: 1, message: e.to_string() }]
        }
    }
}

/// Whether a file this size should be refused at open.
fn exceeds_cap(size: u64) -> bool {
    size > MAX_OPEN_BYTES
}

/// Decode raw bytes as UTF-8, falling back to a lossy decode when they are
/// not. The lossy path is marked so the caller can make the document
/// read-only: a lossy round-trip does not reproduce the original bytes, so
/// saving it back out would corrupt whatever the invalid sequences were.
fn decode_bytes(bytes: &[u8]) -> (String, bool) {
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), false),
        Err(_) => (String::from_utf8_lossy(bytes).into_owned(), true),
    }
}

/// Splits a leading UTF-8 BOM off `text` if present. Both `json_diagnostics`
/// and the JSON formatter otherwise choke on it (serde_json does not treat
/// U+FEFF as insignificant whitespace), reporting a permanent error at 1:1
/// on an otherwise-valid file. Returns `(had_bom, rest)`.
fn strip_bom(text: &str) -> (bool, &str) {
    match text.strip_prefix('\u{feff}') {
        Some(rest) => (true, rest),
        None => (false, text),
    }
}

/// Reads and decodes a file the way the editor opens or reloads one: UTF-8
/// (or a marked-read-only lossy fallback), with any BOM split off. Does not
/// enforce the size cap; callers that need it check `fs::metadata` first,
/// since the cap decision has to happen before committing to reading the
/// whole file.
fn load_file(path: &Path) -> Result<(String, bool, bool), String> {
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    let (decoded, read_only) = decode_bytes(&bytes);
    let (has_bom, body) = strip_bom(&decoded);
    Ok((body.to_string(), has_bom, read_only))
}

/// True when a document's on-disk mtime or size no longer matches the
/// baseline recorded at open/save.
fn disk_changed(old_mtime: Option<SystemTime>, old_size: u64, new_mtime: Option<SystemTime>, new_size: u64) -> bool {
    old_size != new_size || old_mtime != new_mtime
}

/// Every non-overlapping occurrence of `query` in `text`, as char-index
/// ranges (`[start, end)`), scanned left to right. Case sensitivity folds
/// each character to its first lowercase char (not full Unicode
/// case-folding, which can expand one char to several and would break the
/// 1:1 index mapping this function promises); good enough for source text.
#[cfg(test)]
mod tests {

    #[test]
    fn newline_indent_matches_the_current_line() {
        let text: Vec<char> = "fn a() {\n    let x = 1;".chars().collect();
        assert_eq!(newline_indent(&text), "\n    ");
    }

    #[test]
    fn newline_indent_adds_a_level_after_an_opening_brace() {
        let text: Vec<char> = "    fn a() {".chars().collect();
        assert_eq!(newline_indent(&text), "\n        ");
    }

    #[test]
    fn newline_indent_keeps_tabs_when_the_file_uses_tabs() {
        let text: Vec<char> = "\tif x {".chars().collect();
        assert_eq!(newline_indent(&text), "\n\t\t");
    }

    #[test]
    fn newline_indent_on_an_empty_first_line_is_a_bare_newline() {
        assert_eq!(newline_indent(&[]), "\n");
    }

    use super::*;

    #[test]
    fn json_diagnostics_flags_trailing_comma_at_right_spot() {
        let d = json_diagnostics("{\n  \"a\": 1,,\n}");
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].line, d[0].col_start), (1, 9));
        assert!(!d[0].message.is_empty());
        assert!(json_diagnostics("{\"ok\": [1, 2]}").is_empty());
    }

    #[test]
    fn json_diagnostics_strips_bom() {
        let with_bom = "\u{feff}{\"ok\": true}";
        assert!(json_diagnostics(with_bom).is_empty(), "a BOM-prefixed valid JSON file must not error");
    }

    #[test]
    fn pretty_print_round_trips_and_ends_with_newline() {
        let v: serde_json::Value = serde_json::from_str("{\"b\":[1,{\"c\":true}],\"a\":\"x\"}").unwrap();
        let mut out = serde_json::to_string_pretty(&v).unwrap();
        out.push('\n');
        // preserve_order keeps the file's own key order; formatting must not reorder keys.
        assert!(out.starts_with("{\n  \"b\": ["), "{out}");
        assert!(out.ends_with("}\n"));
    }

    #[test]
    fn strip_bom_reports_presence_and_removes_it() {
        assert_eq!(strip_bom("\u{feff}hello"), (true, "hello"));
        assert_eq!(strip_bom("hello"), (false, "hello"));
        assert_eq!(strip_bom(""), (false, ""));
    }

    #[test]
    fn decode_bytes_prefers_utf8_and_falls_back_lossy() {
        let (s, ro) = decode_bytes("hello \u{2713}".as_bytes());
        assert_eq!(s, "hello \u{2713}");
        assert!(!ro);
        // 0xFF is never valid as the start of a UTF-8 sequence.
        let (s2, ro2) = decode_bytes(&[b'h', b'i', 0xFF, b'!']);
        assert!(ro2);
        assert!(s2.contains('\u{fffd}'), "lossy decode should substitute the replacement character");
    }

    #[test]
    fn exceeds_cap_is_strict_greater_than() {
        assert!(!exceeds_cap(MAX_OPEN_BYTES));
        assert!(exceeds_cap(MAX_OPEN_BYTES + 1));
        assert!(!exceeds_cap(0));
    }

    #[test]
    fn disk_changed_compares_both_mtime_and_size() {
        use std::time::Duration as D;
        let t0 = SystemTime::UNIX_EPOCH;
        let t1 = t0 + D::from_secs(1);
        assert!(!disk_changed(Some(t0), 10, Some(t0), 10));
        assert!(disk_changed(Some(t0), 10, Some(t1), 10), "mtime differs");
        assert!(disk_changed(Some(t0), 10, Some(t0), 11), "size differs");
        assert!(disk_changed(None, 0, Some(t0), 0), "mtime unavailable before, available after");
        assert!(!disk_changed(None, 5, None, 5));
    }

    #[test]
    fn apply_text_edits_applies_back_to_front() {
        let mut text = "hello world".to_string();
        // Replace "world" (line 0, col 6..11) and "hello" (col 0..5) in the
        // same batch, given in document order like a real server would send
        // them, to prove the back-to-front ordering inside the function
        // (not the caller) is what keeps both offsets valid.
        let edits = vec![
            lsp::TextEdit { start_line: 0, start_col: 0, end_line: 0, end_col: 5, new_text: "goodbye".into() },
            lsp::TextEdit { start_line: 0, start_col: 6, end_line: 0, end_col: 11, new_text: "there".into() },
        ];
        apply_text_edits(&mut text, &edits);
        assert_eq!(text, "goodbye there");
    }

}

/// The text a newline should insert, given everything before the caret.
///
/// Matches the current line's leading whitespace, and adds one more level
/// after a line whose last non-space character opens a block. The extra level
/// copies the indent character already in use so a tab file stays tabs and a
/// space file stays spaces, since there is no width setting to consult.
fn newline_indent(before: &[char]) -> String {
    let line_start = before.iter().rposition(|c| *c == '\n').map(|i| i + 1).unwrap_or(0);
    let line = &before[line_start..];
    let indent: String = line.iter().take_while(|c| **c == ' ' || **c == '\t').collect();
    let opens = line
        .iter()
        .rev()
        .find(|c| !c.is_whitespace())
        .map(|c| matches!(c, '{' | '[' | '(' | ':'))
        .unwrap_or(false);
    let mut out = String::from("\n");
    out.push_str(&indent);
    if opens {
        if indent.contains('\t') {
            out.push('\t');
        } else {
            out.push_str("    ");
        }
    }
    out
}

/// What the Preview control offers for a given file.
#[derive(Clone, Copy, PartialEq)]
enum PreviewKind {
    Markdown,
    Html,
    None,
}

fn preview_kind(path: &Path) -> PreviewKind {
    match path.extension().map(|e| e.to_string_lossy().to_lowercase()).as_deref() {
        Some("md") | Some("mdx") | Some("markdown") => PreviewKind::Markdown,
        Some("html") | Some("htm") => PreviewKind::Html,
        _ => PreviewKind::None,
    }
}
