//! Headless benchmark for the incremental highlighter in `src/highlight.rs`.
//!
//! Prints, for synthetic documents of 64 KiB, 70 KB, 1 MiB and 5.8 MB:
//! cold full-highlight time, per-keystroke incremental cost at the top, the
//! middle and the end of the document, steady-state layout cost with no edit,
//! time to first coloured paint, and the memory the cache holds.
//!
//! Run with `cargo run --release --example hlbench`. A debug build measures the
//! debug build and is not worth reading.
//!
//! The module is pulled in by path rather than through the crate, because the
//! crate has no library target yet (that is Stage 0 item 6 of the editor-memory
//! plan); when `src/lib.rs` lands this becomes a plain `use`.

#![allow(dead_code)]

#[path = "../src/highlight.rs"]
mod highlight;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use eframe::egui::{Color32, FontId};

use highlight::{Budget, DocHighlight};

// --- counting allocator ----------------------------------------------------

static LIVE: AtomicUsize = AtomicUsize::new(0);

/// Tracks live heap bytes so the cache's real footprint can be reported rather
/// than modelled. Cheap enough not to distort the timings that matter here,
/// but the absolute times are still a touch pessimistic because of it.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        LIVE.fetch_add(l.size(), Ordering::Relaxed);
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        if n >= l.size() {
            LIVE.fetch_add(n - l.size(), Ordering::Relaxed);
        } else {
            LIVE.fetch_sub(l.size() - n, Ordering::Relaxed);
        }
        System.realloc(p, l, n)
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}

// --- fixtures --------------------------------------------------------------

/// Rust source of roughly `target` bytes, with enough variety that the parser
/// does real work: comments, strings, generics and nesting.
fn synth_rust(target: usize) -> String {
    let mut s = String::with_capacity(target + 256);
    let mut i = 0usize;
    while s.len() < target {
        s.push_str(&format!(
            "/// Item {i} does a thing.\n\
             pub fn item_{i}<T: Clone + std::fmt::Debug>(input: &[T], flag: bool) -> Option<T> {{\n\
             \x20   let label = \"item {i} label\"; // trailing note\n\
             \x20   if flag && !input.is_empty() {{\n\
             \x20       println!(\"{{label}} -> {{:?}}\", input[0]);\n\
             \x20       return Some(input[0].clone());\n\
             \x20   }}\n\
             \x20   None\n\
             }}\n\n"
        ));
        i += 1;
    }
    s
}

fn font() -> FontId {
    FontId::monospace(12.0)
}

const PLAIN: Color32 = Color32::from_rgb(0xc0, 0xc0, 0xc0);

/// A screenful.
const VISIBLE_ROWS: usize = 100;

fn line_starts(text: &str) -> Vec<usize> {
    let mut v = vec![0usize];
    for (i, _) in text.match_indices('\n') {
        if i + 1 < text.len() {
            v.push(i + 1);
        }
    }
    v
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn pct(v: &mut Vec<Duration>, p: f64) -> Duration {
    v.sort_unstable();
    let idx = (((v.len() - 1) as f64) * p).round() as usize;
    v[idx]
}

// --- measurements ----------------------------------------------------------

/// Cold: a fresh highlighter over the whole document, no budget.
fn cold_full(text: &str) -> (Duration, usize) {
    let mut hl = DocHighlight::new("rs");
    let t = Instant::now();
    while hl.advance(text, Budget::unlimited()) {}
    (t.elapsed(), hl.highlighted_lines())
}

/// Frames until the top `VISIBLE_ROWS` lines are coloured, driven exactly as
/// the editor drives it: `Budget::frame()` once per frame.
fn time_to_first_paint(text: &str) -> (Duration, usize) {
    let mut hl = DocHighlight::new("rs");
    let t = Instant::now();
    let mut frames = 0usize;
    loop {
        hl.advance(text, Budget::frame());
        let _ = hl.layout_job(text, font(), PLAIN, Some(0..VISIBLE_ROWS), f32::INFINITY);
        frames += 1;
        if hl.highlighted_lines() >= VISIBLE_ROWS.min(hl.line_count()) {
            break;
        }
        if frames > 10_000 {
            break;
        }
    }
    (t.elapsed(), frames)
}

/// Per-keystroke cost: insert one character at the start of `line`, then time
/// `advance` to convergence plus the `layout_job` the layouter would build.
///
/// `whole` picks the job the current `TextEdit` path asks for (the entire
/// document) rather than the visible window a virtualised view would.
fn keystroke(text: &str, line: usize, reps: usize, whole: bool) -> Vec<Duration> {
    let starts = line_starts(text);
    let at = starts[line.min(starts.len() - 1)];
    let mut doc = text.to_string();
    let mut hl = DocHighlight::new("rs");
    while hl.advance(&doc, Budget::unlimited()) {}

    let visible = if whole {
        None
    } else {
        Some(line..(line + VISIBLE_ROWS).min(starts.len()))
    };
    let mut out = Vec::with_capacity(reps);
    for n in 0..reps {
        // Alternate insert and delete so the document does not grow without
        // bound and every sample is a real one-character edit.
        let t = if n % 2 == 0 {
            doc.insert(at, 'x');
            let t = Instant::now();
            hl.note_edit(line);
            while hl.advance(&doc, Budget::unlimited()) {}
            let _ = hl.layout_job(&doc, font(), PLAIN, visible.clone(), f32::INFINITY);
            t.elapsed()
        } else {
            doc.remove(at);
            let t = Instant::now();
            hl.note_edit(line);
            while hl.advance(&doc, Budget::unlimited()) {}
            let _ = hl.layout_job(&doc, font(), PLAIN, visible.clone(), f32::INFINITY);
            t.elapsed()
        };
        out.push(t);
    }
    out
}

/// Steady state: no edit at all, just the job the layouter builds every frame.
fn steady_layout(text: &str, reps: usize, whole: bool) -> Vec<Duration> {
    let mut hl = DocHighlight::new("rs");
    while hl.advance(text, Budget::unlimited()) {}
    let visible = if whole { None } else { Some(0..VISIBLE_ROWS) };
    let mut out = Vec::with_capacity(reps);
    for _ in 0..reps {
        let t = Instant::now();
        let _ = hl.advance(text, Budget::frame());
        let job = hl.layout_job(text, font(), PLAIN, visible.clone(), f32::INFINITY);
        out.push(t.elapsed());
        std::hint::black_box(job.sections.len());
    }
    out
}

/// Live heap held by a fully populated cache, before and after the section
/// window is trimmed around a visible range.
///
/// The untrimmed figure is what the current `TextEdit` path holds, because it
/// asks for the whole document and every line's runs have to stay. The trimmed
/// figure is what a virtualised view holds.
fn cache_memory(text: &str) -> (usize, usize, usize, usize) {
    let before = live();
    let mut hl = DocHighlight::new("rs");
    while hl.advance(text, Budget::unlimited()) {}
    // Whole-document request: nothing is trimmed.
    let _ = hl.layout_job(text, font(), PLAIN, None, f32::INFINITY);
    let untrimmed = live().saturating_sub(before);
    let ckpts = hl.checkpoint_count();

    // Ask for a window, then let `advance` trim to it.
    let _ = hl.layout_job(text, font(), PLAIN, Some(0..VISIBLE_ROWS), f32::INFINITY);
    hl.advance(text, Budget::frame());
    let trimmed = live().saturating_sub(before);
    let cached = hl.cached_run_lines();
    (untrimmed, trimmed, ckpts, cached)
}

fn report(name: &str, text: &str, keystroke_reps: usize) {
    let lines = line_starts(text).len();
    println!("\n== {name}: {} B, {lines} lines ==", text.len());

    let (cold, done) = cold_full(text);
    println!(
        "  cold full highlight      {:8.2} ms   ({:.0} KB/s, {done} lines)",
        ms(cold),
        text.len() as f64 / 1024.0 / cold.as_secs_f64()
    );

    let (ttfp, frames) = time_to_first_paint(text);
    println!("  first coloured paint     {:8.2} ms   ({frames} frame(s), top {VISIBLE_ROWS} lines)", ms(ttfp));

    for (label, line) in [("top", 0usize), ("middle", lines / 2), ("end", lines.saturating_sub(2))] {
        let mut win = keystroke(text, line, keystroke_reps, false);
        let mut all = keystroke(text, line, keystroke_reps, true);
        println!(
            "  keystroke {label:<7}        p50 {:6.3} / p99 {:6.3} ms  (visible window)   |  p50 {:6.3} / p99 {:6.3} ms  (whole-document job)",
            ms(pct(&mut win, 0.50)),
            ms(pct(&mut win, 0.99)),
            ms(pct(&mut all, 0.50)),
            ms(pct(&mut all, 0.99)),
        );
    }

    let mut win = steady_layout(text, 200, false);
    let mut all = steady_layout(text, 50, true);
    println!(
        "  steady state, no edit    p50 {:6.3} / p99 {:6.3} ms  (visible window)   |  p50 {:6.3} / p99 {:6.3} ms  (whole-document job)",
        ms(pct(&mut win, 0.50)),
        ms(pct(&mut win, 0.99)),
        ms(pct(&mut all, 0.50)),
        ms(pct(&mut all, 0.99)),
    );

    let (untrimmed, trimmed, ckpts, cached) = cache_memory(text);
    println!(
        "  cache memory             {:8.2} MB whole-document   |  {:8.2} MB windowed ({cached} lines of runs kept, {ckpts} checkpoints)",
        untrimmed as f64 / (1024.0 * 1024.0),
        trimmed as f64 / (1024.0 * 1024.0),
    );
}

fn main() {
    let t = Instant::now();
    let before = live();
    let n = highlight::syntax_count();
    let asset_bytes = live() - before;
    println!(
        "assets: {n} syntaxes + 2 themes loaded in {:.2} ms, {:.2} MB resident",
        ms(t.elapsed()),
        asset_bytes as f64 / (1024.0 * 1024.0)
    );

    report("64 KiB", &synth_rust(64 * 1024), 200);
    report("70 KB", &synth_rust(70 * 1000), 200);
    report("1 MiB", &synth_rust(1024 * 1024), 100);
    report("5.8 MB", &synth_rust(5_800_000), 30);
}
