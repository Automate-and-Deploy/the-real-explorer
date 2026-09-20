//! Incremental syntax highlighting.
//!
//! The editor used to call `egui_extras::syntax_highlighting::highlight`, which
//! keys a frame cache on the whole document text: every keystroke missed the
//! cache and re-ran syntect over the entire buffer, and even a hit cloned the
//! whole `LayoutJob`. Measured on this tree, 98 % of that cost is
//! `ParseState::parse_line`, so the fix is to cache the *parse state*, not the
//! styles.
//!
//! This module owns the `SyntaxSet` (syntect's defaults plus the `two-face`
//! grammar pack, which is the only way ts/tsx/jsx/mjs/toml get coloured at all)
//! and the two themes `egui_extras` would have picked, and gives each document
//! a [`DocHighlight`] holding per-line style runs with a parse/highlight
//! checkpoint every K lines.
//!
//! After an edit the walk resumes from the nearest checkpoint at or before the
//! first changed line and stops as soon as the recomputed `(ParseState,
//! HighlightState)` pair equals the one already stored for that line — measured
//! convergence after a one-character edit is a single line. Multi-line
//! constructs need no special case: the context stack *is* the state, so an
//! unterminated `/*`, a raw string or a markdown fence simply fails to converge
//! and the walk continues until it does.
//!
//! Styles are interned to a `u16`, so a cached run costs 8 bytes rather than the
//! ~100-byte `TextFormat` epaint wants; the `TextFormat` is materialised only
//! for lines actually put into a job.

use std::cell::Cell;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use eframe::egui::{
    text::{LayoutJob, LayoutSection, TextFormat},
    Color32, FontId,
};
use syntect::highlighting::{
    FontStyle, HighlightIterator, HighlightState, Highlighter, Style, Theme, ThemeSet,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};

/// Bytes a single `(ParseState, HighlightState)` checkpoint costs on the heap.
///
/// Measured with a counting allocator over `src/main.rs` (2 369 lines) and a
/// 2.2 MB file: 1 598 B/line in both. Rounded up; the stride is derived from it
/// so the constant only has to be the right order of magnitude to keep the cap.
const CHECKPOINT_BYTES: usize = 1_600;

/// Ceiling on checkpoint memory for one document. The stride K is chosen to
/// respect it, so a 150 k-line file simply gets sparser checkpoints rather than
/// 240 MB of them.
const CHECKPOINT_MEM_CAP: usize = 8 * 1024 * 1024;

/// Documents at least this large get their first full pass on a background
/// thread; below it the foreground walk finishes within a frame or two anyway
/// and a thread is pure overhead.
const BACKGROUND_MIN_BYTES: usize = 256 * 1024;

/// Lines the background pass highlights before handing a batch over. Big enough
/// that the channel is not the bottleneck, small enough that colour appears
/// while the pass is still running.
const BACKGROUND_BATCH_LINES: usize = 512;

/// Half-width, in lines, of the window of cached section vectors kept around
/// the last requested range. Outside it the runs are dropped and re-derived
/// from the nearest checkpoint; the checkpoints themselves are never dropped by
/// trimming.
const SECTION_WINDOW_LINES: usize = 20_000;

// ---------------------------------------------------------------------------
// shared assets
// ---------------------------------------------------------------------------

/// The grammar and theme tables, built once for the process.
///
/// `SyntaxSet::load_defaults_newlines` is 0.7 ms / 404 KiB and the two-face
/// pack adds about a megabyte of dump, so owning these rather than letting
/// `egui_extras` hide them in a frame cache costs nothing and buys the whole
/// incremental scheme.
struct Assets {
    /// Shared with background passes, which is why it is an `Arc`:
    /// `SyntaxSet` is `Send + Sync`.
    syntaxes: Arc<SyntaxSet>,
    /// What `CodeTheme::dark` resolves to in `egui_extras` 0.29.
    dark: Theme,
    /// What `CodeTheme::light` resolves to in `egui_extras` 0.29.
    light: Theme,
}

static ASSETS: OnceLock<Assets> = OnceLock::new();

fn assets() -> &'static Assets {
    ASSETS.get_or_init(|| {
        // two-face bundles syntect's defaults plus bat's extras, so this is a
        // superset of `load_defaults_newlines()` and not an addition to it.
        let syntaxes = two_face::syntax::extra_newlines();
        let themes = ThemeSet::load_defaults();
        // These two names are exactly what `egui_extras::syntax_highlighting`
        // picks for its dark and light `CodeTheme`, so switching to this module
        // does not change any colour the user already sees.
        let dark = themes.themes["base16-mocha.dark"].clone();
        let light = themes.themes["Solarized (light)"].clone();
        Assets { syntaxes: Arc::new(syntaxes), dark, light }
    })
}

/// Number of syntaxes available, for the bench and for tests that assert the
/// two-face pack really is loaded.
#[allow(dead_code)]
pub fn syntax_count() -> usize {
    assets().syntaxes.syntaxes().len()
}

/// Extensions no bundled grammar claims, mapped to the nearest one that parses
/// them correctly. Verified against the two-face 0.5.2 set (213 syntaxes).
fn alias(ext: &str) -> Option<&'static str> {
    match ext {
        // Nothing claims .jsx. `TypeScriptReact` is the TSX grammar, and JSX is
        // TSX without the type annotations, so it parses JSX exactly.
        "jsx" => Some("TypeScriptReact"),
        // ES and CommonJS modules are JavaScript under a different suffix; the
        // JavaScript grammar only claims .js and .htc.
        "mjs" | "cjs" => Some("JavaScript"),
        _ => None,
    }
}

/// Resolves an extension (or a syntax name) the way the old call did: by name
/// first, then by extension. `None` means the document is highlighted plain.
fn resolve_syntax(lang: &str) -> Option<String> {
    if lang.is_empty() {
        return None;
    }
    let ps = &assets().syntaxes;
    ps.find_syntax_by_name(lang)
        .or_else(|| ps.find_syntax_by_extension(lang))
        .or_else(|| alias(lang).and_then(|n| ps.find_syntax_by_name(n)))
        .map(|s| s.name.clone())
}

// ---------------------------------------------------------------------------
// style interning
// ---------------------------------------------------------------------------

/// Everything from a syntect `Style` that survives into a `TextFormat`.
///
/// Background is deliberately dropped: the editor paints find matches with it
/// and a theme background per run would fight that, which is also what the old
/// `egui_extras` path did.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct StyleKey {
    r: u8,
    g: u8,
    b: u8,
    bold: bool,
    italic: bool,
    underline: bool,
}

impl StyleKey {
    fn from_syntect(s: &Style) -> Self {
        let fs = s.font_style;
        Self {
            r: s.foreground.r,
            g: s.foreground.g,
            b: s.foreground.b,
            bold: fs.contains(FontStyle::BOLD),
            italic: fs.contains(FontStyle::ITALIC),
            // Upstream sets `underline` from `FontStyle::ITALIC`
            // (egui_extras-0.29.1/src/syntax_highlighting.rs:465): a
            // copy-paste bug that underlines every italic run. Read the
            // UNDERLINE bit, which is what the theme actually meant.
            underline: fs.contains(FontStyle::UNDERLINE),
        }
    }

    fn to_format(self, font_id: &FontId) -> TextFormat {
        let color = Color32::from_rgb(self.r, self.g, self.b);
        TextFormat {
            font_id: font_id.clone(),
            color,
            italics: self.italic,
            underline: if self.underline {
                eframe::egui::Stroke::new(1.0_f32, color)
            } else {
                eframe::egui::Stroke::NONE
            },
            ..Default::default()
        }
    }
}

/// Maps a `StyleKey` to a `u16` id. A document uses a handful of distinct
/// styles, so this stays tiny and the id fits beside a length in 8 bytes.
#[derive(Default)]
struct Interner {
    ids: HashMap<StyleKey, u16>,
    keys: Vec<StyleKey>,
}

impl Interner {
    fn intern(&mut self, key: StyleKey) -> u16 {
        if let Some(&id) = self.ids.get(&key) {
            return id;
        }
        let id = self.keys.len() as u16;
        self.keys.push(key);
        self.ids.insert(key, id);
        id
    }

    fn get(&self, id: u16) -> StyleKey {
        self.keys.get(id as usize).copied().unwrap_or(StyleKey {
            r: 0xc0,
            g: 0xc0,
            b: 0xc0,
            bold: false,
            italic: false,
            underline: false,
        })
    }

    fn clear(&mut self) {
        self.ids.clear();
        self.keys.clear();
    }
}

// ---------------------------------------------------------------------------
// budget
// ---------------------------------------------------------------------------

/// How much work one [`DocHighlight::advance`] call may do.
///
/// Both limits are checked; whichever is hit first stops the walk and leaves a
/// watermark for the next call, so a pathological edit at line 0 of a huge file
/// costs one frame rather than one stall.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    /// Maximum source lines to (re)highlight.
    pub lines: usize,
    /// Wall-clock ceiling, checked every 64 lines.
    pub time: Option<Duration>,
}

impl Budget {
    /// The per-frame budget the editor uses: enough to colour several screens
    /// ahead, capped so no single frame blows past a 60 Hz slice.
    pub fn frame() -> Self {
        Self { lines: 4096, time: Some(Duration::from_millis(3)) }
    }

    /// No limit: run to convergence or to the end of the document.
    #[allow(dead_code)]
    pub fn unlimited() -> Self {
        Self { lines: usize::MAX, time: None }
    }

    /// A pure line-count budget, for tests and the bench.
    #[allow(dead_code)]
    pub fn lines(n: usize) -> Self {
        Self { lines: n, time: None }
    }
}

// ---------------------------------------------------------------------------
// per-line cache
// ---------------------------------------------------------------------------

/// One source line's cached highlight.
#[derive(Default)]
struct LineEntry {
    /// Style runs as `(byte length, interned style id)`, summing to the line's
    /// byte length including its newline. `None` once trimmed out of the
    /// section window, or never computed. Boxed rather than a `Vec` because a
    /// `Vec` header costs 8 bytes more per line, which is 1.5 MB at 190 k
    /// lines for a capacity field nothing ever uses.
    runs: Option<Box<[(u32, u16)]>>,
    /// Parser and highlighter state *after* this line, kept every K lines.
    /// This is what convergence is tested against.
    state: Option<Box<(ParseState, HighlightState)>>,
}

/// A batch of background-pass results.
struct BgBatch {
    first_line: usize,
    runs: Vec<Vec<(u32, u16)>>,
    /// Style keys the worker interned since its last batch, appended in id
    /// order so the main thread can extend its remap table.
    new_styles: Vec<StyleKey>,
    /// `(line index, state after that line)` at the worker's stride.
    checkpoints: Vec<(usize, Box<(ParseState, HighlightState)>)>,
    /// Total lines in the snapshot, so progress can be reported.
    total_lines: usize,
}

// ---------------------------------------------------------------------------
// DocHighlight
// ---------------------------------------------------------------------------

/// Per-document incremental highlighter.
///
/// The caller must keep it honest: [`reset`](Self::reset) after a wholesale
/// text replace, [`note_edit`](Self::note_edit) after a splice,
/// [`advance`](Self::advance) once per frame before
/// [`layout_job`](Self::layout_job).
pub struct DocHighlight {
    /// Resolved syntax name, or `None` for a language syntect has no grammar
    /// for — those documents are laid out plain, exactly as before.
    syntax: Option<String>,
    /// Which of the two themes is in force. Changing it invalidates everything,
    /// because `HighlightState` is theme-dependent.
    dark: bool,

    lines: Vec<LineEntry>,
    interner: Interner,
    /// Byte offset of every line start, mirroring `LinesWithEndings`.
    line_starts: Vec<usize>,
    /// `text.len()` when `line_starts` was built, so a missed `note_edit`
    /// cannot serve offsets into the wrong string.
    indexed_len: usize,
    /// Set by `note_edit`/`reset`; makes the next `advance` rebuild the index.
    index_dirty: bool,
    /// Lowest first-changed-line seen since the index was last rebuilt. The
    /// line vector is spliced there by the line-count delta so cached entries
    /// below and above a multi-line edit both stay aligned.
    pending_splice: Option<usize>,

    /// Lines `[0, hl_to)` have valid cached runs and a valid state chain.
    hl_to: usize,
    /// Live cursor: the state after line `cursor_line - 1`, so consecutive
    /// frames continue without re-walking from a checkpoint.
    cursor: Option<(usize, ParseState, HighlightState)>,
    /// Checkpoint stride, recomputed from the line count.
    stride: usize,
    /// Lines walked by the most recent `advance`, for the convergence test.
    last_advance_lines: usize,

    /// Line range most recently asked of `layout_job`, which anchors the
    /// section window. `Cell` because `layout_job` takes `&self`.
    last_range: Cell<Option<(usize, usize)>>,
    /// Sections the previous job needed, so the next one allocates once
    /// instead of doubling its way up to eight megabytes.
    section_hint: Cell<usize>,
    /// Window `trim_sections` last enforced. Trimming walks every line, so
    /// repeating it for an unchanged window would cost more than it saves.
    trimmed_to: Option<(usize, usize)>,

    bg: Option<Receiver<BgBatch>>,
    /// True once a background pass has been started for the current text, so a
    /// finished or cancelled pass is not restarted every frame.
    bg_started: bool,
    /// Worker style id -> local style id.
    bg_remap: Vec<u16>,
    /// Background results are only accepted below this line: an edit that
    /// arrives mid-pass makes everything at or after it stale.
    bg_valid_below: usize,
    /// Total lines in the background snapshot, for progress reporting.
    bg_total: usize,
    /// Size at which the first pass goes off-thread. A field rather than the
    /// constant directly so tests can exercise the channel path without
    /// parsing a quarter of a megabyte.
    bg_min_bytes: usize,
}

impl Default for DocHighlight {
    fn default() -> Self {
        Self::new("")
    }
}

impl DocHighlight {
    /// Build a highlighter for a document with the given extension (or syntax
    /// name). The grammar is resolved once here rather than per frame; an
    /// unknown extension means plain output for the life of the document.
    pub fn new(lang_ext: &str) -> Self {
        Self {
            syntax: resolve_syntax(lang_ext),
            dark: true,
            lines: Vec::new(),
            interner: Interner::default(),
            line_starts: Vec::new(),
            indexed_len: usize::MAX,
            index_dirty: true,
            pending_splice: Some(0),
            hl_to: 0,
            cursor: None,
            stride: 1,
            last_advance_lines: 0,
            last_range: Cell::new(None),
            section_hint: Cell::new(0),
            trimmed_to: None,
            bg: None,
            bg_started: false,
            bg_remap: Vec::new(),
            bg_valid_below: usize::MAX,
            bg_total: 0,
            bg_min_bytes: BACKGROUND_MIN_BYTES,
        }
    }

    /// Send even a small document's first pass to a background thread, so the
    /// channel, the style remap and the mid-pass edit rule can be tested
    /// without a 256 KiB fixture.
    #[cfg(test)]
    fn always_background(&mut self) {
        self.bg_min_bytes = 0;
    }

    /// True when no grammar matched and every job is laid out plain.
    #[allow(dead_code)]
    pub fn is_plain(&self) -> bool {
        self.syntax.is_none()
    }

    /// Throw the whole cache away. For a wholesale text replace (reload from
    /// disk, conflict resolution), where no first-changed-line is meaningful.
    pub fn reset(&mut self) {
        self.lines.clear();
        self.interner.clear();
        self.line_starts.clear();
        self.indexed_len = usize::MAX;
        self.index_dirty = true;
        self.pending_splice = Some(0);
        self.hl_to = 0;
        self.cursor = None;
        self.last_advance_lines = 0;
        self.trimmed_to = None;
        // Drop the receiver: the worker's next `send` fails and it exits.
        self.bg = None;
        self.bg_started = false;
        self.bg_remap.clear();
        self.bg_valid_below = usize::MAX;
        self.bg_total = 0;
    }

    /// Record that the text was spliced and the first line whose content may
    /// have changed. Everything from there down is treated as unknown until
    /// `advance` re-derives it or proves it converged.
    pub fn note_edit(&mut self, first_changed_line: usize) {
        if self.syntax.is_none() {
            return;
        }
        let l = first_changed_line;
        self.hl_to = self.hl_to.min(l);
        self.bg_valid_below = self.bg_valid_below.min(l);
        self.pending_splice = Some(match self.pending_splice {
            Some(p) => p.min(l),
            None => l,
        });
        self.index_dirty = true;
        // The cursor is only usable while it sits at the watermark.
        if self.cursor.as_ref().is_some_and(|(at, _, _)| *at > self.hl_to) {
            self.cursor = None;
        }
    }

    /// Switch between the dark and light theme. Styles are baked into the
    /// cache, so a change costs a full re-highlight; it happens on a theme
    /// toggle, not per frame.
    pub fn set_dark(&mut self, dark: bool) {
        if self.dark != dark {
            self.dark = dark;
            self.reset();
        }
    }

    /// Fraction of the document coloured so far, while a background pass is
    /// running. `None` once there is nothing to wait for.
    pub fn progress(&self) -> Option<f32> {
        if self.bg.is_none() || self.bg_total == 0 {
            return None;
        }
        Some((self.hl_to as f32 / self.bg_total as f32).clamp(0.0, 1.0))
    }

    /// Lines walked by the last `advance`. Convergence tests read this.
    #[allow(dead_code)]
    pub fn last_advance_lines(&self) -> usize {
        self.last_advance_lines
    }

    /// Number of stored checkpoints.
    #[allow(dead_code)]
    pub fn checkpoint_count(&self) -> usize {
        self.lines.iter().filter(|e| e.state.is_some()).count()
    }

    /// Modelled checkpoint memory, from the measured per-checkpoint cost.
    #[allow(dead_code)]
    pub fn estimated_checkpoint_bytes(&self) -> usize {
        self.checkpoint_count() * CHECKPOINT_BYTES
    }

    /// Cached style runs currently held, i.e. the size of the section window.
    #[allow(dead_code)]
    pub fn cached_run_lines(&self) -> usize {
        self.lines.iter().filter(|e| e.runs.is_some()).count()
    }

    /// Checkpoint stride for a document of `lines` lines: the smallest stride
    /// that keeps `lines / stride` checkpoints inside [`CHECKPOINT_MEM_CAP`].
    ///
    /// Small files therefore checkpoint every line, which is what makes
    /// convergence detectable on the very next line rather than at the next
    /// multiple of a fixed K.
    pub fn stride_for(lines: usize) -> usize {
        let max_checkpoints = (CHECKPOINT_MEM_CAP / CHECKPOINT_BYTES).max(1);
        if lines <= max_checkpoints {
            1
        } else {
            lines.div_ceil(max_checkpoints)
        }
    }

    /// Number of source lines, mirroring `LinesWithEndings`.
    #[allow(dead_code)]
    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// Leading lines that currently have valid colour. Everything from here
    /// down is emitted plain until `advance` reaches it.
    #[allow(dead_code)]
    pub fn highlighted_lines(&self) -> usize {
        self.hl_to
    }

    // -- internals ---------------------------------------------------------

    fn theme(&self) -> &'static Theme {
        if self.dark {
            &assets().dark
        } else {
            &assets().light
        }
    }

    /// Rebuild the line index and realign the per-line cache after a splice.
    fn sync_text(&mut self, text: &str) {
        if !self.index_dirty && self.indexed_len == text.len() {
            return;
        }
        let old_lines = self.line_starts.len();
        build_line_starts(text, &mut self.line_starts);
        self.indexed_len = text.len();
        self.index_dirty = false;
        let new_lines = self.line_starts.len();

        // A contiguous splice shifts every line after the changed region by the
        // line-count delta, so the cache is shifted the same way: entries below
        // the splice point keep their index, entries above move with it.
        let at = self.pending_splice.take().unwrap_or(0).min(self.lines.len());
        if new_lines != old_lines && !self.lines.is_empty() {
            if new_lines > old_lines {
                let extra = new_lines - old_lines;
                self.lines.splice(at..at, (0..extra).map(|_| LineEntry::default()));
            } else {
                let gone = (old_lines - new_lines).min(self.lines.len() - at);
                self.lines.drain(at..at + gone);
            }
        }
        self.lines.resize_with(new_lines, LineEntry::default);
        if new_lines != old_lines {
            self.trimmed_to = None;
        }
        self.hl_to = self.hl_to.min(new_lines);
        self.stride = Self::stride_for(new_lines);
        // The edited line itself is never valid until re-walked.
        if at < self.hl_to {
            self.hl_to = at;
        }
        if self.cursor.as_ref().is_some_and(|(l, _, _)| *l != self.hl_to) {
            self.cursor = None;
        }
    }

    fn line_range(&self, i: usize) -> Range<usize> {
        let start = self.line_starts[i];
        let end = self.line_starts.get(i + 1).copied().unwrap_or(self.indexed_len);
        start..end
    }

    /// Re-parse from the dirty watermark, resuming from the nearest checkpoint,
    /// stopping on convergence or when the budget is spent.
    ///
    /// Returns true when work remains, so the caller can request a repaint.
    pub fn advance(&mut self, text: &str, budget: Budget) -> bool {
        self.last_advance_lines = 0;
        if self.syntax.is_none() {
            return false;
        }
        self.sync_text(text);
        self.drain_background();
        self.maybe_start_background(text);
        self.trim_sections();
        let refill_more = self.refill_window(text, budget);

        let total = self.line_starts.len();
        if self.hl_to >= total {
            // The foreground caught up (or converged): whatever the background
            // pass still has to say is already known, so stop paying for it.
            self.bg = None;
            return refill_more;
        }

        let ps = assets().syntaxes.clone();
        let syntax_name = self.syntax.clone().unwrap();
        let Some(syntax) = ps.find_syntax_by_name(&syntax_name) else {
            self.syntax = None;
            return false;
        };
        let theme = self.theme();
        let highlighter = Highlighter::new(theme);

        // Resume: the live cursor if it still sits at the watermark, else the
        // nearest checkpoint strictly below it, else the top of the document.
        let (mut line, mut parse, mut hstate) = match self.cursor.take() {
            Some((at, p, h)) if at == self.hl_to => (at, p, h),
            _ => {
                let mut found = None;
                for i in (0..self.hl_to).rev() {
                    if let Some(st) = &self.lines[i].state {
                        found = Some((i + 1, st.0.clone(), st.1.clone()));
                        break;
                    }
                }
                found.unwrap_or_else(|| {
                    (0, ParseState::new(syntax), HighlightState::new(&highlighter, ScopeStack::new()))
                })
            }
        };
        // Lines between the checkpoint and the watermark are already correct in
        // the cache, but the state chain has to be replayed over them.
        let resume_from = line;
        self.hl_to = self.hl_to.max(resume_from);

        let started = Instant::now();
        let mut walked = 0usize;
        let mut converged = false;
        while line < total {
            if walked >= budget.lines {
                break;
            }
            if let Some(limit) = budget.time {
                // `Instant::now` is not free; 64 lines is well under a frame.
                if walked % 64 == 0 && walked > 0 && started.elapsed() >= limit {
                    break;
                }
            }
            let range = self.line_range(line);
            let src = &text[range];
            let ops = match parse.parse_line(src, &ps) {
                Ok(v) => v,
                Err(_) => Vec::new(),
            };
            let mut runs: Vec<(u32, u16)> = Vec::new();
            for (style, piece) in HighlightIterator::new(&mut hstate, &ops[..], src, &highlighter) {
                if piece.is_empty() {
                    continue;
                }
                let id = self.interner.intern(StyleKey::from_syntect(&style));
                // Merge equal-style neighbours: syntect emits one range per
                // scope change and epaint pays per section, twice.
                match runs.last_mut() {
                    Some(last) if last.1 == id => last.0 += piece.len() as u32,
                    _ => runs.push((piece.len() as u32, id)),
                }
            }
            self.lines[line].runs = Some(runs.into_boxed_slice());
            walked += 1;

            // Convergence: if the state after this line is what was already
            // stored there, every line below is still valid as cached.
            let previous = self.lines[line].state.take();
            let keep = (line + 1) % self.stride == 0;
            if let Some(prev) = previous {
                if line >= resume_from && prev.0 == parse && prev.1 == hstate {
                    self.lines[line].state = Some(prev);
                    self.hl_to = total;
                    converged = true;
                    line += 1;
                    break;
                }
                if keep {
                    self.lines[line].state = Some(Box::new((parse.clone(), hstate.clone())));
                }
            } else if keep {
                self.lines[line].state = Some(Box::new((parse.clone(), hstate.clone())));
            }
            line += 1;
            self.hl_to = self.hl_to.max(line);
        }
        self.last_advance_lines = walked;

        if !converged {
            self.hl_to = self.hl_to.max(line.min(total));
            self.cursor = Some((line, parse, hstate));
        } else {
            self.cursor = None;
        }
        self.hl_to < total || self.bg.is_some() || refill_more
    }

    /// Re-derive the style runs for lines the view is asking for that an
    /// earlier [`trim_sections`](Self::trim_sections) threw away.
    ///
    /// Trimming keeps the checkpoints and drops the runs, on the promise that
    /// the runs can be rebuilt from them. Nothing rebuilt them: the main walk
    /// only moves `hl_to` forward, so a line below the watermark whose runs
    /// were trimmed stayed plain for the rest of the session. That is
    /// invisible on the `TextEdit` path, which asks for the whole document and
    /// is therefore never trimmed, and immediate on the virtualised one the
    /// first time you scroll a long way and come back.
    ///
    /// The checkpoint stride is chosen to keep checkpoint memory under a cap,
    /// so the replay from the nearest one is tens of lines, not thousands.
    /// Returns true when the window is still not complete.
    fn refill_window(&mut self, text: &str, budget: Budget) -> bool {
        let Some((lo, hi)) = self.last_range.get() else { return false };
        let hi = hi.min(self.hl_to).min(self.lines.len());
        if lo >= hi {
            return false;
        }
        let Some(first_missing) = (lo..hi).find(|i| self.lines[*i].runs.is_none()) else {
            return false;
        };
        let ps = assets().syntaxes.clone();
        let Some(name) = self.syntax.clone() else { return false };
        let Some(syntax) = ps.find_syntax_by_name(&name) else { return false };
        let highlighter = Highlighter::new(self.theme());

        // Resume from the nearest checkpoint at or below the gap. The lines
        // between it and the gap are parsed only to carry the state forward;
        // their runs are already cached or deliberately gone.
        let (mut line, mut parse, mut hstate) = {
            let mut found = None;
            for i in (0..first_missing).rev() {
                if let Some(st) = &self.lines[i].state {
                    found = Some((i + 1, st.0.clone(), st.1.clone()));
                    break;
                }
            }
            found.unwrap_or_else(|| {
                (0, ParseState::new(syntax), HighlightState::new(&highlighter, ScopeStack::new()))
            })
        };

        let started = Instant::now();
        let mut walked = 0usize;
        while line < hi {
            if walked >= budget.lines {
                break;
            }
            if let Some(limit) = budget.time {
                if walked % 64 == 0 && walked > 0 && started.elapsed() >= limit {
                    break;
                }
            }
            let range = self.line_range(line);
            let src = &text[range];
            let ops = parse.parse_line(src, &ps).unwrap_or_default();
            let mut runs: Vec<(u32, u16)> = Vec::new();
            for (style, piece) in HighlightIterator::new(&mut hstate, &ops[..], src, &highlighter) {
                if piece.is_empty() {
                    continue;
                }
                let id = self.interner.intern(StyleKey::from_syntect(&style));
                match runs.last_mut() {
                    Some(last) if last.1 == id => last.0 += piece.len() as u32,
                    _ => runs.push((piece.len() as u32, id)),
                }
            }
            if line >= first_missing {
                self.lines[line].runs = Some(runs.into_boxed_slice());
            }
            walked += 1;
            line += 1;
        }
        self.last_advance_lines += walked;
        line < hi
    }

    /// Start the initial pass off-thread for a large document. The UI never
    /// blocks on it: results arrive through a channel that `advance` drains.
    fn maybe_start_background(&mut self, text: &str) {
        if self.bg_started || self.bg.is_some() || text.len() < self.bg_min_bytes {
            return;
        }
        let Some(name) = self.syntax.clone() else { return };
        self.bg_started = true;
        self.bg_valid_below = usize::MAX;
        self.bg_remap.clear();
        self.bg_total = self.line_starts.len();

        let snapshot = text.to_owned();
        let ps = assets().syntaxes.clone();
        let theme = self.theme().clone();
        let stride = self.stride;
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("highlight-fill".into())
            .spawn(move || background_pass(snapshot, ps, name, theme, stride, tx));
        match spawned {
            Ok(_) => self.bg = Some(rx),
            // A thread we cannot spawn is not fatal: the foreground walk still
            // colours the document, just more slowly.
            Err(_) => self.bg = None,
        }
    }

    /// Take whatever the background pass has produced. Batches at or after an
    /// edit are dropped; the foreground walk re-derives those lines.
    fn drain_background(&mut self) {
        let Some(rx) = &self.bg else { return };
        let mut finished = false;
        loop {
            match rx.try_recv() {
                Ok(batch) => {
                    self.bg_total = batch.total_lines;
                    // Extend the worker-id -> local-id table.
                    for key in &batch.new_styles {
                        let id = self.interner.intern(*key);
                        self.bg_remap.push(id);
                    }
                    let first = batch.first_line;
                    if first > self.hl_to {
                        // Non-contiguous (an edit reset the watermark below it):
                        // nothing to graft onto, so drop it.
                        continue;
                    }
                    let mut accepted = first;
                    for (k, runs) in batch.runs.into_iter().enumerate() {
                        let idx = first + k;
                        if idx >= self.bg_valid_below || idx >= self.lines.len() {
                            break;
                        }
                        let mapped: Box<[(u32, u16)]> = runs
                            .into_iter()
                            .map(|(len, id)| {
                                (len, self.bg_remap.get(id as usize).copied().unwrap_or(0))
                            })
                            .collect();
                        self.lines[idx].runs = Some(mapped);
                        accepted = idx + 1;
                    }
                    for (idx, state) in batch.checkpoints {
                        if idx < accepted && idx < self.lines.len() {
                            self.lines[idx].state = Some(state);
                        }
                    }
                    if accepted > self.hl_to {
                        self.hl_to = accepted;
                        // The foreground cursor is now behind the watermark and
                        // would re-walk valid lines; drop it so the next
                        // `advance` resumes from a background checkpoint.
                        self.cursor = None;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
            }
        }
        if finished {
            self.bg = None;
        }
    }

    /// Drop cached style runs outside the window around the last requested
    /// range. Checkpoints stay, so the dropped lines can be re-derived.
    ///
    /// A `None` request means the whole document is the range (today's
    /// `TextEdit` path), and then nothing is trimmed.
    fn trim_sections(&mut self) {
        let Some((lo, hi)) = self.last_range.get() else { return };
        let keep_lo = lo.saturating_sub(SECTION_WINDOW_LINES);
        let keep_hi = hi.saturating_add(SECTION_WINDOW_LINES);
        if keep_lo == 0 && keep_hi >= self.lines.len() {
            return;
        }
        if self.trimmed_to == Some((keep_lo, keep_hi)) {
            return;
        }
        self.trimmed_to = Some((keep_lo, keep_hi));
        for (i, entry) in self.lines.iter_mut().enumerate() {
            if i < keep_lo || i >= keep_hi {
                entry.runs = None;
            }
        }
    }

    /// Tell the highlighter which lines are on screen.
    ///
    /// [`layout_job`](Self::layout_job) records the range it was asked for,
    /// and that record is what the section window and the refill work from.
    /// A virtualised view asks one line at a time, so the last of those calls
    /// would leave the window one row wide and everything else on screen
    /// permanently plain. It calls this once a frame, after painting, with the
    /// whole visible span. Takes `&self` for the same reason `layout_job`
    /// does: the record is a `Cell`, and the view holds the highlighter by
    /// shared reference while it paints.
    pub fn note_visible(&self, lines: Range<usize>) {
        let total = self.line_starts.len();
        self.last_range.set(Some((lines.start.min(total), lines.end.min(total))));
    }

    /// True when the per-line byte index matches `text`, so a `visible` range
    /// handed to [`layout_job`](Self::layout_job) means the lines the caller
    /// thinks it means.
    ///
    /// A document with no grammar never builds the index, and neither does one
    /// whose text moved without a `note_edit`. A virtualised view has to check
    /// this before asking for a line range: the plain fallback below has no
    /// index to slice with and hands back the whole document.
    pub fn indexed_for(&self, text: &str) -> bool {
        self.indexed_len == text.len() && self.syntax.is_some()
    }

    /// Build a `LayoutJob`.
    ///
    /// `visible` is a line range. `None` means the whole document, which is
    /// what today's `TextEdit` needs; `Some(range)` emits only those lines with
    /// byte offsets relative to the start of the range, which is what a
    /// virtualised view needs. Lines not yet highlighted come out plain.
    pub fn layout_job(
        &self,
        text: &str,
        font_id: FontId,
        plain_color: Color32,
        visible: Option<Range<usize>>,
        wrap_width: f32,
    ) -> LayoutJob {
        self.layout_job_capped(text, font_id, plain_color, visible, wrap_width, usize::MAX)
    }

    /// [`layout_job`](Self::layout_job) with a ceiling on how many bytes of the
    /// range it emits, counted from the start of the range.
    ///
    /// The virtualised view lays one source line out per row with word wrap
    /// off, so a file whose only newline is at the end would otherwise become
    /// one galley holding every glyph in the document. Cutting the slice here
    /// rather than truncating the finished job means the oversized line is
    /// never copied, let alone laid out. The cut lands on a char boundary, so
    /// the job is always valid UTF-8 the sections can index.
    pub fn layout_job_capped(
        &self,
        text: &str,
        font_id: FontId,
        plain_color: Color32,
        visible: Option<Range<usize>>,
        wrap_width: f32,
        max_bytes: usize,
    ) -> LayoutJob {
        let total = self.line_starts.len();
        let indexed = self.indexed_len == text.len() && self.syntax.is_some();
        let (lo, hi) = match &visible {
            Some(r) => (r.start.min(total), r.end.min(total)),
            None => (0, total),
        };
        self.last_range.set(visible.as_ref().map(|_| (lo, hi)));

        if !indexed {
            // No usable index (unknown language, or the caller changed the text
            // without telling us): fall back to the old plain layout.
            let slice = if visible.is_some() && total > 0 {
                let s = self.line_starts.get(lo).copied().unwrap_or(0);
                let e = self.line_starts.get(hi).copied().unwrap_or(text.len());
                text.get(s..e).unwrap_or(text)
            } else {
                text
            };
            return LayoutJob::simple(cap_slice(slice, max_bytes).to_owned(), font_id, plain_color, wrap_width);
        }

        let base = if lo < total { self.line_starts[lo] } else { text.len() };
        let end = if hi < total { self.line_starts[hi] } else { text.len() };
        let slice = cap_slice(&text[base..end], max_bytes);

        let mut job = LayoutJob {
            text: slice.to_owned(),
            sections: Vec::with_capacity(self.section_hint.get()),
            ..Default::default()
        };
        job.wrap.max_width = wrap_width;

        // One `TextFormat` per distinct style, cloned into the sections: the
        // interner keeps that table to a handful of entries.
        let plain_fmt = TextFormat { font_id: font_id.clone(), color: plain_color, ..Default::default() };
        let mut cursor = 0usize;
        let mut last_id: Option<u16> = None;
        for i in lo..hi {
            // `slice` may stop short of the range when `max_bytes` bit, and
            // then the last line it does reach is only partly present.
            if cursor >= slice.len() {
                break;
            }
            let lr = self.line_range(i);
            let len = (lr.end - lr.start).min(slice.len() - cursor);
            let runs = if i < self.hl_to { self.lines[i].runs.as_ref() } else { None };
            match runs {
                Some(runs) if !runs.is_empty() => {
                    let mut off = cursor;
                    for &(rlen, id) in runs {
                        let rlen = (rlen as usize).min(cursor + len - off);
                        if rlen == 0 {
                            continue;
                        }
                        // Merge across the line boundary too when the style did
                        // not change: fewer sections for epaint to hash.
                        if last_id == Some(id) {
                            if let Some(s) = job.sections.last_mut() {
                                s.byte_range.end = off + rlen;
                                off += rlen;
                                continue;
                            }
                        }
                        job.sections.push(LayoutSection {
                            leading_space: 0.0,
                            byte_range: off..off + rlen,
                            format: self.interner.get(id).to_format(&font_id),
                        });
                        last_id = Some(id);
                        off += rlen;
                    }
                    // A run table that does not cover the line (only possible
                    // if the text moved under us) is padded plain rather than
                    // producing a job epaint would reject.
                    if off < cursor + len {
                        push_plain(&mut job, off, cursor + len, &plain_fmt);
                        last_id = None;
                    }
                }
                _ => {
                    push_plain(&mut job, cursor, cursor + len, &plain_fmt);
                    last_id = None;
                }
            }
            cursor += len;
        }
        if job.sections.is_empty() && !slice.is_empty() {
            push_plain(&mut job, 0, slice.len(), &plain_fmt);
        }
        self.section_hint.set(job.sections.len());
        job
    }
}

/// The longest prefix of `s` that is at most `max_bytes` long and still ends
/// on a char boundary.
fn cap_slice(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn push_plain(job: &mut LayoutJob, from: usize, to: usize, fmt: &TextFormat) {
    if to <= from {
        return;
    }
    if let Some(last) = job.sections.last_mut() {
        if last.byte_range.end == from && last.format == *fmt {
            last.byte_range.end = to;
            return;
        }
    }
    job.sections.push(LayoutSection {
        leading_space: 0.0,
        byte_range: from..to,
        format: fmt.clone(),
    });
}

/// Byte offset of every line start, matching `LinesWithEndings`: a trailing
/// newline does not open an extra empty line.
fn build_line_starts(text: &str, out: &mut Vec<usize>) {
    out.clear();
    if text.is_empty() {
        return;
    }
    out.push(0);
    // `match_indices` on a char pattern is memchr-backed, which matters at
    // 5.8 MB: a hand-rolled byte loop is several times slower.
    for (i, _) in text.match_indices('\n') {
        if i + 1 < text.len() {
            out.push(i + 1);
        }
    }
}

/// The first line whose content differs between two versions of a document.
///
/// The keystroke path has no splice information, only the old and new strings,
/// so the common prefix is the only thing to go on. Counting newlines in the
/// prefix is O(prefix) but memchr-backed and runs once per edit.
pub fn first_changed_line(old: &str, new: &str) -> usize {
    let a = old.as_bytes();
    let b = new.as_bytes();
    let n = a.len().min(b.len());
    let mut i = 0;
    while i < n && a[i] == b[i] {
        i += 1;
    }
    // Back up to a char boundary so the newline count below cannot split a
    // multi-byte character.
    while i > 0 && !old.is_char_boundary(i) {
        i -= 1;
    }
    old[..i].matches('\n').count()
}

/// The initial full pass, off the UI thread.
///
/// Sends contiguous batches from line 0. Exits early as soon as the receiver is
/// gone, which is how `reset` cancels it.
fn background_pass(
    text: String,
    ps: Arc<SyntaxSet>,
    syntax_name: String,
    theme: Theme,
    stride: usize,
    tx: std::sync::mpsc::Sender<BgBatch>,
) {
    let Some(syntax) = ps.find_syntax_by_name(&syntax_name) else { return };
    let highlighter = Highlighter::new(&theme);
    let mut parse = ParseState::new(syntax);
    let mut hstate = HighlightState::new(&highlighter, ScopeStack::new());
    let mut interner = Interner::default();
    let mut sent_styles = 0usize;

    let mut starts = Vec::new();
    build_line_starts(&text, &mut starts);
    let total = starts.len();

    let mut first_line = 0usize;
    let mut runs_batch: Vec<Vec<(u32, u16)>> = Vec::with_capacity(BACKGROUND_BATCH_LINES);
    let mut ckpts: Vec<(usize, Box<(ParseState, HighlightState)>)> = Vec::new();

    for line in 0..total {
        let start = starts[line];
        let end = starts.get(line + 1).copied().unwrap_or(text.len());
        let src = &text[start..end];
        let ops = parse.parse_line(src, &ps).unwrap_or_default();
        let mut runs: Vec<(u32, u16)> = Vec::new();
        for (style, piece) in HighlightIterator::new(&mut hstate, &ops[..], src, &highlighter) {
            if piece.is_empty() {
                continue;
            }
            let id = interner.intern(StyleKey::from_syntect(&style));
            match runs.last_mut() {
                Some(last) if last.1 == id => last.0 += piece.len() as u32,
                _ => runs.push((piece.len() as u32, id)),
            }
        }
        runs_batch.push(runs);
        if (line + 1) % stride == 0 {
            ckpts.push((line, Box::new((parse.clone(), hstate.clone()))));
        }
        if runs_batch.len() >= BACKGROUND_BATCH_LINES || line + 1 == total {
            let new_styles = interner.keys[sent_styles..].to_vec();
            sent_styles = interner.keys.len();
            let batch = BgBatch {
                first_line,
                runs: std::mem::take(&mut runs_batch),
                new_styles,
                checkpoints: std::mem::take(&mut ckpts),
                total_lines: total,
            };
            first_line = line + 1;
            if tx.send(batch).is_err() {
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything about a job that has to match: the text, and every section's
    /// byte range and visible formatting.
    fn fingerprint(job: &LayoutJob) -> (String, Vec<(usize, usize, [u8; 4], bool, bool)>) {
        let secs = job
            .sections
            .iter()
            .map(|s| {
                (
                    s.byte_range.start,
                    s.byte_range.end,
                    s.format.color.to_array(),
                    s.format.italics,
                    s.format.underline.width > 0.0,
                )
            })
            .collect();
        (job.text.clone(), secs)
    }

    fn font() -> FontId {
        FontId::monospace(12.0)
    }

    fn full(ext: &str, text: &str) -> LayoutJob {
        let mut hl = DocHighlight::new(ext);
        hl.advance(text, Budget::unlimited());
        hl.layout_job(text, font(), Color32::GRAY, None, f32::INFINITY)
    }

    /// Apply an edit, tell the highlighter where it started, and run to
    /// convergence exactly as a frame loop would.
    fn edit(hl: &mut DocHighlight, text: &mut String, next: String) {
        let line = first_changed_line(text, &next);
        *text = next;
        hl.note_edit(line);
        let mut guard = 0;
        while hl.advance(text, Budget::unlimited()) {
            guard += 1;
            assert!(guard < 64, "advance never settled");
        }
    }

    fn fixture(ext: &str) -> String {
        match ext {
            "rs" => concat!(
                "//! header\n",
                "use std::fmt;\n",
                "\n",
                "/* an unterminated looking block\n",
                "   spanning several lines */\n",
                "pub struct Wow {\n",
                "    hi: u64,\n",
                "}\n",
                "\n",
                "fn main() {\n",
                "    let s = r#\"raw \"string\" with # marks\"#;\n",
                "    println!(\"{s}\");\n",
                "}\n",
            ),
            "ts" => concat!(
                "import { x } from './y';\n",
                "/* block\n",
                "   comment */\n",
                "export interface Thing {\n",
                "  name: string;\n",
                "  count?: number;\n",
                "}\n",
                "const f = (a: Thing): string => `hi ${a.name}`;\n",
                "export default f;\n",
            ),
            "py" => concat!(
                "import os\n",
                "\n",
                "def main():\n",
                "    s = '''triple\n",
                "    quoted'''\n",
                "    print(f\"{s} {os.sep}\")\n",
                "\n",
                "if __name__ == '__main__':\n",
                "    main()\n",
            ),
            "go" => concat!(
                "package main\n",
                "\n",
                "import \"fmt\"\n",
                "\n",
                "/* block\n",
                "   comment */\n",
                "func main() {\n",
                "\ts := `raw\n",
                "\tstring`\n",
                "\tfmt.Println(s)\n",
                "}\n",
            ),
            "c" => concat!(
                "#include <stdio.h>\n",
                "\n",
                "/* block\n",
                "   comment */\n",
                "int main(void) {\n",
                "    const char *s = \"hi\\n\";\n",
                "    printf(s);\n",
                "    return 0;\n",
                "}\n",
            ),
            "json" => concat!(
                "{\n",
                "  \"a\": 1,\n",
                "  \"b\": [true, false, null],\n",
                "  \"c\": {\n",
                "    \"d\": \"text\"\n",
                "  }\n",
                "}\n",
            ),
            "toml" => concat!(
                "# a comment\n",
                "[package]\n",
                "name = \"thing\"\n",
                "version = \"0.1.0\"\n",
                "\n",
                "[deps]\n",
                "serde = { version = \"1\", features = [\"derive\"] }\n",
            ),
            "md" => concat!(
                "# Title\n",
                "\n",
                "Some *emphasis* and `code`.\n",
                "\n",
                "```rust\n",
                "fn x() {}\n",
                "```\n",
                "\n",
                "- one\n",
                "- two\n",
            ),
            "html" => concat!(
                "<!doctype html>\n",
                "<html>\n",
                "<!-- a comment\n",
                "     over lines -->\n",
                "<body>\n",
                "  <p class=\"x\">hi</p>\n",
                "  <script>var a = 1;</script>\n",
                "</body>\n",
                "</html>\n",
            ),
            "css" => concat!(
                "/* block\n",
                "   comment */\n",
                ":root {\n",
                "  --c: #ff0000;\n",
                "}\n",
                "body > p.x {\n",
                "  color: var(--c);\n",
                "}\n",
            ),
            _ => unreachable!(),
        }
        .to_string()
    }

    /// Insert `what` at the start of line `line`.
    fn insert_line(text: &str, line: usize, what: &str) -> String {
        let mut starts = Vec::new();
        build_line_starts(text, &mut starts);
        let at = starts.get(line).copied().unwrap_or(text.len());
        let mut out = String::with_capacity(text.len() + what.len());
        out.push_str(&text[..at]);
        out.push_str(what);
        out.push_str(&text[at..]);
        out
    }

    #[test]
    fn incremental_matches_full_highlight_for_every_language() {
        for ext in ["rs", "ts", "py", "go", "c", "json", "toml", "md", "html", "css"] {
            let base = fixture(ext);
            let mut hl = DocHighlight::new(ext);
            assert!(!hl.is_plain(), "{ext} has no grammar");
            let mut text = base.clone();
            hl.advance(&text, Budget::unlimited());

            let mid = text.len() / 2;
            let mid = (mid..text.len()).find(|i| text.is_char_boundary(*i)).unwrap();

            // 1. insert a char mid-file
            let mut with_char = text.clone();
            with_char.insert(mid, 'Z');
            edit(&mut hl, &mut text, with_char);

            // 2. delete it again
            let mut without = text.clone();
            without.remove(mid);
            edit(&mut hl, &mut text, without);

            // 3. open a block comment near the top
            let opened = insert_line(&text, 1, "/* opened\n");
            edit(&mut hl, &mut text, opened);

            // 4. close it
            let closed = insert_line(&text, 2, "closed */\n");
            edit(&mut hl, &mut text, closed);

            // 5. insert a raw string
            let raw = insert_line(&text, 3, "let q = r#\"a \"raw\" # string\"#;\n");
            edit(&mut hl, &mut text, raw);

            // 6. insert a markdown fence
            let fenced = insert_line(&text, 4, "```rust\nfn fenced() {}\n```\n");
            edit(&mut hl, &mut text, fenced);

            let inc = hl.layout_job(&text, font(), Color32::GRAY, None, f32::INFINITY);
            let scratch = full(ext, &text);
            assert_eq!(
                fingerprint(&inc),
                fingerprint(&scratch),
                "incremental and full highlight disagree for .{ext}"
            );
        }
    }

    #[test]
    fn convergence_is_bounded_after_a_single_character_edit() {
        let mut text = String::new();
        for i in 0..2000 {
            text.push_str(&format!("fn f{i}() -> u32 {{ let v = {i}; v + 1 }}\n"));
        }
        let mut hl = DocHighlight::new("rs");
        while hl.advance(&text, Budget::unlimited()) {}
        assert_eq!(hl.line_count(), 2000);

        // Edit in the middle, at the start of line 1000.
        let edited = insert_line(&text, 1000, "// note\n");
        let line = first_changed_line(&text, &edited);
        assert_eq!(line, 1000);
        text = edited;
        hl.note_edit(line);
        let more = hl.advance(&text, Budget::unlimited());
        assert!(!more, "a one-line insert should settle in one pass");
        assert!(
            hl.last_advance_lines() <= 8,
            "convergence took {} lines, expected a handful",
            hl.last_advance_lines()
        );
    }

    #[test]
    fn checkpoint_memory_stays_under_the_cap_on_a_large_document() {
        // The stride policy is what bounds the memory, so assert it directly at
        // 150 k lines rather than paying a full parse of 5.8 MB in a debug test.
        let lines = 150_000;
        let stride = DocHighlight::stride_for(lines);
        assert!(stride > 1, "150 k lines must not checkpoint every line");
        let bytes = lines.div_ceil(stride) * CHECKPOINT_BYTES;
        assert!(
            bytes <= CHECKPOINT_MEM_CAP,
            "modelled checkpoints {bytes} B over the {CHECKPOINT_MEM_CAP} B cap"
        );
        // Small documents checkpoint every line, which is what makes
        // convergence detectable immediately.
        assert_eq!(DocHighlight::stride_for(2_000), 1);

        // And the real cache obeys the stride: walk part of a synthetic
        // 150 k-line document and count what it actually stored.
        let mut text = String::with_capacity(lines * 20);
        for i in 0..lines {
            text.push_str("let x = 1; // ");
            text.push_str(&(i % 97).to_string());
            text.push('\n');
        }
        let mut hl = DocHighlight::new("rs");
        hl.advance(&text, Budget::lines(20_000));
        assert_eq!(hl.line_count(), lines);
        assert_eq!(DocHighlight::stride_for(lines), hl.stride);
        assert!(
            hl.estimated_checkpoint_bytes() <= CHECKPOINT_MEM_CAP,
            "stored checkpoints {} B over the cap",
            hl.estimated_checkpoint_bytes()
        );
        // 20 000 lines walked at this stride is 20 000 / stride checkpoints.
        assert_eq!(hl.checkpoint_count(), 20_000 / stride);
    }

    #[test]
    fn a_range_whose_runs_were_trimmed_is_coloured_again_on_the_way_back() {
        // Big enough that the section window cannot hold the whole document,
        // which is what makes trimming fire at all.
        let lines = SECTION_WINDOW_LINES * 3;
        let mut text = String::with_capacity(lines * 24);
        for i in 0..lines {
            text.push_str("let x = \"s");
            text.push_str(&(i % 97).to_string());
            text.push_str("\"; // note\n");
        }
        let mut hl = DocHighlight::new("rs");
        while hl.advance(&text, Budget::unlimited()) {}

        let top = 2..10;
        let coloured = hl.layout_job(&text, font(), Color32::GRAY, Some(top.clone()), f32::INFINITY);
        assert!(coloured.sections.len() > top.len(), "the top was never coloured to begin with");

        // Scroll far away. The next `advance` trims the top's runs, exactly as
        // it does when the reader jumps to the end of a large file. The view
        // asks one line at a time and then reports the span, so this does too:
        // reporting is the only thing that tells the window it is wider than
        // the last row painted.
        let far = lines - 10..lines - 2;
        for i in far.clone() {
            let _ = hl.layout_job(&text, font(), Color32::GRAY, Some(i..i + 1), f32::INFINITY);
        }
        hl.note_visible(far);
        while hl.advance(&text, Budget::unlimited()) {}

        // Scroll back. The jobs asked for right after the jump may still be
        // plain, because the refill happens in `advance`; by the next frame
        // they have to be what the same range gave before.
        for i in top.clone() {
            let _ = hl.layout_job(&text, font(), Color32::GRAY, Some(i..i + 1), f32::INFINITY);
        }
        hl.note_visible(top.clone());
        while hl.advance(&text, Budget::unlimited()) {}
        let again = hl.layout_job(&text, font(), Color32::GRAY, Some(top), f32::INFINITY);
        assert_eq!(again.text, coloured.text);
        assert_eq!(
            again.sections.len(),
            coloured.sections.len(),
            "a range that came back into view lost its colour"
        );
        for (a, b) in again.sections.iter().zip(coloured.sections.iter()) {
            assert_eq!(a.byte_range, b.byte_range);
            assert_eq!(a.format.color, b.format.color);
        }
    }

    #[test]
    fn layout_job_for_a_line_range_is_relative_to_the_range() {
        let text = fixture("rs");
        let mut hl = DocHighlight::new("rs");
        while hl.advance(&text, Budget::unlimited()) {}

        let whole = hl.layout_job(&text, font(), Color32::GRAY, None, f32::INFINITY);
        let range = 4..8;
        let part = hl.layout_job(&text, font(), Color32::GRAY, Some(range.clone()), f32::INFINITY);

        let mut starts = Vec::new();
        build_line_starts(&text, &mut starts);
        let base = starts[range.start];
        let end = starts[range.end];
        assert_eq!(part.text, text[base..end]);

        // The same sections the whole-document job has for those bytes, clipped
        // to the range and shifted down by its start offset. Clipping rather
        // than filtering because a style run that straddles the first line of
        // the range is legitimately cut short by it.
        let expected: Vec<(usize, usize, [u8; 4], bool, bool)> = fingerprint(&whole)
            .1
            .into_iter()
            .filter(|(s, e, ..)| *e > base && *s < end)
            .map(|(s, e, c, i, u)| (s.max(base) - base, e.min(end) - base, c, i, u))
            .collect();
        assert_eq!(fingerprint(&part).1, expected);
        assert_eq!(part.sections.first().unwrap().byte_range.start, 0);
        assert_eq!(part.sections.last().unwrap().byte_range.end, end - base);
    }

    #[test]
    fn two_face_covers_the_extensions_the_default_set_misses() {
        for ext in ["ts", "tsx", "jsx", "mjs", "toml"] {
            assert!(
                resolve_syntax(ext).is_some(),
                ".{ext} still has no grammar after adding two-face"
            );
        }
    }

    #[test]
    fn an_unknown_extension_lays_out_plain() {
        let mut hl = DocHighlight::new("zzzz");
        assert!(hl.is_plain());
        let text = "anything at all\nsecond line\n";
        assert!(!hl.advance(text, Budget::unlimited()));
        let job = hl.layout_job(text, font(), Color32::GRAY, None, f32::INFINITY);
        assert_eq!(job.text, text);
        assert_eq!(job.sections.len(), 1);
        assert_eq!(job.sections[0].format.color, Color32::GRAY);
    }

    #[test]
    fn italic_is_not_underlined() {
        // The upstream bug read FontStyle::ITALIC into the underline stroke.
        let italic = StyleKey { r: 1, g: 2, b: 3, bold: false, italic: true, underline: false };
        let fmt = italic.to_format(&font());
        assert!(fmt.italics);
        assert_eq!(fmt.underline, eframe::egui::Stroke::NONE);
        let underlined = StyleKey { underline: true, ..italic };
        assert!(underlined.to_format(&font()).underline.width > 0.0);
    }

    #[test]
    fn first_changed_line_counts_the_common_prefix() {
        // Two identical strings share the whole prefix, so the answer is one
        // past the last line: nothing to invalidate.
        assert_eq!(first_changed_line("a\nb\nc\n", "a\nb\nc\n"), 3);
        assert_eq!(first_changed_line("a\nb\nc\n", "a\nX\nc\n"), 1);
        assert_eq!(first_changed_line("a\nb\nc\n", "X\nb\nc\n"), 0);
        // An appended line: the prefix ends exactly on the new line's start.
        assert_eq!(first_changed_line("a\nb\n", "a\nb\nc\n"), 2);
        // A multi-byte character must not be split by the prefix scan.
        assert_eq!(first_changed_line("é\nx\n", "é\ny\n"), 1);
    }

    /// epaint asserts that a job's sections are contiguous and cover its text,
    /// and panics otherwise. Push every fixture through a real `Context` so a
    /// malformed job is caught here rather than on the first frame the user
    /// opens that language.
    #[test]
    fn every_job_lays_out_through_epaint() {
        let ctx = eframe::egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {});
        for ext in ["rs", "ts", "py", "go", "c", "json", "toml", "md", "html", "css"] {
            let text = fixture(ext);
            let mut hl = DocHighlight::new(ext);
            while hl.advance(&text, Budget::unlimited()) {}

            let whole = hl.layout_job(&text, font(), Color32::GRAY, None, 400.0);
            let galley = ctx.fonts(|f| f.layout_job(whole));
            assert_eq!(galley.text(), text, "galley text differs for .{ext}");

            let part = hl.layout_job(&text, font(), Color32::GRAY, Some(2..5), 400.0);
            let expect = part.text.clone();
            let galley = ctx.fonts(|f| f.layout_job(part));
            assert_eq!(galley.text(), expect, "windowed galley differs for .{ext}");
        }
    }

    #[test]
    fn a_reset_reparses_from_scratch() {
        let mut hl = DocHighlight::new("rs");
        let a = fixture("rs");
        while hl.advance(&a, Budget::unlimited()) {}
        let b = "fn other() {}\n// tail\n".to_string();
        hl.reset();
        while hl.advance(&b, Budget::unlimited()) {}
        let inc = hl.layout_job(&b, font(), Color32::GRAY, None, f32::INFINITY);
        assert_eq!(fingerprint(&inc), fingerprint(&full("rs", &b)));
    }

    #[test]
    fn the_background_pass_delivers_the_same_colours() {
        let mut text = String::new();
        for i in 0..600 {
            text.push_str(&format!("/// doc {i}
pub fn g{i}() -> &'static str {{ \"s{i}\" }}
"));
        }
        let mut hl = DocHighlight::new("rs");
        hl.always_background();
        // A one-line budget keeps the foreground out of the way, so most of the
        // colour can only have come off the thread.
        let mut spins = 0;
        let mut saw_pass = false;
        while hl.advance(&text, Budget::lines(1)) {
            saw_pass |= hl.progress().is_some();
            spins += 1;
            assert!(spins < 200_000, "background pass never finished");
        }
        assert!(saw_pass, "no background pass ever ran");
        assert!(hl.highlighted_lines() >= 1200);
        assert_eq!(
            fingerprint(&hl.layout_job(&text, font(), Color32::GRAY, None, f32::INFINITY)),
            fingerprint(&full("rs", &text))
        );
    }

    #[test]
    fn an_edit_during_the_background_pass_discards_the_racing_lines() {
        let mut text = String::new();
        for i in 0..600 {
            text.push_str(&format!("pub const K{i}: u32 = {i}; // note {i}
"));
        }
        let mut hl = DocHighlight::new("rs");
        hl.always_background();
        // Let the pass get going, then open a block comment at line 5: every
        // line from there on must be re-derived, not taken from the snapshot.
        hl.advance(&text, Budget::lines(1));
        let edited = insert_line(&text, 5, "/* opened here
");
        let line = first_changed_line(&text, &edited);
        assert_eq!(line, 5);
        text = edited;
        hl.note_edit(line);
        let mut spins = 0;
        while hl.advance(&text, Budget::lines(64)) {
            spins += 1;
            assert!(spins < 200_000, "never settled after the racing edit");
        }
        assert_eq!(
            fingerprint(&hl.layout_job(&text, font(), Color32::GRAY, None, f32::INFINITY)),
            fingerprint(&full("rs", &text))
        );
    }

    #[test]
    fn a_budgeted_walk_resumes_and_finishes() {
        let mut text = String::new();
        for i in 0..400 {
            text.push_str(&format!("let v{i} = {i};\n"));
        }
        let mut hl = DocHighlight::new("rs");
        let mut passes = 0;
        while hl.advance(&text, Budget::lines(37)) {
            passes += 1;
            assert!(passes < 100);
        }
        assert!(passes > 5, "a 37-line budget should need several passes");
        assert_eq!(fingerprint(&hl.layout_job(&text, font(), Color32::GRAY, None, f32::INFINITY)),
                   fingerprint(&full("rs", &text)));
    }
}
