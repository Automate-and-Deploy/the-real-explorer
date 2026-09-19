//! Collapsible Claude Code chat panel. Drives the `claude` CLI in print mode
//! with `--output-format stream-json`, the same wire protocol the Agent SDK
//! wraps, so no SDK runtime is needed. Each send spawns one process pinned to
//! the explorer's current directory; `--resume` keeps the session across turns.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::thread;

use eframe::egui;

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
    input: String,
    messages: Vec<ChatMsg>,
    session_id: Option<String>,
    busy: bool,
    rx: Option<Receiver<Event>>,
    /// Directory the running turn was pinned to; shown in the header.
    turn_cwd: Option<PathBuf>,
}

impl ChatPanel {
    pub fn new() -> Self {
        Self {
            open: true,
            input: String::new(),
            messages: Vec::new(),
            session_id: None,
            busy: false,
            rx: None,
            turn_cwd: None,
        }
    }

    pub fn new_session(&mut self) {
        self.session_id = None;
        self.messages.clear();
    }

    /// Spawn one `claude -p` turn in `cwd`; output streams back through `rx`.
    fn send(&mut self, prompt: String, cwd: PathBuf) {
        self.messages.push(ChatMsg { role: Role::User, text: prompt.clone() });
        self.messages.push(ChatMsg { role: Role::Assistant, text: String::new() });
        self.busy = true;
        self.turn_cwd = Some(cwd.clone());
        let (tx, rx) = channel();
        self.rx = Some(rx);
        let resume = self.session_id.clone();

        thread::spawn(move || {
            let mut cmd = Command::new("claude");
            cmd.current_dir(&cwd)
                .arg("-p")
                .arg("--output-format")
                .arg("stream-json")
                .arg("--verbose")
                .arg("--include-partial-messages")
                .arg("--permission-mode")
                .arg("acceptEdits");
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
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
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
                                let input = b["input"].to_string();
                                let short: String = input.chars().take(160).collect();
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
        });
    }

    /// Drain events from the worker thread. Call once per frame.
    fn poll(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.rx else { return };
        let mut done = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Event::Session(s) => self.session_id = Some(s),
                Event::Text(t) => {
                    match self.messages.last_mut() {
                        Some(m) if m.role == Role::Assistant => m.text.push_str(&t),
                        _ => self.messages.push(ChatMsg { role: Role::Assistant, text: t }),
                    }
                }
                Event::Tool(t) => {
                    self.messages.push(ChatMsg { role: Role::Tool, text: t });
                    // Subsequent text belongs to a fresh assistant bubble.
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
    pub fn show(&mut self, ctx: &egui::Context, cwd: &PathBuf) {
        self.poll(ctx);
        if !self.open {
            return;
        }
        egui::SidePanel::right("chat")
            .resizable(true)
            .default_width(380.0)
            .min_width(240.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("Claude Code");
                    if self.busy {
                        ui.spinner();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("✖").on_hover_text("Hide chat").clicked() {
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
                                Role::Assistant => ("Claude", egui::Color32::LIGHT_GREEN),
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
                let resp = ui.add(
                    egui::TextEdit::multiline(&mut self.input)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .hint_text("Ask about this folder. Enter sends, Shift+Enter newline."),
                );
                let enter = resp.has_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
                let clicked = ui.add_enabled(!self.busy, egui::Button::new("Send")).clicked();
                if (enter || clicked) && !self.busy && !self.input.trim().is_empty() {
                    let prompt = self.input.trim_end().to_string();
                    self.input.clear();
                    self.send(prompt, cwd.clone());
                }
            });
    }
}
