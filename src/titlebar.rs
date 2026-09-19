//! Custom window chrome drawn with the app theme. The OS title bar is turned
//! off at launch (`ViewportBuilder::with_decorations(false)`); this module
//! draws a title strip that moves the window on drag, toggles maximise on
//! double-click, hosts minimise/maximise/close buttons, and turns the outer
//! few pixels of the window into resize handles.

use eframe::egui::{self, CursorIcon, ResizeDirection, Sense, ViewportCommand};

use crate::icons;

const BAR_HEIGHT: f32 = 30.0;
const EDGE: f32 = 6.0;

/// Draw the title strip as a top panel. Call before other panels.
pub fn show(ctx: &egui::Context, title: &str) {
    egui::TopBottomPanel::top("titlebar")
        .exact_height(BAR_HEIGHT)
        .show_separator_line(false)
        .show(ctx, |ui| {
            let rect = ui.max_rect();
            let bar = ui.interact(rect, ui.id().with("drag"), Sense::click_and_drag());
            if bar.double_clicked() {
                let max = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                ctx.send_viewport_cmd(ViewportCommand::Maximized(!max));
            } else if bar.drag_started() {
                ctx.send_viewport_cmd(ViewportCommand::StartDrag);
            }

            ui.horizontal_centered(|ui| {
                ui.add_space(10.0);
                ui.label(egui::RichText::new(icons::IDE).color(ui.visuals().selection.stroke.color));
                ui.label(egui::RichText::new(title).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let danger = egui::Color32::from_rgb(0xf7, 0x76, 0x8e);
                    if window_button(ui, icons::CLOSE, Some(danger)).clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Close);
                    }
                    let max = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                    let glyph = if max { icons::RESTORE } else { icons::MAXIMIZE };
                    if window_button(ui, glyph, None).clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Maximized(!max));
                    }
                    if window_button(ui, icons::MINIMIZE, None).clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
                    }
                });
            });
        });
}

fn window_button(ui: &mut egui::Ui, glyph: &str, hover: Option<egui::Color32>) -> egui::Response {
    let size = egui::vec2(46.0, BAR_HEIGHT);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.hovered() {
        let fill = hover.unwrap_or(ui.visuals().widgets.hovered.bg_fill);
        ui.painter().rect_filled(rect, 0.0, fill);
    }
    let color = if resp.hovered() && hover.is_some() {
        egui::Color32::WHITE
    } else {
        ui.visuals().text_color()
    };
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(14.0),
        color,
    );
    resp
}

/// Resize handles on the window edges. Call once per frame after all panels
/// so the handles sit on top. No-op when the window is maximised.
pub fn resize_handles(ctx: &egui::Context) {
    if ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
        return;
    }
    let screen = ctx.screen_rect();
    let dirs: [(ResizeDirection, egui::Rect, CursorIcon); 8] = [
        (ResizeDirection::North, egui::Rect::from_min_max(screen.min + egui::vec2(EDGE, 0.0), egui::pos2(screen.max.x - EDGE, screen.min.y + EDGE)), CursorIcon::ResizeVertical),
        (ResizeDirection::South, egui::Rect::from_min_max(egui::pos2(screen.min.x + EDGE, screen.max.y - EDGE), screen.max - egui::vec2(EDGE, 0.0)), CursorIcon::ResizeVertical),
        (ResizeDirection::West, egui::Rect::from_min_max(screen.min + egui::vec2(0.0, EDGE), egui::pos2(screen.min.x + EDGE, screen.max.y - EDGE)), CursorIcon::ResizeHorizontal),
        (ResizeDirection::East, egui::Rect::from_min_max(egui::pos2(screen.max.x - EDGE, screen.min.y + EDGE), screen.max - egui::vec2(0.0, EDGE)), CursorIcon::ResizeHorizontal),
        (ResizeDirection::NorthWest, egui::Rect::from_min_size(screen.min, egui::vec2(EDGE, EDGE)), CursorIcon::ResizeNwSe),
        (ResizeDirection::NorthEast, egui::Rect::from_min_size(egui::pos2(screen.max.x - EDGE, screen.min.y), egui::vec2(EDGE, EDGE)), CursorIcon::ResizeNeSw),
        (ResizeDirection::SouthWest, egui::Rect::from_min_size(egui::pos2(screen.min.x, screen.max.y - EDGE), egui::vec2(EDGE, EDGE)), CursorIcon::ResizeNeSw),
        (ResizeDirection::SouthEast, egui::Rect::from_min_size(screen.max - egui::vec2(EDGE, EDGE), egui::vec2(EDGE, EDGE)), CursorIcon::ResizeNwSe),
    ];
    egui::Area::new(egui::Id::new("resize-handles"))
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .interactable(true)
        .show(ctx, |ui| {
            for (dir, rect, cursor) in dirs {
                let resp = ui.interact(rect, ui.id().with(format!("{dir:?}")), Sense::drag());
                if resp.hovered() || resp.dragged() {
                    ctx.set_cursor_icon(cursor);
                }
                if resp.drag_started() {
                    ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
                }
            }
        });
}
