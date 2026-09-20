//! "Agents, skills and hooks" window. Pure view: it renders the catalog and
//! returns actions for `main.rs` to apply (open in IDE, reveal, delete via the
//! app's confirm modal, refresh), so this file never touches the filesystem
//! except through `harness::create_*`.

use std::path::{Path, PathBuf};

use eframe::egui;

use crate::harness::{self, Catalog, Item, Scope};
use crate::icons;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Agents,
    Skills,
    Hooks,
}

#[derive(Clone, Copy, PartialEq)]
enum Filter {
    Both,
    Project,
    User,
}

pub enum Action {
    OpenInIde(PathBuf),
    Reveal(PathBuf),
    /// Ask the app to confirm and delete this item.
    Delete(Item),
    Refresh,
    Status(String),
}

/// One hook firing seen on the chat stream (`system/hook_started` or
/// `system/hook_response`). Kept so the Hooks tab shows real receipts.
#[derive(Clone, Debug)]
pub struct HookEvent {
    pub name: String,
    pub outcome: String,
    pub exit_code: Option<i64>,
    pub stdout: String,
    pub stderr: String,
}

pub struct HarnessWindow {
    pub open: bool,
    tab: Tab,
    filter: Filter,
    new_name: String,
    new_scope: Scope,
    pub catalog: Option<Catalog>,
    pub hook_feed: Vec<HookEvent>,
    /// Hooks editor, present once `hooks.rs` has loaded a file.
    pub hooks_ui: Option<crate::hooks_ui::HooksEditor>,
}

impl HarnessWindow {
    pub fn new() -> Self {
        Self {
            open: false,
            tab: Tab::Agents,
            filter: Filter::Both,
            new_name: String::new(),
            new_scope: Scope::Project,
            catalog: None,
            hook_feed: Vec::new(),
            hooks_ui: Some(crate::hooks_ui::HooksEditor::new()),
        }
    }

    pub fn push_hook_event(&mut self, ev: HookEvent) {
        self.hook_feed.push(ev);
        if self.hook_feed.len() > 200 {
            self.hook_feed.remove(0);
        }
    }

    /// Render as a separate native window (egui viewport). The immediate
    /// viewport runs inside the parent's frame, so actions come back the same
    /// way as before. Closing the OS window sets `open` false.
    pub fn show(&mut self, ctx: &egui::Context, project: &Path) -> Vec<Action> {
        let mut actions = Vec::new();
        if !self.open {
            return actions;
        }
        if self.catalog.is_none() {
            actions.push(Action::Refresh);
        }
        let id = egui::ViewportId::from_hash_of("harness-window");
        let builder = crate::titlebar::child_chrome(
            egui::ViewportBuilder::default()
                .with_title("Agents, skills and hooks")
                .with_inner_size([960.0, 620.0])
                .with_min_inner_size([640.0, 400.0]),
        );
        ctx.show_viewport_immediate(id, builder, |ctx, class| {
            if class == egui::ViewportClass::Embedded {
                // Backend cannot open native windows; fall back to an in-app window.
                egui::Window::new("Agents, skills and hooks").show(ctx, |ui| self.body(ui, project, &mut actions));
                return;
            }
            crate::titlebar::show(ctx, "Agents, skills and hooks");
            egui::CentralPanel::default().show(ctx, |ui| self.body(ui, project, &mut actions));
            crate::titlebar::resize_handles(ctx);
            // Escape closes as well as the window button; edits in the hooks
            // tab live in this struct and survive a close.
            if ctx.input(|i| i.viewport().close_requested() || i.key_pressed(egui::Key::Escape)) {
                self.open = false;
            }
        });
        actions
    }

    fn body(&mut self, ui: &mut egui::Ui, project: &Path, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Agents, "Agents");
            ui.selectable_value(&mut self.tab, Tab::Skills, "Skills");
            ui.selectable_value(&mut self.tab, Tab::Hooks, "Hooks");
            ui.separator();
            ui.label("Scope");
            ui.selectable_value(&mut self.filter, Filter::Both, "Both");
            ui.selectable_value(&mut self.filter, Filter::Project, "Project");
            ui.selectable_value(&mut self.filter, Filter::User, "User");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(format!("{} Refresh", icons::REFRESH)).clicked() {
                    actions.push(Action::Refresh);
                }
            });
        });
        ui.separator();
        match self.tab {
            Tab::Agents | Tab::Skills => self.items_tab(ui, project, actions),
            Tab::Hooks => self.hooks_tab(ui, project, actions),
        }
    }

    fn items_tab(&mut self, ui: &mut egui::Ui, project: &Path, actions: &mut Vec<Action>) {
        let is_agents = self.tab == Tab::Agents;
        // New item row.
        ui.horizontal(|ui| {
            ui.label(if is_agents { "New agent" } else { "New skill" });
            ui.add(egui::TextEdit::singleline(&mut self.new_name).desired_width(200.0).hint_text("name"));
            ui.radio_value(&mut self.new_scope, Scope::Project, "project");
            ui.radio_value(&mut self.new_scope, Scope::User, "user");
            if ui.button("Create").clicked() {
                let r = if is_agents {
                    harness::create_agent(project, self.new_scope, &self.new_name)
                } else {
                    harness::create_skill(project, self.new_scope, &self.new_name)
                };
                match r {
                    Ok(p) => {
                        self.new_name.clear();
                        actions.push(Action::Refresh);
                        actions.push(Action::OpenInIde(p));
                    }
                    Err(e) => actions.push(Action::Status(e)),
                }
            }
        });
        ui.separator();

        let Some(cat) = &self.catalog else {
            ui.label("scanning");
            return;
        };
        let items: Vec<&Item> = if is_agents { cat.agents.iter() } else { cat.skills.iter() }
            .filter(|i| match self.filter {
                Filter::Both => true,
                Filter::Project => i.scope == Scope::Project,
                Filter::User => i.scope == Scope::User,
            })
            .collect();
        if items.is_empty() {
            for (scope, root, exists) in &cat.roots {
                let sub = if is_agents { "agents" } else { "skills" };
                let p = root.join(sub);
                ui.small(format!(
                    "{}: {} {}",
                    scope.badge(),
                    p.display(),
                    if *exists && p.is_dir() { "is empty" } else { "does not exist" }
                ));
            }
            return;
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new(("harness-grid", is_agents))
                .num_columns(if is_agents { 6 } else { 4 })
                .striped(true)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    ui.strong("");
                    ui.strong("Name");
                    ui.strong("Description");
                    if is_agents {
                        ui.strong("Model");
                        ui.strong("Tools");
                    }
                    ui.strong("");
                    ui.end_row();
                    for it in items {
                        ui.label(egui::RichText::new(it.scope.badge()).weak());
                        ui.label(egui::RichText::new(&it.name).strong());
                        ui.scope(|ui| {
                            ui.set_min_width(if is_agents { 300.0 } else { 440.0 });
                            ui.add(egui::Label::new(egui::RichText::new(&it.description).small()).wrap());
                        });
                        if is_agents {
                            ui.label(it.model.clone().unwrap_or_else(|| "inherit".into()));
                            // Absent `tools:` means every tool. Never render it as an empty cell.
                            let tools = match &it.tools {
                                None => "all tools (inherited)".to_string(),
                                Some(t) => t.join(", "),
                            };
                            ui.scope(|ui| {
                                ui.set_min_width(160.0);
                                ui.add(egui::Label::new(egui::RichText::new(tools).small()).wrap());
                            });
                        }
                        ui.horizontal(|ui| {
                            if ui.small_button("Open").clicked() {
                                actions.push(Action::OpenInIde(it.path.clone()));
                            }
                            if ui.small_button("Reveal").clicked() {
                                actions.push(Action::Reveal(it.path.clone()));
                            }
                            if ui.small_button("Delete").clicked() {
                                actions.push(Action::Delete(it.clone()));
                            }
                        });
                        ui.end_row();
                    }
                });
        });
        ui.separator();
        let roots: Vec<String> = cat
            .roots
            .iter()
            .map(|(s, r, e)| format!("{}: {}{}", s.badge(), r.display(), if *e { "" } else { " (missing)" }))
            .collect();
        ui.small(roots.join("    "));
    }

    fn hooks_tab(&mut self, ui: &mut egui::Ui, project: &Path, actions: &mut Vec<Action>) {
        if let Some(h) = &mut self.hooks_ui {
            h.show(ui, project, actions);
        } else {
            ui.label("Hooks editor not loaded.");
        }
        ui.separator();
        ui.strong("Fired this session");
        ui.small("Receipts from the assistant stream: hook_started / hook_response events.");
        if self.hook_feed.is_empty() {
            ui.small("none yet");
            return;
        }
        egui::ScrollArea::vertical().max_height(160.0).auto_shrink([false, false]).show(ui, |ui| {
            for ev in self.hook_feed.iter().rev() {
                let code = ev.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "-".into());
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&ev.name).monospace().small());
                    ui.label(egui::RichText::new(format!("{} exit {}", ev.outcome, code)).small());
                    let out: String = ev.stdout.chars().take(120).collect();
                    if !out.is_empty() {
                        ui.label(egui::RichText::new(out).weak().small());
                    }
                    if !ev.stderr.is_empty() {
                        let err: String = ev.stderr.chars().take(120).collect();
                        ui.label(egui::RichText::new(err).color(egui::Color32::from_rgb(0xf7, 0x76, 0x8e)).small());
                    }
                });
            }
        });
    }
}
