//! Visual themes. `Omarchy` follows the Tokyo Night palette used by the
//! Omarchy desktop: near-black panels, muted blue accent, monospace text,
//! thin flat borders, no rounded chrome.

use eframe::egui::{self, Color32, FontFamily, FontId, Rounding, Stroke, TextStyle};

use crate::config::Theme;

pub fn apply(ctx: &egui::Context, theme: Theme) {
    use std::sync::Arc;
    let default_dark = {
        let mut s = egui::Style::default();
        s.visuals = egui::Visuals::dark();
        Arc::new(s)
    };
    match theme {
        Theme::System => {
            ctx.options_mut(|o| o.dark_style = default_dark);
            ctx.set_theme(egui::ThemePreference::System);
        }
        Theme::Light => {
            ctx.options_mut(|o| o.dark_style = default_dark);
            ctx.set_theme(egui::ThemePreference::Light);
        }
        Theme::Dark => {
            ctx.options_mut(|o| o.dark_style = default_dark);
            ctx.set_theme(egui::ThemePreference::Dark);
        }
        Theme::Omarchy => {
            ctx.options_mut(|o| o.dark_style = Arc::new(omarchy()));
            ctx.set_theme(egui::ThemePreference::Dark);
        }
    }
}

fn omarchy() -> egui::Style {
    let bg = Color32::from_rgb(0x1a, 0x1b, 0x26);
    let panel = Color32::from_rgb(0x16, 0x16, 0x1e);
    let surface = Color32::from_rgb(0x24, 0x28, 0x3b);
    let border = Color32::from_rgb(0x3b, 0x42, 0x61);
    let fg = Color32::from_rgb(0xc0, 0xca, 0xf5);
    let dim = Color32::from_rgb(0x56, 0x5f, 0x89);
    let accent = Color32::from_rgb(0x7a, 0xa2, 0xf7);
    let select = Color32::from_rgb(0x33, 0x46, 0x7c);

    let mut v = egui::Visuals::dark();
    v.override_text_color = Some(fg);
    v.panel_fill = panel;
    v.window_fill = bg;
    v.extreme_bg_color = bg;
    v.faint_bg_color = Color32::from_rgb(0x1f, 0x20, 0x2e);
    v.selection.bg_fill = select;
    v.selection.stroke = Stroke::new(1.0_f32, accent);
    v.hyperlink_color = accent;
    v.window_stroke = Stroke::new(1.0_f32, border);
    v.window_rounding = Rounding::ZERO;
    v.menu_rounding = Rounding::ZERO;

    v.widgets.noninteractive.bg_fill = panel;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, border);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, fg);
    v.weak_text_color(); // keep default weak derivation
    let _ = dim;
    v.widgets.inactive.bg_fill = surface;
    v.widgets.inactive.weak_bg_fill = surface;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, fg);
    v.widgets.hovered.bg_fill = select;
    v.widgets.hovered.weak_bg_fill = select;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, accent);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, fg);
    v.widgets.active.bg_fill = accent;
    v.widgets.active.weak_bg_fill = accent;
    v.widgets.active.fg_stroke = Stroke::new(1.0_f32, fg);
    v.widgets.open.bg_fill = surface;
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.rounding = Rounding::ZERO;
    }

    let mut s = egui::Style::default();
    s.visuals = v;
    s.spacing.item_spacing = egui::vec2(6.0, 4.0);
    s.spacing.button_padding = egui::vec2(8.0, 3.0);
    let mono = FontFamily::Monospace;
    s.text_styles = [
        (TextStyle::Small, FontId::new(11.0, mono.clone())),
        (TextStyle::Body, FontId::new(13.0, mono.clone())),
        (TextStyle::Monospace, FontId::new(13.0, mono.clone())),
        (TextStyle::Button, FontId::new(13.0, mono.clone())),
        (TextStyle::Heading, FontId::new(16.0, mono)),
    ]
    .into();
    s
}
