//! Headless memory and latency harness for the editor's two rendering paths.
//!
//! The claim the virtualised view makes is not "uses less memory", it is
//! "cost is the viewport, not the document". That only shows up as a ratio, so
//! every case here is measured the same way and compared against the smallest
//! one rather than against a round number.
//!
//! How the numbers are taken:
//!
//! * A `#[global_allocator]` in this test binary counts live and peak heap.
//!   `dhat` reports rather than asserts, `stats_alloc` has no high-water mark
//!   and `std::alloc` has no statistics at all, so thirty lines in-tree it is.
//!   It was calibrated against the struct arithmetic on egui 0.29 (a glyph is
//!   64 bytes, its four vertices 80 and its six indices 24, so 171 a
//!   character) and read 170.8 bytes a character on a 5.8 MB galley.
//! * "Layout bytes" is the live heap that disappears when the editor stops
//!   being drawn for two frames. `GalleyCache::flush_cache` keeps only the
//!   galleys used in the frame just ended (`epaint-0.29.1/src/text/fonts.rs`),
//!   so what evaporates is exactly the laid-out text the view was holding, and
//!   nothing of the document, the line index or the highlighter's cache.
//! * `egui::Context::run` needs no window and no GPU, but `ctx.fonts` panics
//!   before the first run, so every context gets a warm-up frame first.
//!
//! Everything lives in one `#[test]` on purpose: the allocator counters are
//! process-wide, and a second test running beside this one would be measured
//! into it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use eframe::egui;
use the_real_explorer::editor::Editor;

// --- counting allocator ----------------------------------------------------

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// `System`, plus a live total and a high-water mark. Two relaxed atomics per
/// allocation is enough to distort a microbenchmark and nowhere near enough to
/// distort a frame.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            let n = LIVE.fetch_add(l.size(), Relaxed) + l.size();
            PEAK.fetch_max(n, Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Relaxed);
        System.dealloc(p, l);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = System.realloc(p, l, new);
        if !q.is_null() {
            if new > l.size() {
                let n = LIVE.fetch_add(new - l.size(), Relaxed) + (new - l.size());
                PEAK.fetch_max(n, Relaxed);
            } else {
                LIVE.fetch_sub(l.size() - new, Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn live() -> usize {
    LIVE.load(Relaxed)
}

fn peak() -> usize {
    PEAK.load(Relaxed)
}

/// Move the high-water mark down to the current live total, so the next read
/// of [`peak`] is a measurement of what happened after this call.
fn arm_peak() {
    PEAK.store(LIVE.load(Relaxed), Relaxed);
}

fn mb(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

// --- fixtures --------------------------------------------------------------

/// A document of roughly `bytes` bytes, shaped like the markdown notes this
/// was measured against: short lines, a little inline code, nothing that
/// makes the parser interesting.
fn synth(bytes: usize) -> String {
    let line = "- `compute(a, b)` returns **a * b + 17**, the filler line.\n";
    let mut s = String::with_capacity(bytes + line.len());
    while s.len() < bytes {
        s.push_str(line);
    }
    s
}

/// The shape per-line layout degenerates on: one line, no newline anywhere.
fn synth_one_line(bytes: usize) -> String {
    let chunk = "word ";
    let mut s = String::with_capacity(bytes + chunk.len());
    while s.len() < bytes {
        s.push_str(chunk);
    }
    s
}

fn write_fixture(dir: &Path, name: &str, text: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, text).expect("write fixture");
    p
}

// --- headless egui ---------------------------------------------------------

struct Harness {
    ctx: egui::Context,
    input: egui::RawInput,
}

impl Harness {
    /// A fresh context with an empty galley cache and a built font atlas.
    fn new() -> Self {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1600.0, 1000.0))),
            ..Default::default()
        };
        let h = Self { ctx, input };
        // The fonts do not exist until a run has happened, and the first run
        // also builds the atlas; both would land in the first measurement.
        h.blank();
        h.blank();
        h
    }

    /// A frame with nothing in it. Two of these evict every galley the editor
    /// was holding, which is what makes the layout measurement possible.
    fn blank(&self) {
        let _ = self.ctx.run(self.input.clone(), |c| {
            egui::CentralPanel::default().show(c, |ui| {
                ui.label("");
            });
        });
    }

    /// One frame with the editor drawn. Returns how many text shapes it
    /// painted, which is the only proof from in here that rows were drawn at
    /// all rather than the view quietly rendering nothing.
    fn frame(&self, ed: &mut Editor) -> usize {
        self.frame_with(ed, Vec::new())
    }

    fn frame_with(&self, ed: &mut Editor, events: Vec<egui::Event>) -> usize {
        let mut input = self.input.clone();
        input.events = events;
        let out = self.ctx.run(input, |c| {
            egui::CentralPanel::default().show(c, |ui| {
                ed.show(ui, &[], false);
            });
        });
        let n = out.shapes.iter().map(|s| count_text(&s.shape)).sum();
        drop(out);
        n
    }
}

fn count_text(shape: &egui::Shape) -> usize {
    match shape {
        egui::Shape::Text(_) => 1,
        egui::Shape::Vec(v) => v.iter().map(count_text).sum(),
        _ => 0,
    }
}

/// A Ctrl+`key` press and release, as winit delivers it on Windows.
fn ctrl_key(key: egui::Key) -> Vec<egui::Event> {
    let modifiers = egui::Modifiers { alt: false, ctrl: true, shift: false, mac_cmd: false, command: true };
    vec![
        egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers },
        egui::Event::Key { key, physical_key: None, pressed: false, repeat: false, modifiers },
    ]
}

/// Run frames until the background colouring pass has nothing left to say,
/// then a few more so the galley cache holds a steady screen of rows.
fn settle(h: &Harness, ed: &mut Editor) {
    let deadline = Instant::now() + Duration::from_secs(30);
    for i in 0..4000 {
        h.frame(ed);
        let quiet = ed.docs[0].colouring_progress().is_none();
        if quiet && i >= 24 {
            break;
        }
        if Instant::now() > deadline {
            break;
        }
    }
    for _ in 0..3 {
        h.frame(ed);
    }
}

/// Live heap the editor's laid-out text is holding, in bytes.
///
/// Measured by taking it away: two frames without the editor let
/// `GalleyCache::flush_cache` drop every galley that is no longer in use, and
/// the difference is what those galleys cost. The view is drawn again before
/// returning so the caller is where it was.
fn layout_bytes(h: &Harness, ed: &mut Editor) -> usize {
    let with = live();
    h.blank();
    h.blank();
    let without = live();
    h.frame(ed);
    h.frame(ed);
    with.saturating_sub(without)
}

/// Peak heap growth over one settled frame: the transient cost of drawing,
/// as opposed to what drawing leaves behind.
fn frame_peak(h: &Harness, ed: &mut Editor) -> usize {
    let base = live();
    arm_peak();
    h.frame(ed);
    peak().saturating_sub(base)
}

/// One document opened and settled, with the numbers that describe it.
struct Case {
    label: &'static str,
    layout: usize,
    frame_peak: usize,
    text_shapes: usize,
}

fn measure(label: &'static str, path: &Path, threshold: usize) -> (Case, Harness, Editor) {
    let h = Harness::new();
    let mut ed = Editor::new();
    ed.set_large_threshold(threshold);
    assert!(ed.open(path, path.parent().unwrap(), &[]), "{label}: open refused");
    settle(&h, &mut ed);
    let text_shapes = h.frame(&mut ed);
    let layout = layout_bytes(&h, &mut ed);
    let frame_peak = frame_peak(&h, &mut ed);
    (Case { label, layout, frame_peak, text_shapes }, h, ed)
}

// --- the harness ------------------------------------------------------------

#[test]
fn virtualised_view_costs_the_viewport_not_the_document() {
    let dir = std::env::temp_dir().join("tre-large-view-harness");
    let _ = std::fs::create_dir_all(&dir);

    let small = write_fixture(&dir, "small-64k.md", &synth(64 * 1024));
    let mid = write_fixture(&dir, "mid-1m.md", &synth(1024 * 1024));
    let big = write_fixture(&dir, "big-5800k.md", &synth(5_800_000));
    let oneline = write_fixture(&dir, "oneline-5800k.md", &synth_one_line(5_800_000));

    let mut report: Vec<Case> = Vec::new();

    // 64 KiB forced onto the large path. This is the yardstick: the same
    // window, the same number of visible rows, the smallest document. If the
    // view is really viewport-bound, 5.8 MB has to land on top of this.
    let (small_large, _, _) = measure("64 KiB, large view", &small, 1);
    // 64 KiB on the path it really takes, for the ratio the plan asked for.
    let (small_edit, _, _) = measure("64 KiB, TextEdit", &small, usize::MAX);
    // 1 MiB is exactly at the threshold, so this is its real path.
    let (mid_large, _, _) = measure("1 MiB, large view", &mid, usize::MAX.min(1024 * 1024));
    // 1 MiB forced onto the TextEdit path: reported, never gated. Its
    // keystroke cost is a known Stage 2 item.
    let (mid_edit, _, _) = measure("1 MiB, TextEdit", &mid, usize::MAX);
    let (one_large, _, _) = measure("5.8 MB one line, large view", &oneline, 1024 * 1024);

    // The 5.8 MB case is kept alive: the scroll and first-paint measurements
    // below are the same document.
    let (big_large, h, mut ed) = measure("5.8 MB, large view", &big, 1024 * 1024);

    // Scrolling end to end and back. This is the real proof that cost is
    // viewport-bound: if anything were retained per row visited, it would
    // show up here and nowhere else.
    let before_scroll = live();
    for _ in 0..8 {
        h.frame_with(&mut ed, ctrl_key(egui::Key::End));
    }
    for _ in 0..8 {
        h.frame_with(&mut ed, ctrl_key(egui::Key::Home));
    }
    for _ in 0..4 {
        h.frame(&mut ed);
    }
    let scroll_delta = live().saturating_sub(before_scroll);

    // The write paths refuse above the threshold instead of half-applying.
    assert!(ed.is_large(0), "5.8 MB document did not take the large path");
    ed.save_active(false);
    assert!(ed.status.contains("Cannot save"), "save was not refused: {}", ed.status);
    ed.format_current();
    assert!(ed.status.contains("Cannot format"), "format was not refused: {}", ed.status);
    drop(ed);

    // Time to first paint: opening the document and drawing the frame that
    // shows it, on a context whose atlas already exists.
    let h2 = Harness::new();
    let mut ed2 = Editor::new();
    ed2.set_large_threshold(1024 * 1024);
    let t = Instant::now();
    assert!(ed2.open(&big, dir.as_path(), &[]), "5.8 MB open refused");
    let first_paint_shapes = h2.frame(&mut ed2);
    let first_paint = t.elapsed();
    drop(ed2);

    report.extend([small_large, small_edit, mid_large, mid_edit, one_large, big_large]);

    println!("\n--- editor layout cost, 1600x1000 window, egui 0.29 ---");
    for c in &report {
        println!(
            "{:<30} layout {:>10.2} MB   frame peak {:>8.2} MB   text shapes {:>4}",
            c.label,
            mb(c.layout),
            mb(c.frame_peak),
            c.text_shapes
        );
    }
    println!("5.8 MB time to first paint      {:>8.1} ms   ({first_paint_shapes} text shapes)", first_paint.as_secs_f64() * 1000.0);
    println!("5.8 MB scroll end and back      {:>8.2} MB live movement", mb(scroll_delta));
    println!();

    let yardstick = report[0].layout;
    let big_layout = report[5].layout;

    // The view drew something. A layout figure of zero because nothing was
    // painted would otherwise pass every threshold below.
    assert!(report[5].text_shapes >= 20, "5.8 MB large view painted only {} text shapes", report[5].text_shapes);
    assert!(first_paint_shapes >= 20, "first paint drew only {first_paint_shapes} text shapes");

    assert!(
        big_layout <= 4 * 1024 * 1024,
        "5.8 MB large view holds {:.2} MB of layout, over the 4 MB ceiling",
        mb(big_layout)
    );
    assert!(
        (big_layout as f64) <= 1.25 * yardstick as f64,
        "5.8 MB large view holds {:.2} MB of layout against {:.2} MB for 64 KiB in the same window, over 1.25x",
        mb(big_layout),
        mb(yardstick)
    );
    assert!(
        first_paint < Duration::from_millis(250),
        "5.8 MB time to first paint was {:.1} ms",
        first_paint.as_secs_f64() * 1000.0
    );
    assert!(
        scroll_delta < 10 * 1024 * 1024,
        "scrolling the 5.8 MB document end to end and back moved live memory by {:.2} MB",
        mb(scroll_delta)
    );

    let _ = std::fs::remove_dir_all(&dir);
}
