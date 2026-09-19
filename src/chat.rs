//! Collapsible chat panel with two backends.
//!
//! `ClaudeCode` drives the `claude` CLI in print mode with
//! `--output-format stream-json`, the same wire protocol the Agent SDK wraps,
//! so no SDK runtime is needed. Each send spawns one process pinned to the
//! explorer's current directory; `--resume` keeps the session across turns.
//!
//! `OpenAiCompatible` posts to any `/chat/completions` endpoint with SSE
//! streaming (Ollama, LM Studio, OpenRouter, OpenAI). Chat only, no tools;
//! the current directory listing is sent as a system message for context.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use eframe::egui;
use egui_phosphor::regular as ph;
use serde_json::{json, Value};

use crate::attach::{self, Attachment, DragPaths};
use crate::config::{Backend, Config};
use crate::harness_ui::HookEvent;

#[derive(Clone, Copy, PartialEq)]
pub enum Role {
    User,
    Assistant,
    Tool,
    Error,
}

pub struct ChatMsg {
    pub role: Role,
    pub text: String,
}

enum Event {
    Session(String),
    Hook(HookEvent),
    Text(String),
    Tool(String),
    Error(String),
    /// Cost/token accounting for the turn that just produced a `result`
    /// (Claude Code) or an `usage` object (OpenAI-compatible).
    Usage(TurnUsage),
    Done,
}

/// Transcript entries kept in memory. Older ones are dropped: the panel is
/// meant to stay open all day, and the OpenAI-compatible backend resends the
/// history on every turn, so unbounded growth costs memory and tokens.
const MAX_MESSAGES: usize = 400;
/// User and assistant turns resent as context to an OpenAI-compatible endpoint.
const MAX_HISTORY_TURNS: usize = 40;

/// Tokens and dollar cost for the last completed turn. Fields are `None`
/// rather than `0` when the backend didn't report them, so the footer omits
/// what it doesn't know instead of printing a misleading zero.
#[derive(Clone, Debug, Default, PartialEq)]
struct TurnUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cost_usd: Option<f64>,
}

impl TurnUsage {
    /// Small grey footer text, e.g. `"1.2k in / 340 out · $0.0123"`. `None`
    /// when nothing was reported at all, so the caller shows nothing rather
    /// than an empty `·` separator.
    fn format(&self) -> Option<String> {
        if self.input_tokens.is_none() && self.output_tokens.is_none() && self.cost_usd.is_none() {
            return None;
        }
        let mut parts = Vec::new();
        if self.input_tokens.is_some() || self.output_tokens.is_some() {
            let i = self.input_tokens.map(format_tokens).unwrap_or_else(|| "?".to_string());
            let o = self.output_tokens.map(format_tokens).unwrap_or_else(|| "?".to_string());
            parts.push(format!("{i} in / {o} out"));
        }
        if let Some(c) = self.cost_usd {
            parts.push(format_cost(c));
        }
        Some(parts.join(" \u{b7} "))
    }
}

/// `1234` -> `"1.2k"`, small counts pass through unchanged.
fn format_tokens(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Sub-cent turns are common with small models; show 4 decimal places for
/// those so they don't all round to "$0.00", and 2 for anything bigger.
fn format_cost(usd: f64) -> String {
    if usd < 0.01 {
        format!("${usd:.4}")
    } else {
        format!("${usd:.2}")
    }
}

/// Everything about one working directory's conversation that needs to
/// survive a folder switch: the transcript, the Claude Code session id (so
/// `--resume` still targets the right session), and the last user turn (for
/// the retry button, which needs the original `Attachment`s, not just the
/// summary text baked into the transcript).
#[derive(Default)]
struct DirEntry {
    messages: Vec<ChatMsg>,
    session_id: Option<String>,
    last_turn: Option<(String, Vec<Attachment>)>,
}

/// Per-directory transcript store. Only ever touched while the panel is
/// idle: a running turn's directory must never be swapped out from under it
/// (see `ChatPanel::turn_cwd` and the busy guard in `show`), so the map only
/// ever holds directories other than the one currently loaded into
/// `ChatPanel`'s own fields.
#[derive(Default)]
struct DirHistory {
    per_dir: HashMap<PathBuf, DirEntry>,
}

impl DirHistory {
    /// Store `outgoing` under `from` (if any directory was active before),
    /// then remove and return whatever was previously stored for `into`, or
    /// a fresh empty entry if this is the first visit to that directory.
    fn swap(&mut self, from: Option<&PathBuf>, outgoing: DirEntry, into: &PathBuf) -> DirEntry {
        if let Some(from) = from {
            self.per_dir.insert(from.clone(), outgoing);
        }
        self.per_dir.remove(into).unwrap_or_default()
    }
}

pub struct ChatPanel {
    pub open: bool,
    /// Dropped paths waiting for the next send. Cleared on send.
    attachments: Vec<Attachment>,
    /// Last refusal or dedupe note, shown under the chips.
    attach_note: String,
    /// `--agent` for the Claude Code backend. Changing it starts a new session.
    pub agent: Option<String>,
    /// (name, scope badge, description) snapshot for the picker.
    pub agents: Vec<(String, &'static str, String)>,
    /// (name, description) snapshot for the `/` picker.
    pub skills: Vec<(String, String)>,
    /// Hook receipts from the stream, drained by the app into the harness window.
    pub hook_events: Vec<HookEvent>,
    skill_pick: usize,
    input: String,
    messages: Vec<ChatMsg>,
    session_id: Option<String>,
    /// Prompt and attachments of the last turn actually sent, kept so the
    /// retry button can resend exactly what went out, not a reconstruction
    /// of the summary text shown in the transcript.
    last_turn: Option<(String, Vec<Attachment>)>,
    /// Tokens/cost from the last turn's `result` (Claude Code) or `usage`
    /// (OpenAI-compatible), shown small and grey in the header.
    last_usage: Option<TurnUsage>,
    busy: bool,
    rx: Option<Receiver<Event>>,
    /// cwd the in-flight turn was actually sent with. The header shows this
    /// instead of the explorer's live cwd while busy, since the app's cwd
    /// can move on if the user navigates mid-turn but the turn keeps
    /// running where it started.
    turn_cwd: Option<PathBuf>,
    /// Backend the in-flight turn is using, captured at send time so Stop
    /// still targets the right mechanism even if settings change mid-turn.
    active_backend: Option<Backend>,
    /// pid of the running Claude Code child, if any. Set by the worker
    /// thread right after spawn; read by the UI thread on a Stop click.
    active_pid: Arc<Mutex<Option<u32>>>,
    /// Cooperative cancellation for the OpenAI-compatible backend: an HTTP
    /// response body can't be killed like a child process, so the streaming
    /// loop polls this between lines instead.
    cancel_flag: Arc<AtomicBool>,
    /// Per-working-directory transcript store; see `DirHistory`.
    dir_history: DirHistory,
    /// Directory whose transcript is currently loaded into `messages` /
    /// `session_id` / `last_turn` above.
    current_dir: Option<PathBuf>,
}

impl ChatPanel {
    pub fn new(open: bool) -> Self {
        Self {
            open,
            attachments: Vec::new(),
            attach_note: String::new(),
            agent: None,
            agents: Vec::new(),
            skills: Vec::new(),
            hook_events: Vec::new(),
            skill_pick: 0,
            input: String::new(),
            messages: Vec::new(),
            session_id: None,
            last_turn: None,
            last_usage: None,
            busy: false,
            rx: None,
            turn_cwd: None,
            active_backend: None,
            active_pid: Arc::new(Mutex::new(None)),
            cancel_flag: Arc::new(AtomicBool::new(false)),
            dir_history: DirHistory::default(),
            current_dir: None,
        }
    }

    pub fn new_session(&mut self) {
        self.session_id = None;
        self.messages.clear();
        self.last_turn = None;
        self.last_usage = None;
    }

    /// Stop the in-flight turn. Clears `rx` immediately so anything the
    /// worker thread sends afterward (it may take a moment to unwind after
    /// being killed or seeing the cancel flag) is silently discarded rather
    /// than appended to a transcript the user already sees as stopped.
    fn stop(&mut self) {
        let confirmed = match self.active_backend {
            Some(Backend::ClaudeCode) => {
                let pid = self.active_pid.lock().ok().and_then(|mut g| g.take());
                match pid {
                    Some(pid) => kill_process_tree(pid),
                    // Stopped before the worker even recorded a pid: nothing to kill.
                    None => true,
                }
            }
            Some(Backend::OpenAiCompatible) => {
                self.cancel_flag.store(true, Ordering::SeqCst);
                true
            }
            None => true,
        };
        self.messages.push(ChatMsg {
            role: if confirmed { Role::Tool } else { Role::Error },
            text: if confirmed {
                "stopped by user".to_string()
            } else {
                "stop requested, but the process could not be confirmed dead — it may still be running".to_string()
            },
        });
        // Same cleanup as a normal Done: drop the empty assistant placeholder
        // the streaming reply was writing into.
        self.messages.retain(|m| !(m.role == Role::Assistant && m.text.is_empty()));
        self.busy = false;
        self.rx = None;
        self.turn_cwd = None;
        self.active_backend = None;
    }

    /// If `cwd` differs from the directory whose transcript is currently
    /// loaded, save the current one and load `cwd`'s. Never called while
    /// busy: the in-flight turn's directory must stay loaded so its
    /// streaming reply keeps landing in `messages`.
    fn sync_dir(&mut self, cwd: &PathBuf) {
        if self.current_dir.as_ref() == Some(cwd) {
            return;
        }
        let outgoing = DirEntry {
            messages: std::mem::take(&mut self.messages),
            session_id: self.session_id.take(),
            last_turn: self.last_turn.take(),
        };
        let incoming = self.dir_history.swap(self.current_dir.as_ref(), outgoing, cwd);
        self.messages = incoming.messages;
        self.session_id = incoming.session_id;
        self.last_turn = incoming.last_turn;
        self.current_dir = Some(cwd.clone());
    }

    /// Resend the last turn's exact prompt and attachments as a new turn.
    fn retry_last(&mut self, cwd: PathBuf, cfg: &Config) {
        if self.busy {
            return;
        }
        let Some((prompt, atts)) = self.last_turn.clone() else { return };
        self.attachments = atts;
        self.send(prompt, cwd, cfg);
    }

    /// Add a dropped path as a chip. Refuses non-UTF-8 paths and duplicates,
    /// and says so in `attach_note` rather than silently.
    pub fn attach(&mut self, path: &std::path::Path) {
        if self.attachments.iter().any(|a| a.path == path) {
            self.attach_note = format!("already attached: {}", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
            return;
        }
        match Attachment::from_path(path) {
            Ok(a) => {
                self.attach_note.clear();
                self.attachments.push(a);
            }
            Err(e) => self.attach_note = format!("not attached: {e}"),
        }
    }

    fn send(&mut self, prompt: String, cwd: PathBuf, cfg: &Config) {
        // Snapshot history before pushing, for the OpenAI backend.
        let history: Vec<Value> = self
            .messages
            .iter()
            .filter(|m| matches!(m.role, Role::User | Role::Assistant))
            .map(|m| json!({"role": if m.role == Role::User {"user"} else {"assistant"}, "content": m.text}))
            .collect();
        let history: Vec<Value> = if history.len() > MAX_HISTORY_TURNS {
            history[history.len() - MAX_HISTORY_TURNS..].to_vec()
        } else {
            history
        };

        let atts = std::mem::take(&mut self.attachments);
        self.attach_note.clear();
        // Saved before `atts`/`prompt` are consumed below, so retry can
        // resend the exact same attachments even though the transcript only
        // keeps their names.
        self.last_turn = Some((prompt.clone(), atts.clone()));
        let shown = if atts.is_empty() {
            prompt.clone()
        } else {
            let names: Vec<String> = atts.iter().map(|a| a.name()).collect();
            format!("{prompt}\n\nattached: {}", names.join(", "))
        };
        self.messages.push(ChatMsg { role: Role::User, text: shown });
        self.messages.push(ChatMsg { role: Role::Assistant, text: String::new() });
        self.busy = true;
        self.turn_cwd = Some(cwd.clone());
        self.active_backend = Some(cfg.backend);
        // Fresh state for this turn: a stale pid or a cancel flag left set
        // from a previous stop would otherwise misfire on the new one.
        if let Ok(mut g) = self.active_pid.lock() {
            *g = None;
        }
        self.cancel_flag.store(false, Ordering::SeqCst);
        let (tx, rx) = channel();
        self.rx = Some(rx);

        match cfg.backend {
            Backend::ClaudeCode => {
                let resume = self.session_id.clone();
                let mode = cfg.claude_permission_mode.clone();
                let agent = self.agent.clone();
                let pid_slot = self.active_pid.clone();
                // Attachments outside the cwd need --add-dir for the file tools to reach them.
                let mut add_dirs: Vec<PathBuf> = Vec::new();
                for a in &atts {
                    let dir = if a.kind == attach::Kind::Folder { a.path.clone() } else { a.path.parent().map(|p| p.to_path_buf()).unwrap_or_default() };
                    if !dir.starts_with(&cwd) && !add_dirs.contains(&dir) {
                        add_dirs.push(dir);
                    }
                }
                let full = attach::claude_block(&atts, &prompt);
                thread::spawn(move || run_claude(tx, full, cwd, resume, mode, agent, add_dirs, pid_slot));
            }
            Backend::OpenAiCompatible => {
                let cfg = cfg.clone();
                let cancel_flag = self.cancel_flag.clone();
                thread::spawn(move || {
                    // Contents are read here, off the UI thread, at send time.
                    let (full, notices) = attach::openai_block(&atts, &prompt);
                    for n in notices {
                        let _ = tx.send(Event::Tool(format!("attachment: {n}")));
                    }
                    run_openai(tx, full, cwd, history, cfg, cancel_flag)
                });
            }
        }
    }

    /// Drain events from the worker thread. Call once per frame.
    fn poll(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.rx else { return };
        let mut done = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Event::Session(s) => self.session_id = Some(s),
                Event::Hook(h) => self.hook_events.push(h),
                Event::Text(t) => match self.messages.last_mut() {
                    Some(m) if m.role == Role::Assistant => m.text.push_str(&t),
                    _ => self.messages.push(ChatMsg { role: Role::Assistant, text: t }),
                },
                Event::Tool(t) => {
                    self.messages.push(ChatMsg { role: Role::Tool, text: t });
                    self.messages.push(ChatMsg { role: Role::Assistant, text: String::new() });
                }
                Event::Error(e) => self.messages.push(ChatMsg { role: Role::Error, text: e }),
                Event::Usage(u) => self.last_usage = Some(u),
                Event::Done => done = true,
            }
        }
        if self.messages.len() > MAX_MESSAGES {
            let cut = self.messages.len() - MAX_MESSAGES;
            self.messages.drain(..cut);
        }
        if done {
            self.busy = false;
            self.rx = None;
            self.turn_cwd = None;
            self.active_backend = None;
            self.messages.retain(|m| !(m.role == Role::Assistant && m.text.is_empty()));
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    /// Render as a right side panel. `cwd` is where the next turn runs.
    pub fn show(&mut self, ctx: &egui::Context, cwd: &PathBuf, cfg: &Config) {
        self.poll(ctx);
        // Keep the per-directory transcript store current even while the
        // panel is hidden, so reopening it later shows the right history.
        // Guarded on `!busy`: an in-flight turn's directory must stay loaded
        // (see `turn_cwd`), not be swapped out because the explorer moved on.
        if !self.busy {
            self.sync_dir(cwd);
        }
        if !self.open {
            return;
        }
        let title = match cfg.backend {
            Backend::ClaudeCode => "Claude Code".to_string(),
            Backend::OpenAiCompatible => cfg.model.clone(),
        };
        egui::SidePanel::right("chat")
            .resizable(true)
            .default_width(380.0)
            .min_width(240.0)
            .show(ctx, |ui| {
                let panel_rect = ui.max_rect();
                ui.horizontal(|ui| {
                    ui.strong(title);
                    if self.busy {
                        ui.spinner();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(crate::icons::CLOSE).on_hover_text("Hide chat").clicked() {
                            self.open = false;
                        }
                        if ui.add_enabled(!self.busy, egui::Button::new("New")).clicked() {
                            self.new_session();
                        }
                    });
                });
                ui.horizontal(|ui| {
                    // A running turn keeps working in the directory it was
                    // sent from; showing the explorer's live cwd instead
                    // would misreport where the work is happening if the
                    // user navigated away mid-turn.
                    let shown_cwd = if self.busy { self.turn_cwd.as_ref().unwrap_or(cwd) } else { cwd };
                    ui.small(format!("cwd: {}", shown_cwd.display()));
                    if let Some(usage) = self.last_usage.as_ref().and_then(TurnUsage::format) {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.small(egui::RichText::new(usage).color(ui.visuals().weak_text_color()));
                        });
                    }
                });
                if cfg.backend == Backend::ClaudeCode && !self.agents.is_empty() {
                    ui.horizontal(|ui| {
                        ui.small("Agent");
                        let current = self.agent.clone().unwrap_or_else(|| "(default)".into());
                        let mut picked: Option<Option<String>> = None;
                        egui::ComboBox::from_id_salt("agent-pick")
                            .selected_text(current)
                            .width(220.0)
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(self.agent.is_none(), "(default)").clicked() {
                                    picked = Some(None);
                                }
                                for (name, badge, desc) in &self.agents {
                                    let r = ui.selectable_label(self.agent.as_deref() == Some(name), format!("{badge} {name}"));
                                    if !desc.is_empty() {
                                        r.clone().on_hover_text(desc);
                                    }
                                    if r.clicked() {
                                        picked = Some(Some(name.clone()));
                                    }
                                }
                            });
                        if let Some(p) = picked {
                            if p != self.agent {
                                // --agent is per session; make the reset visible rather than
                                // silently applying it to a resumed one.
                                self.agent = p.clone();
                                self.session_id = None;
                                self.messages.push(ChatMsg {
                                    role: Role::Tool,
                                    text: format!("new session: agent {}", p.unwrap_or_else(|| "(default)".into())),
                                });
                            }
                        }
                    });
                }
                ui.separator();

                let input_h = 90.0;
                // Only the very last user message gets a retry button, and
                // only while idle: retrying mid-turn would race the turn
                // already in flight.
                let last_user_idx = if self.busy { None } else { self.messages.iter().rposition(|m| m.role == Role::User) };
                let can_retry = last_user_idx.is_some() && self.last_turn.is_some();
                let mut retry_clicked = false;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .max_height(ui.available_height() - input_h)
                    .show(ui, |ui| {
                        for (i, m) in self.messages.iter().enumerate() {
                            let (label, color) = match m.role {
                                Role::User => ("You", egui::Color32::LIGHT_BLUE),
                                Role::Assistant => ("Assistant", egui::Color32::LIGHT_GREEN),
                                Role::Tool => ("tool", egui::Color32::GRAY),
                                Role::Error => ("error", egui::Color32::LIGHT_RED),
                            };
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(label).small().color(color));
                                // Kept off tool lines on purpose: a button on every
                                // tool-use line would be noise, not a feature.
                                if m.role == Role::Assistant && !m.text.is_empty() {
                                    if ui.small_button(ph::COPY).on_hover_text("Copy").clicked() {
                                        ui.ctx().copy_text(m.text.clone());
                                    }
                                }
                                if can_retry && Some(i) == last_user_idx {
                                    if ui.small_button(ph::ARROW_CLOCKWISE).on_hover_text("Retry").clicked() {
                                        retry_clicked = true;
                                    }
                                }
                            });
                            if m.role == Role::Tool {
                                ui.add(egui::Label::new(egui::RichText::new(&m.text).small().monospace()).wrap());
                            } else {
                                ui.add(egui::Label::new(&m.text).wrap());
                            }
                            ui.add_space(6.0);
                        }
                    });
                if retry_clicked {
                    self.retry_last(cwd.clone(), cfg);
                }

                ui.separator();
                self.chips(ui, cfg);
                let resp = ui.add(
                    egui::TextEdit::multiline(&mut self.input)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .hint_text("Ask about this folder. Enter sends, Shift+Enter newline."),
                );
                // Backspace in an empty input removes the last chip; Escape clears them.
                if resp.has_focus() && self.input.is_empty() && !self.attachments.is_empty() {
                    if ui.input(|i| i.key_pressed(egui::Key::Backspace)) {
                        self.attachments.pop();
                    }
                    if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                        self.attachments.clear();
                    }
                }
                // `/` skill picker: only when the input starts with `/` (or a line does)
                // and has no space yet, so `src/foo.rs` mid-sentence never opens it.
                let slash_filter: Option<String> = {
                    let last_line = self.input.rsplit('\n').next().unwrap_or("");
                    last_line.strip_prefix('/').filter(|rest| !rest.contains(' ')).map(|r| r.to_lowercase())
                };
                let mut picker_took_enter = false;
                if let (Some(filter), false) = (&slash_filter, self.skills.is_empty()) {
                    let matches: Vec<(String, String)> = self
                        .skills
                        .iter()
                        .filter(|(n, _)| n.to_lowercase().contains(filter))
                        .take(10)
                        .cloned()
                        .collect();
                    if !matches.is_empty() && resp.has_focus() {
                        self.skill_pick = self.skill_pick.min(matches.len() - 1);
                        let mut chosen: Option<String> = None;
                        ui.input_mut(|i| {
                            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
                                self.skill_pick = (self.skill_pick + 1).min(matches.len() - 1);
                            }
                            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
                                self.skill_pick = self.skill_pick.saturating_sub(1);
                            }
                            if i.consume_key(egui::Modifiers::NONE, egui::Key::Enter) || i.consume_key(egui::Modifiers::NONE, egui::Key::Tab) {
                                chosen = Some(matches[self.skill_pick].0.clone());
                                picker_took_enter = true;
                            }
                        });
                        let anchor = resp.rect.left_top();
                        egui::Area::new(ui.id().with("skill-pick"))
                            .order(egui::Order::Foreground)
                            .fixed_pos(anchor - egui::vec2(0.0, 8.0 + 22.0 * matches.len() as f32))
                            .show(ctx, |ui| {
                                egui::Frame::popup(ui.style()).show(ui, |ui| {
                                    ui.set_min_width(resp.rect.width());
                                    for (i, (n, d)) in matches.iter().enumerate() {
                                        let text = if d.is_empty() { format!("/{n}") } else { format!("/{n}    {}", d.chars().take(60).collect::<String>()) };
                                        if ui.selectable_label(i == self.skill_pick, text).clicked() {
                                            chosen = Some(n.clone());
                                        }
                                    }
                                });
                            });
                        if let Some(name) = chosen {
                            // Replace the `/filter` on the last line with `/name `.
                            let cut = self.input.rfind('/').unwrap_or(0);
                            self.input.truncate(cut);
                            self.input.push_str(&format!("/{name} "));
                            // Move the caret to the end.
                            let id = resp.id;
                            let mut st = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
                            let end = self.input.chars().count();
                            st.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(end))));
                            egui::TextEdit::store_state(ctx, id, st);
                        }
                    }
                }
                let enter = !picker_took_enter
                    && resp.has_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
                // Send doubles as Stop while a turn is running, same button
                // so there's nothing new to learn: label and action both flip.
                let clicked = ui.button(if self.busy { "Stop" } else { "Send" }).clicked();
                if self.busy {
                    if clicked {
                        self.stop();
                    }
                } else if (enter || clicked) && !self.input.trim().is_empty() {
                    let prompt = self.input.trim_end().to_string();
                    self.input.clear();
                    self.send(prompt, cwd.clone(), cfg);
                }

                // Drop zone: a hover-only interact over the whole panel, registered last.
                // `Ui::dnd_drop_zone` was rejected because it repaints the frame fill every
                // frame; hover sense steals no clicks from the TextEdit or buttons, and the
                // dnd_* payload calls gate on contains_pointer, so they work under widgets.
                let zone = ui.interact(panel_rect, ui.id().with("dropzone"), egui::Sense::hover());
                if zone.dnd_hover_payload::<DragPaths>().is_some() {
                    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("chat_drop")));
                    let stroke = ui.visuals().selection.stroke;
                    painter.rect_stroke(panel_rect.shrink(2.0), 4.0, egui::Stroke::new(2.0_f32, stroke.color));
                    painter.text(
                        panel_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "Drop to attach",
                        egui::FontId::proportional(18.0),
                        stroke.color,
                    );
                }
                if let Some(p) = zone.dnd_release_payload::<DragPaths>() {
                    for path in &p.0 {
                        self.attach(path);
                    }
                }
            });
    }

    /// Attachment chips above the input: icon, name, size, remove button.
    fn chips(&mut self, ui: &mut egui::Ui, cfg: &Config) {
        if self.attachments.is_empty() && self.attach_note.is_empty() {
            return;
        }
        let mut remove: Option<usize> = None;
        let openai = cfg.backend == Backend::OpenAiCompatible;
        ui.horizontal_wrapped(|ui| {
            for (i, a) in self.attachments.iter().enumerate() {
                let (glyph, color) = match a.kind {
                    attach::Kind::Folder => crate::icons::folder(false),
                    _ => crate::icons::file(&a.name()),
                };
                let cut = openai && a.will_be_cut();
                let text_color = if cut { ui.visuals().warn_fg_color } else { ui.visuals().text_color() };
                let label = match a.kind {
                    attach::Kind::Folder => a.name(),
                    attach::Kind::Binary => format!("{} (binary)", a.name()),
                    attach::Kind::Text => format!("{} ({})", a.name(), attach::human(a.size)),
                };
                egui::Frame::group(ui.style()).inner_margin(egui::vec2(6.0, 2.0)).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.label(egui::RichText::new(glyph).color(crate::icons::tint(color, ui.visuals().dark_mode)));
                    let r = ui.label(egui::RichText::new(label).color(text_color).small());
                    let tip = if cut {
                        format!("{}\nwill be cut or skipped on this backend", a.path.display())
                    } else {
                        a.path.display().to_string()
                    };
                    r.on_hover_text(tip);
                    if ui.small_button(crate::icons::CLOSE).clicked() {
                        remove = Some(i);
                    }
                });
            }
            if self.attachments.len() > 1 && ui.small_button("Clear").clicked() {
                self.attachments.clear();
            }
        });
        if let Some(i) = remove {
            self.attachments.remove(i);
        }
        if !self.attach_note.is_empty() {
            ui.small(&self.attach_note);
        }
    }
}

// ---------- backends ----------

fn run_claude(
    tx: Sender<Event>,
    prompt: String,
    cwd: PathBuf,
    resume: Option<String>,
    mode: String,
    agent: Option<String>,
    add_dirs: Vec<PathBuf>,
    pid_slot: Arc<Mutex<Option<u32>>>,
) {
    let mut cmd = Command::new("claude");
    cmd.current_dir(&cwd)
        .arg("-p")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose")
        .arg("--include-partial-messages")
        .arg("--permission-mode")
        .arg(mode);
    if let Some(sid) = resume {
        cmd.arg("--resume").arg(sid);
    }
    if let Some(a) = agent {
        cmd.arg("--agent").arg(a);
    }
    if !add_dirs.is_empty() {
        cmd.arg("--add-dir");
        for d in &add_dirs {
            cmd.arg(d);
        }
    }
    cmd.arg(prompt)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // New process group (pgid == pid) so Stop can signal the whole tree
        // — `claude` plus whatever tools it shells out to — with one
        // negative pid instead of hunting down descendants individually.
        cmd.process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(Event::Error(format!(
                "Could not start `claude`: {e}. Check the CLI is installed and on PATH, then try Send again."
            )));
            let _ = tx.send(Event::Done);
            return;
        }
    };
    if let Ok(mut g) = pid_slot.lock() {
        *g = Some(child.id());
    }
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let tx_err = tx.clone();
    let err_thread = thread::spawn(move || {
        let mut buf = String::new();
        for line in BufReader::new(stderr).lines().flatten() {
            buf.push_str(&line);
            buf.push('\n');
        }
        if !buf.trim().is_empty() {
            let _ = tx_err.send(Event::Error(buf));
        }
    });

    // Set once a `result` message is actually seen, so a stream that ends
    // without one (crash, killed process, protocol change) is reported
    // distinctly instead of silently looking like a normal, quiet turn.
    let mut saw_result = false;
    for line in BufReader::new(stdout).lines().flatten() {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            // A message worse than the truth, so seeing it is a bug report.
            let head: String = line.chars().take(200).collect();
            let _ = tx.send(Event::Tool(format!("unparsed stream line ({} bytes): {head}", line.len())));
            continue;
        };
        match v["type"].as_str().unwrap_or("") {
            "system" if v["subtype"] == "init" => {
                if let Some(s) = v["session_id"].as_str() {
                    let _ = tx.send(Event::Session(s.to_string()));
                }
            }
            "system" if v["subtype"] == "hook_started" || v["subtype"] == "hook_response" => {
                let _ = tx.send(Event::Hook(HookEvent {
                    name: v["hook_name"].as_str().unwrap_or("").to_string(),
                    outcome: if v["subtype"] == "hook_started" { "started".into() } else { v["outcome"].as_str().unwrap_or("").to_string() },
                    exit_code: v["exit_code"].as_i64(),
                    stdout: v["stdout"].as_str().unwrap_or("").to_string(),
                    stderr: v["stderr"].as_str().unwrap_or("").to_string(),
                }));
            }
            "rate_limit_event" => {
                if let Some(u) = v["rate_limit_info"]["unifiedWindows"]["five_hour"]["utilization"].as_f64() {
                    if u > 0.9 {
                        let _ = tx.send(Event::Tool(format!("rate limit: five-hour window at {:.0}%", u * 100.0)));
                    }
                }
            }
            "stream_event" => {
                let ev = &v["event"];
                if ev["type"] == "content_block_delta" && ev["delta"]["type"] == "text_delta" {
                    if let Some(t) = ev["delta"]["text"].as_str() {
                        let _ = tx.send(Event::Text(t.to_string()));
                    }
                }
            }
            "assistant" => {
                if let Some(blocks) = v["message"]["content"].as_array() {
                    for b in blocks.iter().filter(|b| b["type"] == "tool_use") {
                        let name = b["name"].as_str().unwrap_or("tool");
                        let full = b["input"].to_string();
                        let n = full.chars().count();
                        let mut short: String = full.chars().take(160).collect();
                        if n > 160 {
                            short.push_str(&format!(" … (+{} chars)", n - 160));
                        }
                        let _ = tx.send(Event::Tool(format!("{name} {short}")));
                    }
                }
            }
            "result" => {
                saw_result = true;
                let cost = v["total_cost_usd"].as_f64();
                let input_tokens = v["usage"]["input_tokens"].as_u64();
                let output_tokens = v["usage"]["output_tokens"].as_u64();
                if cost.is_some() || input_tokens.is_some() || output_tokens.is_some() {
                    let _ = tx.send(Event::Usage(TurnUsage { input_tokens, output_tokens, cost_usd: cost }));
                }
                if v["is_error"] == true {
                    let msg = v["result"].as_str().unwrap_or("turn failed").to_string();
                    let _ = tx.send(Event::Error(msg));
                }
            }
            _ => {}
        }
    }
    if !saw_result {
        let _ = tx.send(Event::Error(
            "Claude Code exited without finishing the turn (no result message) — it may have crashed or been killed. Check the output above, then try Send again.".to_string(),
        ));
    }
    let _ = err_thread.join();
    let _ = child.wait();
    if let Ok(mut g) = pid_slot.lock() {
        *g = None;
    }
    let _ = tx.send(Event::Done);
}

/// Kill a Claude Code child and every descendant, then confirm it is
/// actually gone rather than trusting the kill call's own exit code —
/// `taskkill`/`kill` both report success even when the target already
/// exited or never existed.
fn kill_process_tree(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        // A dead pid makes a second, filtered taskkill print nothing for it
        // (a live one still shows up in the list); that absence is the
        // confirmation, not the first call's own exit code.
        match Command::new("tasklist").args(["/FI", &format!("PID eq {pid}")]).output() {
            Ok(out) => !String::from_utf8_lossy(&out.stdout).contains(&pid.to_string()),
            Err(_) => false,
        }
    }
    #[cfg(not(windows))]
    {
        // `process_group(0)` at spawn made `pid` double as the group id, so
        // a negative target *should* reach every descendant with one signal.
        // It does not: verified on macOS 26.2 with a real `claude` turn that
        // a Bash tool call spawns its shell in a *different*, new process
        // group (Node's `child_process.spawn(..., { detached: true })`),
        // still parented under `claude` but invisible to a group-wide
        // signal. `kill -TERM -<pid>` / `kill -KILL -<pid>` killed `claude`
        // itself while the Bash-tool subprocess (and everything under it)
        // kept running, reparented to launchd, and the old `kill -0 -<pid>`
        // check still reported "confirmed dead" because a zombie and a gone
        // group both make `kill -0` fail the same way, so it cannot tell
        // them apart from here. Fix: walk the real PPID tree via `pgrep -P`
        // (catches a detached descendant the group signal misses) in
        // addition to the group, and confirm every collected pid by itself.
        let mut pids = vec![pid];
        let mut frontier = vec![pid];
        while let Some(p) = frontier.pop() {
            let Ok(out) = Command::new("pgrep").arg("-P").arg(p.to_string()).output() else { continue };
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Ok(child) = line.trim().parse::<u32>() {
                    if !pids.contains(&child) {
                        pids.push(child);
                        frontier.push(child);
                    }
                }
            }
        }
        let signal = |sig: &str, pids: &[u32]| {
            let _ = Command::new("kill").arg(sig).arg(format!("-{pid}")).status();
            for p in pids {
                let _ = Command::new("kill").arg(sig).arg(p.to_string()).status();
            }
        };
        signal("-TERM", &pids);
        thread::sleep(std::time::Duration::from_millis(120));
        signal("-KILL", &pids);
        thread::sleep(std::time::Duration::from_millis(80));
        // Confirmation is not `kill -0` on each plain pid: a zombie's pid
        // slot still exists, so a *plain* (non-group) `kill -0` on it
        // succeeds — measured directly against `claude`'s own pid here,
        // which reported alive by that check moments after being reaped
        // with SIGKILL. `kill -0` on the *group* form (`-pid`) does fail for
        // a lone zombie on macOS (EPERM), which is what the previous
        // single-pid version of this check relied on — but that shortcut
        // does not extend to the individually-collected descendant pids
        // above, which are plain pids, not groups. Read `ps` state instead:
        // empty (gone) or `Z` (zombie, terminated but unreaped by its
        // parent — inert either way) both count as dead.
        pids.iter().all(|p| {
            match Command::new("ps").arg("-o").arg("stat=").arg("-p").arg(p.to_string()).output() {
                Ok(out) if out.status.success() => {
                    let stat = String::from_utf8_lossy(&out.stdout);
                    let stat = stat.trim();
                    stat.is_empty() || stat.starts_with('Z')
                }
                _ => true,
            }
        })
    }
}

fn run_openai(tx: Sender<Event>, prompt: String, cwd: PathBuf, mut history: Vec<Value>, cfg: Config, cancel_flag: Arc<AtomicBool>) {
    // Give the model the directory context Claude Code would get from tools.
    let listing: Vec<String> = std::fs::read_dir(&cwd)
        .map(|rd| rd.flatten().take(200).map(|d| d.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    let system = format!(
        "You are a file-explorer assistant. Current directory: {}\nEntries:\n{}",
        cwd.display(),
        listing.join("\n")
    );
    let mut messages = vec![json!({"role": "system", "content": system})];
    messages.append(&mut history);
    messages.push(json!({"role": "user", "content": prompt}));

    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let body = json!({"model": cfg.model, "messages": messages, "stream": true});
    let mut req = ureq::post(&url).set("Content-Type", "application/json");
    if !cfg.api_key.is_empty() {
        req = req.set("Authorization", &format!("Bearer {}", cfg.api_key));
    }
    let resp = match req.send_string(&body.to_string()) {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            let _ = tx.send(Event::Error(format!(
                "Server rejected the request (HTTP {code}). Check the endpoint URL, model name and API key in Settings.\n{text}"
            )));
            let _ = tx.send(Event::Done);
            return;
        }
        Err(e) => {
            let _ = tx.send(Event::Error(format!(
                "Could not reach {url}: {e}. Check the endpoint is running and the base URL in Settings."
            )));
            let _ = tx.send(Event::Done);
            return;
        }
    };

    // No `saw_done` bookkeeping here: unlike Claude Code's explicit `result`
    // message, an OpenAI-compatible stream ending after `[DONE]` and one
    // ending because the connection just dropped both look like "the loop
    // stopped reading lines" from here, so there is nothing more specific to
    // report than what's already surfaced via `Event::Error` above or from
    // an `error` field on the wire.
    for line in BufReader::new(resp.into_reader()).lines().flatten() {
        // Checked between lines, not just once: an HTTP response body can't
        // be killed like a child process, so this is the only way Stop can
        // make the read loop actually stop reading.
        if cancel_flag.load(Ordering::SeqCst) {
            break;
        }
        let Some(data) = line.strip_prefix("data: ") else { continue };
        if data.trim() == "[DONE]" {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
        if let Some(t) = v["choices"][0]["delta"]["content"].as_str() {
            let _ = tx.send(Event::Text(t.to_string()));
        }
        if let Some(e) = v["error"]["message"].as_str() {
            let _ = tx.send(Event::Error(e.to_string()));
        }
        // Only some OpenAI-compatible servers send a trailing `usage`
        // object (e.g. with `stream_options.include_usage`); when it's
        // absent we show nothing rather than guess at token counts.
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            let input_tokens = u["prompt_tokens"].as_u64();
            let output_tokens = u["completion_tokens"].as_u64();
            if input_tokens.is_some() || output_tokens.is_some() {
                let _ = tx.send(Event::Usage(TurnUsage { input_tokens, output_tokens, cost_usd: None }));
            }
        }
    }
    let _ = tx.send(Event::Done);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_tokens_uses_k_suffix_above_1000() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1000), "1.0k");
        assert_eq!(format_tokens(12_345), "12.3k");
    }

    #[test]
    fn format_cost_uses_more_precision_under_a_cent() {
        assert_eq!(format_cost(0.0031), "$0.0031");
        assert_eq!(format_cost(0.0099), "$0.0099");
        assert_eq!(format_cost(0.01), "$0.01");
        assert_eq!(format_cost(1.5), "$1.50");
    }

    #[test]
    fn turn_usage_format_omits_unknown_fields_entirely() {
        assert_eq!(TurnUsage::default().format(), None);
        let tokens_only = TurnUsage { input_tokens: Some(100), output_tokens: Some(50), cost_usd: None };
        assert_eq!(tokens_only.format().as_deref(), Some("100 in / 50 out"));
        let cost_only = TurnUsage { input_tokens: None, output_tokens: None, cost_usd: Some(0.02) };
        assert_eq!(cost_only.format().as_deref(), Some("$0.02"));
        let partial_tokens = TurnUsage { input_tokens: Some(2000), output_tokens: None, cost_usd: Some(0.1234) };
        assert_eq!(partial_tokens.format().as_deref(), Some("2.0k in / ? out \u{b7} $0.12"));
    }

    #[test]
    fn dir_history_swap_roundtrips_messages_and_session_per_directory() {
        let mut h = DirHistory::default();
        let dir_a = PathBuf::from("/a");
        let dir_b = PathBuf::from("/b");

        // First visit to B: nothing stored yet, so it comes back empty.
        let entry = h.swap(None, DirEntry::default(), &dir_b);
        assert!(entry.messages.is_empty());
        assert!(entry.session_id.is_none());

        // Leaving B (still empty) for A, also never visited: still empty.
        let entry = h.swap(Some(&dir_b), DirEntry::default(), &dir_a);
        assert!(entry.messages.is_empty());

        // Build up a real conversation in A, then wander off to B and back.
        let a_messages = vec![ChatMsg { role: Role::User, text: "hi".into() }];
        let a_entry = DirEntry { messages: a_messages, session_id: Some("sess-a".into()), last_turn: Some(("hi".into(), Vec::new())) };
        let _ = h.swap(Some(&dir_a), a_entry, &dir_b);
        let restored = h.swap(Some(&dir_b), DirEntry::default(), &dir_a);
        assert_eq!(restored.messages.len(), 1);
        assert_eq!(restored.messages[0].text, "hi");
        assert_eq!(restored.session_id.as_deref(), Some("sess-a"));
        assert!(restored.last_turn.is_some());
    }

    /// Regression for the macOS bug where `kill_process_tree` reported a
    /// turn dead while a Bash-tool subprocess kept running: Claude Code
    /// (like many Node CLIs) spawns tool subprocesses with
    /// `child_process.spawn(..., { detached: true })`, landing them in a
    /// *new* process group invisible to a plain `-<pid>` group signal, even
    /// though they are still real descendants. `set -m` (job control) makes
    /// a plain `sh -c` script background a job into its own new group the
    /// same way, so this reproduces the failure mode without needing a real
    /// `claude` invocation, an API key, or network access.
    #[cfg(not(windows))]
    #[test]
    fn kill_process_tree_reaches_a_detached_descendant() {
        use std::os::unix::process::CommandExt;

        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg("set -m; sleep 300 & echo GRANDCHILD_PID=$!; wait")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("spawn should succeed");
        let pid = child.id();

        let stdout = child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let grandchild: u32 = line
            .trim()
            .strip_prefix("GRANDCHILD_PID=")
            .expect("child should report its own grandchild's pid")
            .parse()
            .unwrap();

        // Sanity check the repro actually landed the grandchild in a
        // different group than `pid` -- otherwise this test would pass
        // even without the fix, for the wrong reason.
        let pgid_of = |p: u32| {
            Command::new("ps").arg("-o").arg("pgid=").arg("-p").arg(p.to_string()).output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default()
        };
        assert_ne!(
            pgid_of(pid),
            pgid_of(grandchild),
            "test setup should put the grandchild in its own process group"
        );

        assert!(kill_process_tree(pid), "kill_process_tree should confirm the tree is dead");

        let grandchild_state = Command::new("ps")
            .arg("-o")
            .arg("stat=")
            .arg("-p")
            .arg(grandchild.to_string())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        assert!(
            grandchild_state.is_empty() || grandchild_state.starts_with('Z'),
            "detached grandchild pid {grandchild} should be dead too, ps stat was {grandchild_state:?}"
        );

        let _ = child.wait();
    }
}
