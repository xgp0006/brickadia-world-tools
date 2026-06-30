//! Custom egui visuals: slate background, ember accent.
//!
//! The map widget's OSM tiles render with their own palette — we deliberately
//! do NOT tint them. The custom visuals apply only to the surrounding chrome:
//! panels, buttons, selectable states, sliders, separators.

use egui::{Color32, CornerRadius, Stroke, Visuals};

const SURFACE_BG: Color32 = Color32::from_rgb(0x1A, 0x1F, 0x26);
const SURFACE_PANEL: Color32 = Color32::from_rgb(0x22, 0x28, 0x31);
const SURFACE_RAISED: Color32 = Color32::from_rgb(0x2C, 0x34, 0x3F);
const SURFACE_HOVER: Color32 = Color32::from_rgb(0x35, 0x3F, 0x4D);
const ACCENT_EMBER: Color32 = Color32::from_rgb(0xC8, 0x64, 0x3C);
const ACCENT_EMBER_DIM: Color32 = Color32::from_rgb(0x8E, 0x4A, 0x2C);
const TEXT_PRIMARY: Color32 = Color32::from_rgb(0xE4, 0xE0, 0xD2);
/// High-contrast text on the ember selected-fill (legibility fix).
const TEXT_SELECTED: Color32 = Color32::from_rgb(0xFF, 0xF6, 0xEE);
const STROKE_DIM: Color32 = Color32::from_rgb(0x3A, 0x42, 0x4E);

/// Install the heightmap2brz visuals palette on the given egui context.
pub fn install_theme(ctx: &egui::Context) {
    let mut visuals = Visuals::dark();

    visuals.panel_fill = SURFACE_PANEL;
    visuals.window_fill = SURFACE_PANEL;
    visuals.extreme_bg_color = SURFACE_BG;
    visuals.faint_bg_color = SURFACE_RAISED;
    visuals.code_bg_color = SURFACE_BG;
    visuals.window_stroke = Stroke::new(1.0, STROKE_DIM);
    visuals.window_corner_radius = CornerRadius::same(6);
    visuals.menu_corner_radius = CornerRadius::same(4);

    visuals.widgets.noninteractive.bg_fill = SURFACE_PANEL;
    visuals.widgets.noninteractive.weak_bg_fill = SURFACE_PANEL;
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, STROKE_DIM);

    visuals.widgets.inactive.bg_fill = SURFACE_RAISED;
    visuals.widgets.inactive.weak_bg_fill = SURFACE_RAISED;
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, STROKE_DIM);

    visuals.widgets.hovered.bg_fill = SURFACE_HOVER;
    visuals.widgets.hovered.weak_bg_fill = SURFACE_HOVER;
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, ACCENT_EMBER);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT_EMBER_DIM);

    visuals.widgets.active.bg_fill = ACCENT_EMBER_DIM;
    visuals.widgets.active.weak_bg_fill = ACCENT_EMBER_DIM;
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT_EMBER);

    visuals.widgets.open.bg_fill = SURFACE_RAISED;
    visuals.widgets.open.weak_bg_fill = SURFACE_RAISED;
    visuals.widgets.open.fg_stroke = Stroke::new(1.0, ACCENT_EMBER);
    visuals.widgets.open.bg_stroke = Stroke::new(1.0, ACCENT_EMBER_DIM);

    // Selected widgets (selectable_label/value): a SOLID bright-ember fill with
    // near-white text. egui paints a selected label's text in `selection.stroke`,
    // so the old ember-on-dim-ember was unreadable — this is the contrast fix.
    visuals.selection.bg_fill = ACCENT_EMBER;
    visuals.selection.stroke = Stroke::new(1.0, TEXT_SELECTED);
    visuals.hyperlink_color = ACCENT_EMBER;
    visuals.warn_fg_color = Color32::from_rgb(0xE6, 0xB7, 0x4F);
    visuals.error_fg_color = Color32::from_rgb(0xE6, 0x6E, 0x52);

    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    style.visuals.widgets.noninteractive.fg_stroke.color = TEXT_PRIMARY;
    ctx.set_style(style);
}

/// The ember accent — for section headers and in-palette accents.
pub(crate) const ACCENT: Color32 = ACCENT_EMBER;
/// Bbox overlay stroke color — re-exported so map_tab.rs draws in palette.
pub(crate) const BBOX_STROKE: Color32 = ACCENT_EMBER;
/// Bbox overlay fill color (low alpha so the underlying tiles show through).
pub(crate) const BBOX_FILL: Color32 = Color32::from_rgba_premultiplied(0x4C, 0x26, 0x0F, 0x30);
/// Status-bar bbox error label color.
pub(crate) const STATUS_ERROR_FG: Color32 = Color32::from_rgb(0xE6, 0x6E, 0x52);
/// Settings warning ("config load failed") tint.
pub(crate) const STATUS_WARN_FG: Color32 = Color32::from_rgb(0xE6, 0xB7, 0x4F);
