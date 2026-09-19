//! Hooks editor view over `hooks.rs`. Picks a scope (project, project-local,
//! user), shows the absolute path it will write, edits groups and commands
//! per event, and can dry-run a command with sample JSON on stdin. Saving
//! goes through `hooks::save`, which preserves every other key in the file,
//! refuses if the file changed underneath, and writes atomically.

use std::path::Path;
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

use eframe::egui;

use crate::harness_ui::Action;
use crate::hooks::{self, Command, Group, Hooks, Loaded, Scope, TestResult, KNOWN_EVENTS};

pub struct HooksEditor {
    scope: Scope,
    /// Scope the loaded document came from, to put the picker back when a
    /// switch is refused because of unsaved edits.
    loaded_scope: Scope,
    loaded: Option<Loaded>,
    edit: Hooks,
    load_error: Option<String>,
    status: String,
    dirty: bool,
    new_event: String,
    /// Dry run: (event, command, editable stdin) and the result when done.
    test: Option<TestState>,
}

struct TestState {
    command: String,
    stdin: String,
    argv: String,
    rx: Option<Receiver<TestResult>>,
    result: Option<TestResult>,
}

impl HooksEditor {
    pub fn new() -> Self {
        Self {
            scope: Scope::User,
            loaded_scope: Scope::User,
            loaded: None,
            edit: Hooks::default(),
            load_error: None,
            status: String::new(),
            dirty: false,
            new_event: String::new(),
            test: None,
        }
    }

    fn reload(&mut self, project: &Path) {
        self.load_error = None;
        match hooks::path_for(self.scope, project) {
            None => self.load_error = Some("no home directory".into()),
            Some(p) => match hooks::load(&p) {
                Ok(l) => {
                    self.edit = l.hooks.clone();
                    self.loaded = Some(l);
                    self.dirty = false;
                    self.loaded_scope = self.scope;
                }
                Err(e) => {
                    self.loaded = None;
                    self.load_error = Some(e.to_string());
                }
            },
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, project: &Path, actions: &mut Vec<Action>) {
        if self.loaded.is_none() && self.load_error.is_none() {
            self.reload(project);
        }
        ui.horizontal(|ui| {
            ui.label("Scope");
            let mut changed = false;
            changed |= ui.selectable_value(&mut self.scope, Scope::Project, "project").changed();
            changed |= ui.selectable_value(&mut self.scope, Scope::ProjectLocal, "project-local").changed();
            changed |= ui.selectable_value(&mut self.scope, Scope::User, "user").changed();
            // Unsaved hook commands are hand-written and not recoverable, so a
            // scope switch or reload that would discard them asks first.
            if changed {
                if self.dirty {
                    self.scope = self.loaded_scope;
                    self.status = "Unsaved changes: save them, or press Discard to reload".into();
                } else {
                    self.reload(project);
                }
            }
            if ui.button("Reload").clicked() {
                if self.dirty {
                    self.status = "Unsaved changes: press Discard to throw them away".into();
                } else {
                    self.reload(project);
                }
            }
            if self.dirty && ui.button("Discard").clicked() {
                self.dirty = false;
                self.reload(project);
            }
            let can_save = self.loaded.is_some() && self.dirty;
            if ui.add_enabled(can_save, egui::Button::new("Save")).clicked() {
                if let Some(l) = &self.loaded {
                    match hooks::save(l, &self.edit) {
                        Ok(()) => {
                            self.status = format!("Saved {}", l.path.display());
                            self.reload(project);
                        }
                        Err(e) => self.status = e.to_string(),
                    }
                }
            }
            if let Some(p) = hooks::path_for(self.scope, project) {
                let exists = if p.exists() { "" } else { " (will be created)" };
                ui.small(format!("{}{exists}", p.display()));
            }
        });
        if let Some(e) = &self.load_error {
            ui.colored_label(egui::Color32::from_rgb(0xf7, 0x76, 0x8e), e);
            return;
        }
        if let Some(l) = &self.loaded {
            if l.path.exists() {
                if let Some(p) = l.path.file_name() {
                    let bak = l.path.with_file_name(format!("{}.bak", p.to_string_lossy()));
                    if bak.exists() {
                        ui.small(format!("backup: {}", bak.display()));
                    }
                }
            }
        }
        if !self.status.is_empty() {
            ui.small(&self.status);
        }
        ui.separator();

        // Add event.
        ui.horizontal(|ui| {
            ui.label("Add event");
            egui::ComboBox::from_id_salt("hook-event")
                .selected_text(if self.new_event.is_empty() { "pick" } else { self.new_event.as_str() })
                .show_ui(ui, |ui| {
                    for e in KNOWN_EVENTS {
                        ui.selectable_value(&mut self.new_event, e.to_string(), *e);
                    }
                });
            ui.add(egui::TextEdit::singleline(&mut self.new_event).desired_width(160.0).hint_text("or type one"));
            if ui.button("Add").clicked() && !self.new_event.trim().is_empty() {
                let name = self.new_event.trim().to_string();
                if !self.edit.events.iter().any(|(e, _)| *e == name) {
                    self.edit.events.push((name, vec![Group { matcher: None, hooks: vec![Command { command: String::new(), timeout: None, extra: Default::default() }], extra: Default::default() }]));
                    self.dirty = true;
                }
                self.new_event.clear();
            }
        });

        let cwd = project.to_path_buf();
        let mut remove_event: Option<usize> = None;
        let mut start_test: Option<(String, String)> = None;
        egui::ScrollArea::vertical().max_height(260.0).auto_shrink([false, false]).show(ui, |ui| {
            for (ei, (event, groups)) in self.edit.events.iter_mut().enumerate() {
                let takes_matcher = matches!(event.as_str(), "PreToolUse" | "PostToolUse" | "Notification" | "PreCompact" | "SessionStart" | "SessionEnd");
                egui::CollapsingHeader::new(format!("{event}  ({} group(s))", groups.len()))
                    .id_salt(("hook-ev", ei))
                    .default_open(true)
                    .show(ui, |ui| {
                        let mut remove_group: Option<usize> = None;
                        for (gi, g) in groups.iter_mut().enumerate() {
                            ui.horizontal(|ui| {
                                ui.label("matcher");
                                let mut m = g.matcher.clone().unwrap_or_default();
                                let r = ui.add_enabled(takes_matcher, egui::TextEdit::singleline(&mut m).desired_width(160.0).hint_text(if takes_matcher { "Bash|Edit" } else { "no matcher (fires on every event)" }));
                                if r.changed() {
                                    g.matcher = if m.is_empty() { None } else { Some(m) };
                                    self.dirty = true;
                                }
                                if ui.small_button("remove group").clicked() {
                                    remove_group = Some(gi);
                                }
                            });
                            let mut remove_cmd: Option<usize> = None;
                            for (ci, c) in g.hooks.iter_mut().enumerate() {
                                ui.horizontal(|ui| {
                                    ui.add_space(16.0);
                                    if ui.add(egui::TextEdit::singleline(&mut c.command).desired_width(420.0).font(egui::TextStyle::Monospace)).changed() {
                                        self.dirty = true;
                                    }
                                    if ui.small_button("Test").clicked() {
                                        start_test = Some((event.clone(), c.command.clone()));
                                    }
                                    if ui.small_button(crate::icons::CLOSE).clicked() {
                                        remove_cmd = Some(ci);
                                    }
                                });
                            }
                            if let Some(ci) = remove_cmd {
                                g.hooks.remove(ci);
                                self.dirty = true;
                            }
                            if ui.small_button("+ command").clicked() {
                                g.hooks.push(Command { command: String::new(), timeout: None, extra: Default::default() });
                                self.dirty = true;
                            }
                            ui.add_space(4.0);
                        }
                        if let Some(gi) = remove_group {
                            groups.remove(gi);
                            self.dirty = true;
                        }
                        ui.horizontal(|ui| {
                            if ui.small_button("+ group").clicked() {
                                groups.push(Group { matcher: None, hooks: Vec::new(), extra: Default::default() });
                                self.dirty = true;
                            }
                            if ui.small_button("remove event").clicked() {
                                remove_event = Some(ei);
                            }
                        });
                    });
            }
        });
        if let Some(ei) = remove_event {
            self.edit.events.remove(ei);
            self.dirty = true;
        }
        if let Some((event, command)) = start_test {
            let stdin = hooks::sample_stdin(&event, &cwd);
            let argv = if cfg!(windows) { format!("cmd /c {command}") } else { format!("sh -c '{command}'") };
            self.test = Some(TestState { command, stdin, argv, rx: None, result: None });
        }
        let _ = actions;

        // Dry-run panel.
        let mut close_test = false;
        if let Some(t) = &mut self.test {
            ui.separator();
            ui.strong("Test hook");
            ui.small(format!("argv: {}", t.argv));
            ui.small("stdin (editable):");
            ui.add(egui::TextEdit::multiline(&mut t.stdin).desired_rows(3).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace));
            let running = t.rx.is_some();
            let mut run = false;
            ui.horizontal(|ui| {
                if ui.add_enabled(!running, egui::Button::new("Run")).clicked() {
                    run = true;
                }
                if running {
                    ui.spinner();
                    ui.small("30 s timeout; a cancelled hook's descendants are not reaped");
                }
                if ui.button("Close").clicked() {
                    close_test = true;
                }
            });
            if run {
                let (tx, rx) = channel();
                let (cmd, stdin, cwd) = (t.command.clone(), t.stdin.clone(), cwd.clone());
                std::thread::spawn(move || {
                    let _ = tx.send(hooks::run_test(&cmd, &stdin, &cwd, Duration::from_secs(30)));
                });
                t.rx = Some(rx);
                t.result = None;
            }
            if let Some(rx) = &t.rx {
                if let Ok(r) = rx.try_recv() {
                    t.result = Some(r);
                    t.rx = None;
                } else {
                    ui.ctx().request_repaint_after(Duration::from_millis(100));
                }
            }
            if let Some(r) = &t.result {
                let code = r.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "none".into());
                ui.label(format!("exit {code}  in {:?}{}", r.duration, if r.timed_out { "  (timed out, killed)" } else { "" }));
                if !r.stdout.is_empty() {
                    ui.label(egui::RichText::new(&r.stdout).monospace().small());
                }
                if !r.stderr.is_empty() {
                    ui.label(egui::RichText::new(&r.stderr).monospace().small().color(egui::Color32::from_rgb(0xf7, 0x76, 0x8e)));
                }
            }
        }
        if close_test {
            self.test = None;
        }
    }
}
