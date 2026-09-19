//! IDE tab: open-file strip, syntax-highlighted text editor, LSP-backed
//! completion popup and diagnostics. One `LspClient` is kept per server
//! command and shared by every open document that maps to it.
//!
//! Cursor positions flow in three forms: egui char index (chars from start),
//! LSP (zero-based line, UTF-16 column) and byte offsets for slicing. The
//! helpers at the bottom convert between them.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use eframe::egui::{self, text::LayoutJob, Color32, Key, Modifiers};

use crate::lsp::{self, CompletionItem, Diagnostic, LspClient, LspEvent, ServerDef};

pub struct Doc {
    pub path: PathBuf,
    pub text: String,
    /// Copy of `text` from last frame, to detect edits made by the widget.
    last_text: String,
    pub version: i64,
    pub dirty: bool,
    /// Command of the server handling this file, key into `Editor::servers`.
    server: Option<String>,
    pub diagnostics: Vec<Diagnostic>,
}

struct Completion {
    items: Vec<CompletionItem>,
    selected: usize,
    /// Char index where the identifier being completed starts.
    prefix_start: usize,
}

pub struct Editor {
    pub docs: Vec<Doc>,
    pub active: usize,
    servers: HashMap<String, LspClient>,
    /// Servers that failed to start, so we do not retry every frame.
    failed: HashMap<String, String>,
    completion: Option<Completion>,
    pending_completion: Option<u64>,
    hover: Option<(u64, String)>,
    pub status: String,
    cursor_char: usize,
    /// True when a file was opened this frame: focus the editor and reset cursor.
    focus_next: bool,
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
            hover: None,
            status: String::new(),
            cursor_char: 0,
            focus_next: false,
        }
    }

    /// Open (or focus) a file. Starts the matching language server on demand,
    /// rooted at `root` (the explorer's current folder or a detected project).
    pub fn open(&mut self, path: &Path, root: &Path, servers: &[ServerDef]) {
        if let Some(i) = self.docs.iter().position(|d| d.path == path) {
            self.active = i;
            self.focus_next = true;
            return;
        }
        let text = match fs::read(path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => {
                    self.status = format!("{} is not UTF-8 text", path.display());
                    return;
                }
            },
            Err(e) => {
                self.status = format!("Cannot open {}: {e}", path.display());
                return;
            }
        };
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        let def = servers.iter().find(|s| s.extensions.iter().any(|x| *x == ext)).cloned();
        let mut doc = Doc {
            path: path.to_path_buf(),
            text: text.clone(),
            last_text: text.clone(),
            version: 0,
            dirty: false,
            server: None,
            diagnostics: Vec::new(),
        };
        if let Some(def) = def {
            let project_root = find_project_root(path, &def).unwrap_or_else(|| root.to_path_buf());
            if !self.servers.contains_key(&def.command) && !self.failed.contains_key(&def.command) {
                match LspClient::start(&def, &project_root) {
                    Ok(c) => {
                        self.servers.insert(def.command.clone(), c);
                        self.status = format!("{} starting for {}", def.command, project_root.display());
                    }
                    Err(e) => {
                        self.failed.insert(def.command.clone(), e.clone());
                        self.status = format!(
                            "No language server for .{ext}: '{}' is not installed or not on PATH (Settings > Language servers)",
                            def.command
                        );
                    }
                }
            }
            if let Some(c) = self.servers.get_mut(&def.command) {
                c.did_open(path, &def.language_id, &text);
                doc.server = Some(def.command.clone());
            }
        }
        self.docs.push(doc);
        self.active = self.docs.len() - 1;
        self.focus_next = true;
    }

    pub fn close(&mut self, i: usize) {
        if i >= self.docs.len() {
            return;
        }
        let d = self.docs.remove(i);
        if let Some(c) = d.server.as_ref().and_then(|s| self.servers.get_mut(s)) {
            c.did_close(&d.path);
        }
        if self.active >= self.docs.len() && !self.docs.is_empty() {
            self.active = self.docs.len() - 1;
        }
        self.completion = None;
    }

    /// Pretty-print the active document if it is JSON. Returns Err with the
    /// parse error when the text is not valid JSON, leaving it untouched.
    pub fn format_active(&mut self) -> Result<(), String> {
        let Some(d) = self.docs.get_mut(self.active) else { return Ok(()) };
        if !is_json(&d.path) {
            return Err("Format: only JSON is supported so far".into());
        }
        let v: serde_json::Value = serde_json::from_str(&d.text).map_err(|e| e.to_string())?;
        let mut out = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
        out.push('\n');
        if out != d.text {
            d.text = out.clone();
            d.last_text = out;
            d.dirty = true;
            d.version += 1;
            let (path, version, text) = (d.path.clone(), d.version, d.text.clone());
            if let Some(c) = d.server.as_ref().and_then(|s| self.servers.get_mut(s)) {
                c.did_change(&path, version, &text);
            }
        }
        Ok(())
    }

    pub fn save_active(&mut self, format_json: bool) {
        if format_json && self.docs.get(self.active).map(|d| is_json(&d.path)).unwrap_or(false) {
            if let Err(e) = self.format_active() {
                self.status = format!("Saved without formatting: {e}");
            }
        }
        let Some(d) = self.docs.get_mut(self.active) else { return };
        match fs::write(&d.path, &d.text) {
            Ok(()) => {
                d.dirty = false;
                self.status = format!("Saved {}", d.path.display());
                if let Some(c) = d.server.as_ref().and_then(|s| self.servers.get_mut(s)) {
                    c.did_save(&d.path, &d.text);
                }
            }
            Err(e) => self.status = format!("Save failed: {e}"),
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
                LspEvent::Message(m) => self.status = m,
                LspEvent::Died(_) => {}
            }
        }
    }

    /// Char index where the identifier under the cursor begins.
    fn prefix_start(&self) -> usize {
        let Some(d) = self.docs.get(self.active) else { return 0 };
        let chars: Vec<char> = d.text.chars().collect();
        let mut i = self.cursor_char.min(chars.len());
        while i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_') {
            i -= 1;
        }
        i
    }

    fn request_completion(&mut self) {
        let Some(d) = self.docs.get(self.active) else { return };
        let Some(c) = d.server.as_ref().and_then(|s| self.servers.get_mut(s)) else { return };
        let (line, col) = char_to_lsp(&d.text, self.cursor_char);
        let id = c.completion(&d.path, line, col);
        self.pending_completion = Some(id);
    }

    fn accept_completion(&mut self, ctx: &egui::Context, edit_id: egui::Id) {
        let Some(comp) = self.completion.take() else { return };
        let Some(item) = comp.items.get(comp.selected) else { return };
        let Some(d) = self.docs.get_mut(self.active) else { return };
        let chars: Vec<char> = d.text.chars().collect();
        let start = comp.prefix_start.min(chars.len());
        let end = self.cursor_char.min(chars.len());
        let mut new_text: String = chars[..start].iter().collect();
        new_text.push_str(&item.insert_text);
        let new_cursor = new_text.chars().count();
        new_text.extend(chars[end..].iter());
        d.text = new_text;
        // Move the widget's cursor to the end of the inserted text.
        let mut state = egui::TextEdit::load_state(ctx, edit_id).unwrap_or_default();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(new_cursor))));
        egui::TextEdit::store_state(ctx, edit_id, state);
        self.cursor_char = new_cursor;
    }

    pub fn show(&mut self, ui: &mut egui::Ui, servers: &[ServerDef], format_json_on_save: bool) {
        let _ = servers;
        self.poll_servers();
        let ctx = ui.ctx().clone();

        // ---- tab strip ----
        let mut close: Option<usize> = None;
        ui.horizontal_wrapped(|ui| {
            for (i, d) in self.docs.iter().enumerate() {
                let name = d.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let label = if d.dirty { format!("{name} •") } else { name };
                if ui.selectable_label(i == self.active, label).clicked() {
                    self.active = i;
                    self.completion = None;
                }
                if ui.small_button(crate::icons::CLOSE).clicked() {
                    close = Some(i);
                }
                ui.separator();
            }
        });
        if let Some(i) = close {
            self.close(i);
        }
        if self.docs.is_empty() {
            ui.centered_and_justified(|ui| ui.label("Open a file from the Explorer tab"));
            return;
        }
        ui.separator();

        // ---- keys the popup owns, consumed before the widget sees them ----
        let mut accept = false;
        let mut ctrl_space = false;
        let mut save = false;
        let mut format = false;
        let mut hover_req = false;
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
            }
            if i.consume_key(Modifiers::COMMAND, Key::Space) {
                ctrl_space = true;
            }
            if i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::F) {
                format = true;
            }
            if i.consume_key(Modifiers::COMMAND, Key::S) {
                save = true;
            }
            if i.consume_key(Modifiers::COMMAND, Key::K) {
                hover_req = true;
            }
        });

        let edit_id = ui.make_persistent_id(("editor", self.active));
        if accept {
            self.accept_completion(&ctx, edit_id);
        }
        if format {
            match self.format_active() {
                Ok(()) => self.status = "Formatted".into(),
                Err(e) => self.status = e,
            }
        }
        if save {
            self.save_active(format_json_on_save);
        }

        // ---- editor body ----
        let active = self.active;
        let lang = self.docs[active]
            .path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let theme = egui_extras::syntax_highlighting::CodeTheme::from_memory(&ctx, ui.style());
        let diags = self.docs[active].diagnostics.clone();
        let mut layouter = |ui: &egui::Ui, text: &str, wrap_width: f32| {
            let mut job = egui_extras::syntax_highlighting::highlight(ui.ctx(), ui.style(), &theme, text, &lang);
            underline_diagnostics(&mut job, text, &diags);
            job.wrap.max_width = wrap_width;
            ui.fonts(|f| f.layout_job(job))
        };

        let diag_h = if self.docs[active].diagnostics.is_empty() { 0.0 } else { 90.0 };
        let mut galley_pos = egui::Pos2::ZERO;
        let mut cursor_pos: Option<egui::Pos2> = None;
        let mut text_changed = false;
        egui::ScrollArea::both()
            .id_salt(("editor-scroll", active))
            .auto_shrink([false, false])
            .max_height(ui.available_height() - diag_h)
            .show(ui, |ui| {
                let d = &mut self.docs[active];
                let out = egui::TextEdit::multiline(&mut d.text)
                    .id(edit_id)
                    .code_editor()
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
                if d.text != d.last_text {
                    text_changed = true;
                    d.dirty = true;
                    d.version += 1;
                    d.last_text = d.text.clone();
                }
            });

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
            let chars: Vec<char> = text.chars().collect();
            let prev = self.cursor_char.checked_sub(1).and_then(|i| chars.get(i)).copied();
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
            let (line, col) = char_to_lsp(&d.text, self.cursor_char);
            let path = d.path.clone();
            if let Some(c) = d.server.as_ref().and_then(|s| self.servers.get_mut(s)) {
                let id = c.hover(&path, line, col);
                self.hover = Some((id, String::new()));
            }
        }

        // ---- completion popup ----
        if let (Some(comp), Some(pos)) = (&mut self.completion, cursor_pos) {
            let prefix: String = {
                let chars: Vec<char> = self.docs[active].text.chars().collect();
                let s = comp.prefix_start.min(chars.len());
                let e = self.cursor_char.min(chars.len()).max(s);
                chars[s..e].iter().collect::<String>().to_lowercase()
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
        if let (Some((_, text)), Some(pos)) = (&self.hover, cursor_pos) {
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
        if self.completion.is_some() || self.pending_completion.is_some() || self.hover.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        } else if !self.servers.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }

    fn prefix_len(&self) -> usize {
        self.cursor_char.saturating_sub(self.prefix_start())
    }
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
fn char_to_lsp(text: &str, idx: usize) -> (u32, u32) {
    let mut line = 0u32;
    let mut col = 0u32;
    for (i, c) in text.chars().enumerate() {
        if i == idx {
            break;
        }
        if c == '\n' {
            line += 1;
            col = 0;
        } else {
            col += c.len_utf16() as u32;
        }
    }
    (line, col)
}

/// (line, UTF-16 column) -> byte offset, clamped to the text.
fn lsp_to_byte(text: &str, line: u32, col: u32) -> usize {
    let mut cur_line = 0u32;
    let mut cur_col = 0u32;
    for (b, c) in text.char_indices() {
        if cur_line == line && cur_col >= col {
            return b;
        }
        if c == '\n' {
            if cur_line == line {
                return b;
            }
            cur_line += 1;
            cur_col = 0;
        } else if cur_line == line {
            cur_col += c.len_utf16() as u32;
        }
    }
    text.len()
}

/// Add a coloured underline to every diagnostic range by splitting sections.
fn underline_diagnostics(job: &mut LayoutJob, text: &str, diags: &[Diagnostic]) {
    if diags.is_empty() {
        return;
    }
    let ranges: Vec<(usize, usize, Color32)> = diags
        .iter()
        .map(|d| {
            let s = lsp_to_byte(text, d.line, d.col_start);
            let mut e = lsp_to_byte(text, d.line_end, d.col_end);
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
        .collect();
    let old = std::mem::take(&mut job.sections);
    for sec in old {
        let (a, b) = (sec.byte_range.start, sec.byte_range.end);
        // Cut points inside this section.
        let mut cuts: Vec<usize> = vec![a, b];
        for (s, e, _) in &ranges {
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
                part.format.underline = egui::Stroke::new(1.5, *color);
            }
            job.sections.push(part);
        }
    }
}

/// LSP CompletionItemKind to a short glyph.
fn kind_glyph(kind: u32) -> &'static str {
    match kind {
        2 | 3 => "ƒ",  // method, function
        4 => "⊕",      // constructor
        5 | 10 => "▪", // field, property
        6 => "𝑥",      // variable
        7 | 8 | 22 | 23 => "◇", // class, interface, struct, event
        9 => "▤",      // module
        13 | 20 => "𝔼", // enum, enum member
        14 => "⌘",     // keyword
        15 => "✂",     // snippet
        21 => "π",     // constant
        _ => "·",
    }
}

fn is_json(path: &Path) -> bool {
    matches!(
        path.extension().map(|e| e.to_string_lossy().to_lowercase()).as_deref(),
        Some("json") | Some("jsonc")
    )
}

/// Built-in JSON syntax check used when no language server handles .json.
fn json_diagnostics(text: &str) -> Vec<Diagnostic> {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(_) => Vec::new(),
        Err(e) => {
            let line = (e.line().max(1) - 1) as u32;
            let col = (e.column().max(1) - 1) as u32;
            vec![Diagnostic { line, col_start: col, line_end: line, col_end: col + 1, severity: 1, message: e.to_string() }]
        }
    }
}

#[cfg(test)]
mod tests {
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
    fn pretty_print_round_trips_and_ends_with_newline() {
        let v: serde_json::Value = serde_json::from_str("{\"b\":[1,{\"c\":true}],\"a\":\"x\"}").unwrap();
        let mut out = serde_json::to_string_pretty(&v).unwrap();
        out.push('\n');
        assert!(out.starts_with("{\n  \"a\": \"x\""));
        assert!(out.ends_with("}\n"));
    }
}
