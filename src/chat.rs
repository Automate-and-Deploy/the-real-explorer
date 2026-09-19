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

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

use eframe::egui;
use serde_json::{json, Value};

use crate::attach::{self, Attachment, DragPaths};
use crate::config::{Backend, Config};

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
    Text(String),
    Tool(String),
    Error(String),
    Done,
}

pub struct ChatPanel {
    pub open: bool,
    /// Dropped paths waiting for the next send. Cleared on send.
    attachments: Vec<Attachment>,
    /// Last refusal or dedupe note, shown under the chips.
    attach_note: String,
    input: String,
    messages: Vec<ChatMsg>,
    session_id: Option<String>,
    busy: bool,
    rx: Option<Receiver<Event>>,
}

impl ChatPanel {
    pub fn new(open: bool) -> Self {
        Self {
            open,
            attachments: Vec::new(),
            attach_note: String::new(),
            input: String::new(),
            messages: Vec::new(),
            session_id: None,
            busy: false,
            rx: None,
        }
    }

    pub fn new_session(&mut self) {
        self.session_id = None;
        self.messages.clear();
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

        let atts = std::mem::take(&mut self.attachments);
        self.attach_note.clear();
        let shown = if atts.is_empty() {
            prompt.clone()
        } else {
            let names: Vec<String> = atts.iter().map(|a| a.name()).collect();
            format!("{prompt}\n\nattached: {}", names.join(", "))
        };
        self.messages.push(ChatMsg { role: Role::User, text: shown });
        self.messages.push(ChatMsg { role: Role::Assistant, text: String::new() });
        self.busy = true;
        let (tx, rx) = channel();
        self.rx = Some(rx);

        match cfg.backend {
            Backend::ClaudeCode => {
                let resume = self.session_id.clone();
                let mode = cfg.claude_permission_mode.clone();
                let full = attach::claude_block(&atts, &prompt);
                thread::spawn(move || run_claude(tx, full, cwd, resume, mode));
            }
            Backend::OpenAiCompatible => {
                let cfg = cfg.clone();
                thread::spawn(move || {
                    // Contents are read here, off the UI thread, at send time.
                    let (full, notices) = attach::openai_block(&atts, &prompt);
                    for n in notices {
                        let _ = tx.send(Event::Tool(format!("attachment: {n}")));
                    }
                    run_openai(tx, full, cwd, history, cfg)
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
                Event::Text(t) => match self.messages.last_mut() {
                    Some(m) if m.role == Role::Assistant => m.text.push_str(&t),
                    _ => self.messages.push(ChatMsg { role: Role::Assistant, text: t }),
                },
                Event::Tool(t) => {
                    self.messages.push(ChatMsg { role: Role::Tool, text: t });
                    self.messages.push(ChatMsg { role: Role::Assistant, text: String::new() });
                }
                Event::Error(e) => self.messages.push(ChatMsg { role: Role::Error, text: e }),
                Event::Done => done = true,
            }
        }
        if done {
            self.busy = false;
            self.rx = None;
            self.messages.retain(|m| !(m.role == Role::Assistant && m.text.is_empty()));
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    /// Render as a right side panel. `cwd` is where the next turn runs.
    pub fn show(&mut self, ctx: &egui::Context, cwd: &PathBuf, cfg: &Config) {
        self.poll(ctx);
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
                ui.small(format!("cwd: {}", cwd.display()));
                ui.separator();

                let input_h = 90.0;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .max_height(ui.available_height() - input_h)
                    .show(ui, |ui| {
                        for m in &self.messages {
                            let (label, color) = match m.role {
                                Role::User => ("You", egui::Color32::LIGHT_BLUE),
                                Role::Assistant => ("Assistant", egui::Color32::LIGHT_GREEN),
                                Role::Tool => ("tool", egui::Color32::GRAY),
                                Role::Error => ("error", egui::Color32::LIGHT_RED),
                            };
                            ui.label(egui::RichText::new(label).small().color(color));
                            if m.role == Role::Tool {
                                ui.add(egui::Label::new(egui::RichText::new(&m.text).small().monospace()).wrap());
                            } else {
                                ui.add(egui::Label::new(&m.text).wrap());
                            }
                            ui.add_space(6.0);
                        }
                    });

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
                let enter = resp.has_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
                let clicked = ui.add_enabled(!self.busy, egui::Button::new("Send")).clicked();
                if (enter || clicked) && !self.busy && !self.input.trim().is_empty() {
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
                    painter.rect_stroke(panel_rect.shrink(2.0), 4.0, egui::Stroke::new(2.0, stroke.color));
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

fn run_claude(tx: Sender<Event>, prompt: String, cwd: PathBuf, resume: Option<String>, mode: String) {
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

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(Event::Error(format!("cannot start claude: {e}")));
            let _ = tx.send(Event::Done);
            return;
        }
    };
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

    for line in BufReader::new(stdout).lines().flatten() {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        match v["type"].as_str().unwrap_or("") {
            "system" if v["subtype"] == "init" => {
                if let Some(s) = v["session_id"].as_str() {
                    let _ = tx.send(Event::Session(s.to_string()));
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
                        let short: String = b["input"].to_string().chars().take(160).collect();
                        let _ = tx.send(Event::Tool(format!("{name} {short}")));
                    }
                }
            }
            "result" => {
                if v["is_error"] == true {
                    let msg = v["result"].as_str().unwrap_or("turn failed").to_string();
                    let _ = tx.send(Event::Error(msg));
                }
            }
            _ => {}
        }
    }
    let _ = err_thread.join();
    let _ = child.wait();
    let _ = tx.send(Event::Done);
}

fn run_openai(tx: Sender<Event>, prompt: String, cwd: PathBuf, mut history: Vec<Value>, cfg: Config) {
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
            let _ = tx.send(Event::Error(format!("HTTP {code}: {text}")));
            let _ = tx.send(Event::Done);
            return;
        }
        Err(e) => {
            let _ = tx.send(Event::Error(format!("request failed: {e}")));
            let _ = tx.send(Event::Done);
            return;
        }
    };

    for line in BufReader::new(resp.into_reader()).lines().flatten() {
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
    }
    let _ = tx.send(Event::Done);
}
