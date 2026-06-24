//! Sculpt tab: the egui UI, terrain rendering, brush overlay, undo/redo, and
//! convert wiring for the height-sculpting workspace.
//!
//! Rendering split (the "60fps brush feel" requirement, spec §5):
//! - The terrain is a single egui texture regenerated ONLY when the field
//!   changes (`dirty`) — a hypsometric colormap + hillshade. Steady frames reuse
//!   the cached texture, so a large canvas costs nothing to redraw.
//! - The brush cursor is an egui overlay circle painted every frame, fully
//!   decoupled from the grid, with a smoothly animated radius — so resizing the
//!   brush animates at display rate regardless of canvas size.
//!
//! A pointer drag lays dabs along its path; undo snapshots the affected rect
//! before each stroke. Convert hands the field to [`super::convert_heightfield`]
//! on a worker thread (the same Promise pattern as the Map tab's build).

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use egui::{Color32, Pos2, Rect, Sense, Stroke, TextEdit, Ui, Vec2};
use poll_promise::Promise;

use crate::gui::build::{self, BuildError, BuildOutcome, BuildStage};
use crate::gui::theme::{STATUS_ERROR_FG, STATUS_WARN_FG};

use super::brush::{Brush, BrushShape, Falloff};
use super::convert::{convert_heightfield, OutputOptions};
use super::heightfield::{FieldMeta, HeightField, FLOOR_M};
use super::tools::{Flatten, Lower, Raise, SetHeight, Smooth, Tool};

/// Cap on the undo/redo history depth (spec §6, "deque capped at ~32").
const UNDO_CAP: usize = 32;
/// Dab spacing along a drag, as a fraction of the brush radius — closely spaced
/// dabs make a continuous stroke without redundant O(radius²) work per pixel.
const DAB_SPACING: f32 = 0.25;
/// Minimum world-space cells between dabs, so a tiny radius still advances.
const MIN_DAB_STEP_CELLS: f32 = 0.5;
/// How fast the overlay brush radius eases toward its target, in cells/second of
/// animation — large enough to feel instant, small enough to read as a smooth
/// grow/shrink rather than a snap.
const BRUSH_ANIM_SPEED: f32 = 80.0;

/// Default blank-canvas dimensions and pitch.
const DEFAULT_CANVAS_W: u32 = 256;
const DEFAULT_CANVAS_H: u32 = 256;
const DEFAULT_CELL_M: f64 = 4.0;

/// The five MVP height tools, as a UI-dispatchable enum. Each maps to a [`Tool`]
/// impl in `tools.rs`; color/scatter tools (future MVPs) extend the same trait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SculptTool {
    Raise,
    Lower,
    Smooth,
    Flatten,
    Set,
}

impl SculptTool {
    const ALL: [SculptTool; 5] =
        [Self::Raise, Self::Lower, Self::Smooth, Self::Flatten, Self::Set];

    const fn label(self) -> &'static str {
        match self {
            Self::Raise => "Raise",
            Self::Lower => "Lower",
            Self::Smooth => "Smooth",
            Self::Flatten => "Flatten",
            Self::Set => "Set height",
        }
    }

    /// Strength is meters-per-dab for Raise/Lower; a 0..=1 blend factor for the
    /// pull-toward-a-value tools (Smooth/Flatten/Set). Different default ranges
    /// keep one slider sensible across all five.
    const fn strength_is_blend(self) -> bool {
        matches!(self, Self::Smooth | Self::Flatten | Self::Set)
    }

    /// Apply one dab of this tool to `field` at `center`, shaped by `brush`,
    /// using `target` for the value-driven tools. Dispatches to the trait impls.
    fn apply_dab(self, field: &mut HeightField, brush: &Brush, center: (f32, f32), target: f32) {
        match self {
            Self::Raise => Raise.apply(field, brush, center),
            Self::Lower => Lower.apply(field, brush, center),
            Self::Smooth => Smooth.apply(field, brush, center),
            Self::Flatten => Flatten { target }.apply(field, brush, center),
            Self::Set => SetHeight { target }.apply(field, brush, center),
        }
    }
}

/// A pre-stroke snapshot of the cells a stroke can touch, for exact undo/redo.
/// Stores the affected bounding rect (`x0,y0,w,h`) and a row-major copy of its
/// `f32` cells — rect-sized, so memory is bounded by the brush footprint, not
/// the whole field (spec §6).
#[derive(Clone)]
struct RectSnapshot {
    x0: u32,
    y0: u32,
    w: u32,
    h: u32,
    cells: Vec<f32>,
}

impl RectSnapshot {
    /// Capture the rect `[x0, x0+w) × [y0, y0+h)` of `field`. Caller guarantees
    /// the rect is in bounds (clamped by `stroke_rect`).
    fn capture(field: &HeightField, x0: u32, y0: u32, w: u32, h: u32) -> Self {
        debug_assert!(x0 + w <= field.width && y0 + h <= field.height, "snapshot rect OOB");
        let mut cells = Vec::with_capacity((w as usize) * (h as usize));
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                cells.push(field.at(x, y));
            }
        }
        Self { x0, y0, w, h, cells }
    }

    /// Restore this snapshot's cells into `field`, returning a snapshot of what
    /// was there *before* the restore (so redo can re-apply). Exact (bit-for-bit
    /// `f32`) restoration.
    fn restore_into(&self, field: &mut HeightField) -> RectSnapshot {
        let prior = RectSnapshot::capture(field, self.x0, self.y0, self.w, self.h);
        let mut i = 0usize;
        for y in self.y0..self.y0 + self.h {
            for x in self.x0..self.x0 + self.w {
                // Direct index write: the snapshot holds already-valid (>= floor)
                // values captured from the field, so `set`'s clamp is a no-op —
                // but go through `set` to keep the floor invariant a single
                // chokepoint.
                field.set(x, y, self.cells[i]);
                i += 1;
            }
        }
        prior
    }
}

/// All sculpt-tab state: the editable field, the active tool + brush, the view
/// transform, the cached terrain texture, undo/redo history, output options, and
/// the convert worker handle.
pub(crate) struct SculptState {
    field: Option<HeightField>,
    tool: SculptTool,
    brush: Brush,
    /// Target height (meters above floor) for Flatten/Set.
    target_height: f32,

    // View transform: `pan` is the screen-space top-left of cell (0,0); `zoom`
    // is screen pixels per cell. Both are user-driven (drag-pan with the middle
    // button, scroll to zoom).
    pan: Vec2,
    zoom: f32,
    view_initialized: bool,

    // Rendering cache: the terrain texture is rebuilt only when `dirty`.
    texture: Option<egui::TextureHandle>,
    dirty: bool,
    /// Inclusive cell rect `(x0, x1, y0, y1)` changed since the last texture
    /// render, or `None` when the whole texture needs a rebuild (first render,
    /// new field, undo/redo, or a colormap-rescaling extent change). A live drag
    /// accumulates only the brush footprints here, so each dragged frame uploads
    /// just that sub-rect via `set_partial` instead of the whole field.
    dirty_rect: Option<(u32, u32, u32, u32)>,
    /// Cached global `(min, max)` cell height used by the hypsometric colormap,
    /// as of the last texture render. Folded monotonically from each dirtied
    /// rect so a partial render reuses it without a second full-field scan; a
    /// change to it forces a full re-render (the colormap rescales). `None`
    /// until the first render computes it.
    render_extent: Option<(f32, f32)>,
    /// Smoothly-animated overlay brush radius (cells), eased toward
    /// `brush.radius_cells` each frame for the 60fps resize feel.
    anim_radius: f32,

    // Bounded per-stroke history. `undo` holds pre-stroke snapshots; `redo` holds
    // the snapshots produced by undoing (i.e. the post-stroke state).
    undo: VecDeque<RectSnapshot>,
    redo: VecDeque<RectSnapshot>,
    /// Per-dab pre-edit snapshots of the in-progress stroke. Each entry captures
    /// only its own dab's rect (bounded `O(radius²)`) **before** that dab edits,
    /// so growth cost stays `O(new cells)` per dab instead of re-capturing the
    /// whole accumulated union every dab (the super-linear trap). They are
    /// collapsed into one union snapshot, earliest-pre-stroke-value-wins, by
    /// `commit_stroke`. Empty between strokes.
    active_stroke: Vec<RectSnapshot>,
    /// Last dab center (cells) during a drag, to space dabs along the path.
    last_dab: Option<(f32, f32)>,

    // Blank-canvas + output controls.
    new_w: u32,
    new_h: u32,
    new_cell_m: f64,
    output_name: String,
    out: OutputOptions,

    // Convert worker (same Promise pattern as the Map tab).
    convert_promise: Option<Promise<Result<BuildOutcome, BuildError>>>,
    convert_progress: Arc<Mutex<(BuildStage, f32)>>,
    convert_cancel: Arc<AtomicBool>,
    last_outcome: Option<BuildOutcome>,
    last_error: Option<String>,
}

impl Default for SculptState {
    fn default() -> Self {
        Self {
            field: None,
            tool: SculptTool::Raise,
            brush: Brush {
                shape: BrushShape::Circle,
                radius_cells: 12.0,
                strength: 5.0,
                falloff: Falloff::Smoothstep,
            },
            target_height: 20.0,
            pan: Vec2::ZERO,
            zoom: 4.0,
            view_initialized: false,
            texture: None,
            dirty: true,
            dirty_rect: None,
            render_extent: None,
            anim_radius: 12.0,
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            active_stroke: Vec::new(),
            last_dab: None,
            new_w: DEFAULT_CANVAS_W,
            new_h: DEFAULT_CANVAS_H,
            new_cell_m: DEFAULT_CELL_M,
            output_name: "sculpt".to_string(),
            out: OutputOptions::default(),
            convert_promise: None,
            convert_progress: Arc::new(Mutex::new((BuildStage::GeneratingBricks, 0.0))),
            convert_cancel: Arc::new(AtomicBool::new(false)),
            last_outcome: None,
            last_error: None,
        }
    }
}

impl SculptState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Load a field into the tab (from Send-to-Sculpt or a fresh canvas/image),
    /// resetting view + history. Marks dirty so the texture rebuilds.
    pub(crate) fn set_field(&mut self, field: HeightField) {
        self.field = Some(field);
        self.dirty = true;
        // A new field invalidates the cached extent and any partial dirty rect:
        // force a full texture rebuild and a fresh global min/max scan.
        self.dirty_rect = None;
        self.render_extent = None;
        self.view_initialized = false;
        self.undo.clear();
        self.redo.clear();
        self.active_stroke.clear();
        self.last_dab = None;
    }

    /// Mark a cell rect (inclusive `x0,x1,y0,y1`) changed since the last render,
    /// unioning into any pending sub-rect, and flag the texture dirty. If a full
    /// rebuild is already pending (`dirty && dirty_rect.is_none()`) it stays full.
    /// Used by the dab path to bound per-frame texture work to the brush
    /// footprint.
    fn mark_dirty_rect(&mut self, rect: (u32, u32, u32, u32)) {
        let pending_full = self.dirty && self.dirty_rect.is_none();
        self.dirty = true;
        if pending_full {
            return;
        }
        self.dirty_rect = Some(match self.dirty_rect {
            None => rect,
            Some(cur) => (
                cur.0.min(rect.0),
                cur.1.max(rect.1),
                cur.2.min(rect.2),
                cur.3.max(rect.3),
            ),
        });
    }

    /// Flag the whole texture for a full rebuild on the next render — for
    /// non-local changes (undo/redo, load) where a sub-rect cannot describe what
    /// moved.
    fn mark_dirty_all(&mut self) {
        self.dirty = true;
        self.dirty_rect = None;
    }

    fn is_converting(&self) -> bool {
        self.convert_promise.is_some()
    }

    /// Signal an in-flight convert worker to stop (app shutdown). The detached
    /// thread observes the flag and aborts before the heavy mesh/write.
    pub(crate) fn cancel_convert(&self) {
        self.convert_cancel.store(true, Ordering::Relaxed);
    }
}

/// Blank-canvas metadata: a sane default scale for an authored-from-scratch
/// field. The pitch comes from the New-canvas control; the rest mirror a
/// faithful 1:1 map build at a walkable scale.
fn blank_meta(cell_m: f64) -> FieldMeta {
    FieldMeta {
        cell_m,
        studs_per_meter: 4.0,
        vertical_exaggeration: 1.0,
        micro: false,
        centroid_lat: 0.0,
        source_name: "sculpt".to_string(),
    }
}

/// Entry point: draw the whole Sculpt tab.
pub(crate) fn draw(state: &mut SculptState, ctx: &egui::Context, ui: &mut Ui) {
    poll_convert_promise(state);
    if state.is_converting() {
        ctx.request_repaint_after(std::time::Duration::from_millis(120));
    }

    egui::SidePanel::right("sculpt_controls")
        .resizable(true)
        .default_width(280.0)
        .show_inside(ui, |ui| draw_controls(state, ui));
    egui::CentralPanel::default().show_inside(ui, |ui| draw_canvas(state, ctx, ui));
}

// ----- Controls panel ------------------------------------------------------

fn draw_controls(state: &mut SculptState, ui: &mut Ui) {
    ui.heading("Sculpt");
    ui.separator();

    draw_new_canvas_section(state, ui);
    ui.add_space(8.0);

    if state.field.is_none() {
        ui.label(
            "No canvas yet. Create a blank canvas above, load a heightmap image, \
             or use “Send to Sculpt” on the Map tab.",
        );
        return;
    }

    draw_tool_section(state, ui);
    ui.add_space(8.0);
    draw_brush_section(state, ui);
    ui.add_space(8.0);
    draw_history_section(state, ui);
    ui.add_space(8.0);
    draw_output_section(state, ui);
    ui.add_space(8.0);
    draw_convert_section(state, ui);
    ui.add_space(6.0);
    draw_last_result(state, ui);
}

fn draw_new_canvas_section(state: &mut SculptState, ui: &mut Ui) {
    ui.collapsing("New / load", |ui| {
        ui.horizontal(|ui| {
            ui.label("Width");
            ui.add(egui::DragValue::new(&mut state.new_w).range(2..=4096).speed(1.0));
            ui.label("Height");
            ui.add(egui::DragValue::new(&mut state.new_h).range(2..=4096).speed(1.0));
        });
        ui.horizontal(|ui| {
            ui.label("Cell size (m)");
            ui.add(
                egui::DragValue::new(&mut state.new_cell_m)
                    .range(0.1..=1000.0)
                    .speed(0.1),
            );
        });
        // Guard the cell budget the converter enforces, so a too-big canvas is
        // refused at creation rather than failing only at Convert.
        let cells = u64::from(state.new_w) * u64::from(state.new_h);
        let over = cells > crate::gui::tiles::MAX_DEM_CELLS;
        if over {
            ui.colored_label(
                STATUS_WARN_FG,
                format!(
                    "{cells} cells exceeds the {} budget — shrink the canvas.",
                    crate::gui::tiles::MAX_DEM_CELLS
                ),
            );
        }
        if ui.add_enabled(!over, egui::Button::new("New blank canvas")).clicked() {
            let meta = blank_meta(state.new_cell_m);
            state.set_field(HeightField::flat(state.new_w, state.new_h, meta));
        }
        if ui.button("Load heightmap image…").clicked() {
            load_heightmap_image(state);
        }
    });
}

fn draw_tool_section(state: &mut SculptState, ui: &mut Ui) {
    ui.label("Tool");
    egui::ComboBox::from_id_salt("sculpt_tool_combo")
        .selected_text(state.tool.label())
        .width(260.0)
        .show_ui(ui, |ui| {
            for t in SculptTool::ALL {
                if ui.selectable_label(state.tool == t, t.label()).clicked() {
                    state.tool = t;
                }
            }
        });
    if matches!(state.tool, SculptTool::Flatten | SculptTool::Set) {
        ui.horizontal(|ui| {
            ui.label("Target height (m)");
            ui.add(
                egui::DragValue::new(&mut state.target_height)
                    .range(0.0..=100_000.0)
                    .speed(0.5),
            );
        });
    }
}

fn draw_brush_section(state: &mut SculptState, ui: &mut Ui) {
    ui.label("Brush");
    ui.add(
        egui::Slider::new(&mut state.brush.radius_cells, 1.0..=200.0)
            .text("radius (cells)")
            .logarithmic(true),
    );
    let strength_range = if state.tool.strength_is_blend() {
        0.0..=1.0
    } else {
        0.0..=200.0
    };
    ui.add(egui::Slider::new(&mut state.brush.strength, strength_range).text("strength"));
    ui.horizontal(|ui| {
        ui.label("Falloff");
        ui.selectable_value(&mut state.brush.falloff, Falloff::Smoothstep, "Smooth");
        ui.selectable_value(&mut state.brush.falloff, Falloff::Linear, "Linear");
        ui.selectable_value(&mut state.brush.falloff, Falloff::Constant, "Hard");
    });
}

fn draw_history_section(state: &mut SculptState, ui: &mut Ui) {
    ui.horizontal(|ui| {
        if ui.add_enabled(!state.undo.is_empty(), egui::Button::new("⟲ Undo")).clicked() {
            do_undo(state);
        }
        if ui.add_enabled(!state.redo.is_empty(), egui::Button::new("⟳ Redo")).clicked() {
            do_redo(state);
        }
    });
}

fn draw_output_section(state: &mut SculptState, ui: &mut Ui) {
    ui.label("Output name");
    ui.add(
        TextEdit::singleline(&mut state.output_name)
            .hint_text("sculpt")
            .desired_width(260.0),
    );
    ui.checkbox(&mut state.out.brdb, "World (.brdb → Worlds/)");
    ui.checkbox(&mut state.out.brz, "Prefab (.brz → Prefabs/)");
    ui.checkbox(&mut state.out.install_to_brickadia, "Install into Brickadia");
    ui.checkbox(&mut state.out.overwrite, "Overwrite existing");
    ui.checkbox(&mut state.out.skip_floor, "Skip flat floor (reveal native ground)")
        .on_hover_text(
            "On (default for sculpt): flat (zero-height) areas emit no bricks, so the native \
             Brickadia floor shows through. Off: a watertight base plate is built under \
             everything (matches a direct Map build).",
        );
}

fn draw_convert_section(state: &mut SculptState, ui: &mut Ui) {
    if state.is_converting() {
        let (stage, fraction) = match state.convert_progress.lock() {
            Ok(g) => *g,
            Err(_) => (BuildStage::GeneratingBricks, 0.0),
        };
        ui.label(stage.label());
        ui.add(egui::ProgressBar::new(fraction.clamp(0.0, 1.0)).show_percentage());
        if ui.button("Cancel").clicked() {
            state.convert_cancel.store(true, Ordering::Relaxed);
        }
        return;
    }
    let name_empty = state.output_name.trim().is_empty();
    let no_format = !state.out.brdb && !state.out.brz;
    let enabled = !name_empty && !no_format;
    let resp = ui.add_enabled(
        enabled,
        egui::Button::new("⬇  Convert to bricks").min_size(Vec2::new(260.0, 32.0)),
    );
    if name_empty {
        ui.colored_label(STATUS_WARN_FG, "• Output name is empty");
    }
    if no_format {
        ui.colored_label(STATUS_WARN_FG, "• Select at least one output format");
    }
    if enabled && resp.clicked() {
        start_convert(state);
    }
}

fn draw_last_result(state: &SculptState, ui: &mut Ui) {
    if let Some(outcome) = &state.last_outcome {
        ui.colored_label(
            STATUS_WARN_FG,
            format!(
                "✔ {} bricks · {}×{} cells · {:.0}–{:.0} m",
                outcome.brick_count,
                outcome.dem_width,
                outcome.dem_height,
                outcome.elevation_min_m,
                outcome.elevation_max_m,
            ),
        );
        if let Some(dest) = &outcome.installed_path {
            ui.small(format!("installed → {}", dest.display()));
        } else {
            ui.small(format!("wrote → {}", outcome.brdb_path.display()));
        }
        if let Some(warn) = &outcome.install_warning {
            ui.colored_label(STATUS_ERROR_FG, format!("⚠ {warn}"));
        }
    }
    if let Some(err) = &state.last_error {
        ui.colored_label(STATUS_ERROR_FG, format!("✘ {err}"));
    }
}

// ----- Canvas: render + interaction ----------------------------------------

fn draw_canvas(state: &mut SculptState, ctx: &egui::Context, ui: &mut Ui) {
    let Some(field) = state.field.as_ref() else {
        ui.centered_and_justified(|ui| {
            ui.label("Create or load a canvas to start sculpting.");
        });
        return;
    };
    let (fw, fh) = (field.width, field.height);

    // Allocate the whole central area as one interactive surface.
    let avail = ui.available_size();
    let (rect, response) = ui.allocate_exact_size(avail, Sense::click_and_drag());

    // Initialize the view to fit-and-center the field the first time it is shown.
    if !state.view_initialized {
        init_view(state, rect, fw, fh);
    }

    // Rebuild the terrain texture only when the field changed (dirty), and then
    // upload only the dirtied sub-rect when possible (a live drag), instead of
    // re-rendering + re-uploading the whole field every frame.
    if state.dirty || state.texture.is_none() {
        regen_texture(state, ctx);
        state.dirty = false;
        state.dirty_rect = None;
    }

    // Handle zoom (scroll) and pan (middle/secondary drag) before painting so the
    // overlay and dabs use the up-to-date transform.
    handle_view_input(state, &response, ui, rect);

    // Paint the terrain texture at the current pan/zoom.
    paint_terrain(state, ui, rect, fw, fh);

    // Apply sculpt dabs from a primary-button drag.
    handle_sculpt_input(state, &response, fw, fh);

    // Brush overlay + smooth radius animation. Animating every frame while the
    // pointer is over the canvas keeps the resize buttery regardless of grid
    // size (it touches no texture).
    paint_brush_overlay(state, ctx, ui, &response, rect);
}

/// Center + fit the field in the viewport: zoom so the longer axis fills ~90% of
/// the rect, pan so the field is centered.
fn init_view(state: &mut SculptState, rect: Rect, fw: u32, fh: u32) {
    let margin = 0.9;
    let zx = rect.width() * margin / (fw.max(1) as f32);
    let zy = rect.height() * margin / (fh.max(1) as f32);
    state.zoom = zx.min(zy).clamp(0.05, 64.0);
    let field_px = Vec2::new(fw as f32 * state.zoom, fh as f32 * state.zoom);
    state.pan = rect.min.to_vec2() + (rect.size() - field_px) * 0.5;
    state.view_initialized = true;
}

/// Scroll-to-zoom (anchored at the pointer) and middle/secondary-drag-to-pan.
fn handle_view_input(state: &mut SculptState, response: &egui::Response, ui: &Ui, rect: Rect) {
    // Pan with the middle or secondary button so the primary button is free for
    // sculpting (mirrors the Map tab's "secondary/middle pans while drawing").
    if response.dragged_by(egui::PointerButton::Middle)
        || response.dragged_by(egui::PointerButton::Secondary)
    {
        state.pan += response.drag_delta();
    }

    // Scroll-to-zoom, keeping the cell under the pointer fixed.
    let scroll = ui.input(|i| i.smooth_scroll_delta.y);
    if scroll.abs() > 0.0
        && response.hovered()
        && let Some(ptr) = response.hover_pos()
    {
        let old_zoom = state.zoom;
        let factor = (scroll * 0.005).exp();
        let new_zoom = (old_zoom * factor).clamp(0.05, 64.0);
        if (new_zoom - old_zoom).abs() > f32::EPSILON {
            // Keep the cell under the pointer stationary: solve pan so the
            // pointer maps to the same cell before and after the zoom change.
            let cell = (ptr - rect.min.to_vec2() - state.pan) / old_zoom;
            state.pan = (ptr - rect.min.to_vec2()) - cell * new_zoom;
            state.zoom = new_zoom;
        }
    }
}

/// Screen position of cell-space coordinate `(cx, cy)`.
fn cell_to_screen(state: &SculptState, cx: f32, cy: f32) -> Pos2 {
    Pos2::new(state.pan.x + cx * state.zoom, state.pan.y + cy * state.zoom)
}

/// Cell-space coordinate under screen position `p` (inverse of `cell_to_screen`).
fn screen_to_cell(state: &SculptState, p: Pos2) -> (f32, f32) {
    (
        (p.x - state.pan.x) / state.zoom,
        (p.y - state.pan.y) / state.zoom,
    )
}

fn paint_terrain(state: &SculptState, ui: &Ui, rect: Rect, fw: u32, fh: u32) {
    let Some(tex) = state.texture.as_ref() else { return };
    let top_left = cell_to_screen(state, 0.0, 0.0);
    let bottom_right = cell_to_screen(state, fw as f32, fh as f32);
    let img_rect = Rect::from_min_max(top_left, bottom_right);
    let painter = ui.painter_at(rect);
    let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
    painter.image(tex.id(), img_rect, uv, Color32::WHITE);
}

/// Lay sculpt dabs along a primary-button drag, spaced ~`radius * DAB_SPACING`.
/// Snapshots the affected rect at stroke start (for undo) and commits it when the
/// stroke ends.
fn handle_sculpt_input(state: &mut SculptState, response: &egui::Response, fw: u32, fh: u32) {
    // Sculpt only on the PRIMARY button; middle/secondary are pan.
    if response.drag_started_by(egui::PointerButton::Primary) {
        // Begin a stroke: capture a snapshot of the WHOLE field's potentially
        // affected region. We don't know the full stroke extent up front, so we
        // grow the snapshot rect lazily — capture the first dab's rect now and
        // union as the stroke extends (see `extend_stroke_snapshot`).
        state.active_stroke.clear();
        state.last_dab = None;
    }

    let primary_down = response.dragged_by(egui::PointerButton::Primary)
        || response.drag_started_by(egui::PointerButton::Primary);
    if primary_down
        && let Some(ptr) = response.interact_pointer_pos()
    {
        let (cx, cy) = screen_to_cell(state, ptr);
        apply_dabs_along(state, (cx, cy), fw, fh);
    }

    if response.drag_stopped_by(egui::PointerButton::Primary) {
        commit_stroke(state);
    }
}

/// Step from the last dab center to `(cx, cy)`, applying a dab at each step so a
/// fast drag still paints a continuous stroke. Each dab grows the stroke's undo
/// snapshot to cover its rect, then edits the field.
fn apply_dabs_along(state: &mut SculptState, to: (f32, f32), fw: u32, fh: u32) {
    let step = (state.brush.radius_cells * DAB_SPACING).max(MIN_DAB_STEP_CELLS);
    let from = state.last_dab.unwrap_or(to);
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let dist = (dx * dx + dy * dy).sqrt();
    // Number of intermediate dab positions (at least one: the destination).
    let n = (dist / step).floor() as u32;
    // Bounded loop (Rule 2): cap the per-call dab count so a huge teleport (e.g.
    // a tiny radius dragged across the whole canvas in one frame) can't spin.
    let n = n.min(4096);
    for i in 1..=n {
        let t = (i as f32) * step / dist.max(f32::EPSILON);
        let c = (from.0 + dx * t, from.1 + dy * t);
        apply_one_dab(state, c, fw, fh);
    }
    // Always stamp the destination so a click (no drag distance) still paints.
    apply_one_dab(state, to, fw, fh);
    state.last_dab = Some(to);
}

/// Grow the stroke snapshot to cover this dab's rect, then apply the dab.
fn apply_one_dab(state: &mut SculptState, center: (f32, f32), fw: u32, fh: u32) {
    let Some(rect) = stroke_rect(center, state.brush.radius_cells, fw, fh) else {
        return; // entirely off-grid
    };
    extend_stroke_snapshot(state, rect);
    let field = state.field.as_mut().expect("field present during sculpt");
    let tool = state.tool;
    let brush = state.brush;
    let target = state.target_height;
    tool.apply_dab(field, &brush, center, target);
    // Mark exactly this dab's footprint dirty so the texture regen uploads only
    // the brush rect (converted to the inclusive `x0,x1,y0,y1` the renderer
    // wants), keeping per-dragged-frame texture cost O(brush rect), not O(canvas).
    let (rx, ry, rw, rh) = rect;
    state.mark_dirty_rect((rx, rx + rw - 1, ry, ry + rh - 1));
}

/// Integer cell rect a dab of radius `r` at `center` can touch, clamped to the
/// field. `None` when entirely off-grid. Inclusive-exclusive `(x0, y0, w, h)`.
fn stroke_rect(center: (f32, f32), r: f32, fw: u32, fh: u32) -> Option<(u32, u32, u32, u32)> {
    if r <= 0.0 || fw == 0 || fh == 0 {
        return None;
    }
    let (cx, cy) = center;
    let min_x = (cx - r).floor();
    let max_x = (cx + r).ceil();
    let min_y = (cy - r).floor();
    let max_y = (cy + r).ceil();
    let last_x = (fw - 1) as f32;
    let last_y = (fh - 1) as f32;
    if max_x < 0.0 || min_x > last_x || max_y < 0.0 || min_y > last_y {
        return None;
    }
    let x0 = min_x.max(0.0) as u32;
    let x1 = (max_x.min(last_x)) as u32;
    let y0 = min_y.max(0.0) as u32;
    let y1 = (max_y.min(last_y)) as u32;
    Some((x0, y0, x1 - x0 + 1, y1 - y0 + 1))
}

/// Capture this dab's own pre-edit rect and append it to the in-progress
/// stroke's per-dab snapshot list. Called BEFORE the dab edits the field, so each
/// snapshot holds the field values immediately prior to that dab — and the
/// EARLIEST snapshot covering any given cell holds that cell's true pre-stroke
/// value. The list is collapsed into one union snapshot at [`commit_stroke`].
///
/// Cost is `O(dab rect)` per dab — bounded by the brush footprint — instead of
/// re-capturing (and re-copying) the whole growing union every dab, which made a
/// long stroke super-linear.
fn extend_stroke_snapshot(state: &mut SculptState, rect: (u32, u32, u32, u32)) {
    let (rx, ry, rw, rh) = rect;
    let field = state.field.as_ref().expect("field present during sculpt");
    state.active_stroke.push(RectSnapshot::capture(field, rx, ry, rw, rh));
}

/// Collapse the in-progress stroke's per-dab snapshots into one union snapshot
/// (the bounding rect of every dab) and commit it to the undo history.
///
/// The union is filled from the CURRENT (post-stroke) field — correct for cells
/// inside the bounding rect that no dab touched — then each per-dab snapshot is
/// overlaid from LAST to FIRST, so the earliest (= true pre-stroke) value wins
/// for any cell multiple dabs covered. This reproduces exactly the same single
/// pre-stroke snapshot the old grow-the-union code produced, but pays only
/// `O(stroke length × brush rect)` once at commit instead of per dab.
fn commit_stroke(state: &mut SculptState) {
    if let Some(snap) = collapse_stroke_snapshots(state) {
        state.undo.push_back(snap);
        while state.undo.len() > UNDO_CAP {
            state.undo.pop_front();
        }
        state.redo.clear();
    }
    state.active_stroke.clear();
    state.last_dab = None;
}

/// Build the single union pre-stroke snapshot from the per-dab list, or `None`
/// if the stroke captured nothing (every dab was off-grid).
fn collapse_stroke_snapshots(state: &SculptState) -> Option<RectSnapshot> {
    let dabs = &state.active_stroke;
    let first = dabs.first()?;
    // Bounding rect of all per-dab snapshots.
    let mut ux0 = first.x0;
    let mut uy0 = first.y0;
    let mut ux1 = first.x0 + first.w;
    let mut uy1 = first.y0 + first.h;
    for d in &dabs[1..] {
        ux0 = ux0.min(d.x0);
        uy0 = uy0.min(d.y0);
        ux1 = ux1.max(d.x0 + d.w);
        uy1 = uy1.max(d.y0 + d.h);
    }

    let field = state.field.as_ref().expect("field present at stroke commit");
    let mut union = RectSnapshot::capture(field, ux0, uy0, ux1 - ux0, uy1 - uy0);
    // Overlay each dab snapshot earliest-LAST, so the first dab to touch a cell
    // (its true pre-stroke value) wins over any later dab that re-covered it.
    for d in dabs.iter().rev() {
        overlay_into(&mut union, d);
    }
    Some(union)
}

/// Overlay `src`'s cells onto `dst` where they overlap (`src` is fully contained
/// in `dst` by construction of the union rect), keeping `src`'s values.
fn overlay_into(dst: &mut RectSnapshot, src: &RectSnapshot) {
    for y in src.y0..src.y0 + src.h {
        for x in src.x0..src.x0 + src.w {
            let di = ((y - dst.y0) * dst.w + (x - dst.x0)) as usize;
            let si = ((y - src.y0) * src.w + (x - src.x0)) as usize;
            dst.cells[di] = src.cells[si];
        }
    }
}

fn do_undo(state: &mut SculptState) {
    let Some(snap) = state.undo.pop_back() else { return };
    let Some(field) = state.field.as_mut() else { return };
    let redo_snap = snap.restore_into(field);
    state.redo.push_back(redo_snap);
    while state.redo.len() > UNDO_CAP {
        state.redo.pop_front();
    }
    // Undo can change the global extent (e.g. removing the tallest peak), which
    // rescales the whole colormap — force a full re-render, not a partial.
    state.mark_dirty_all();
}

fn do_redo(state: &mut SculptState) {
    let Some(snap) = state.redo.pop_back() else { return };
    let Some(field) = state.field.as_mut() else { return };
    let undo_snap = snap.restore_into(field);
    state.undo.push_back(undo_snap);
    while state.undo.len() > UNDO_CAP {
        state.undo.pop_front();
    }
    // Redo likewise can change the global extent → full re-render.
    state.mark_dirty_all();
}

/// Draw the brush cursor — a circle following the pointer with a smoothly eased
/// radius. Decoupled from the grid texture, so it animates at frame rate.
fn paint_brush_overlay(
    state: &mut SculptState,
    ctx: &egui::Context,
    ui: &Ui,
    response: &egui::Response,
    rect: Rect,
) {
    // Ease the displayed radius toward the target each frame (frame-rate
    // independent via the actual delta-time).
    let dt = ctx.input(|i| i.stable_dt).clamp(0.0, 0.1);
    let target = state.brush.radius_cells;
    let delta = target - state.anim_radius;
    if delta.abs() > 1e-3 {
        let step = BRUSH_ANIM_SPEED * dt;
        if delta.abs() <= step {
            state.anim_radius = target;
        } else {
            state.anim_radius += step.copysign(delta);
        }
        // Keep animating until settled.
        ctx.request_repaint();
    }

    if !response.hovered() {
        return;
    }
    let Some(ptr) = response.hover_pos() else { return };
    let radius_px = state.anim_radius * state.zoom;
    let painter = ui.painter_at(rect);
    // Outer ring + a faint inner fill so the cursor reads on both light and dark
    // terrain.
    painter.circle_stroke(ptr, radius_px, Stroke::new(2.0, Color32::from_rgb(0xFF, 0xE0, 0x60)));
    painter.circle_stroke(
        ptr,
        radius_px,
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(0x10, 0x10, 0x10, 0xC0)),
    );
    // A center crosshair pip.
    painter.circle_filled(ptr, 1.5, Color32::from_rgb(0xFF, 0xE0, 0x60));

    // Only request a repaint when there's actual work to show: a live drag (so
    // painting tracks at frame rate), or the pointer moved this frame (so the
    // cursor ring follows). When the pointer is stationary and nothing is
    // animating, let egui idle instead of pinning the UI at full frame rate.
    // (The brush-size ease above already self-requests a repaint while easing.)
    let pointer_moved = ctx.input(|i| i.pointer.delta()) != Vec2::ZERO;
    if response.dragged() || pointer_moved {
        ctx.request_repaint();
    }
}

// ----- Terrain image: hypsometric colormap + hillshade ---------------------

/// (Re)build the cached terrain texture, uploading the minimum needed:
/// - **Partial** (a live drag): only the dirtied cell rect changed *and* the
///   global hypsometric extent (min/max) is unchanged, so render just that
///   sub-rect (expanded one cell for the hillshade halo) and `set_partial` it.
///   Per-dragged-frame cost is `O(brush rect)`, not `O(canvas)`.
/// - **Full** (first render, new field, undo/redo, or an extent change that
///   rescales the whole colormap): render the whole field and `set` it.
///
/// The extent is cached in `state.render_extent` and folded monotonically from
/// each dirtied rect, so the common partial path never re-scans the whole field.
/// (A tool that lowers the single tallest peak leaves the cached max slightly
/// high until the next full render — a purely cosmetic, self-healing bound on
/// colour saturation, not a stale or mis-shaded sub-rect.)
fn regen_texture(state: &mut SculptState, ctx: &egui::Context) {
    let field = state.field.as_ref().expect("field present");
    let (fw, fh) = (field.width, field.height);
    let opts = egui::TextureOptions::LINEAR;

    // Decide partial vs full. Partial requires: an existing texture, a bounded
    // dirty rect, and that folding the dirty rect's own extent leaves the global
    // (min, max) the colormap normalizes against unchanged.
    let partial = match (state.texture.is_some(), state.dirty_rect, state.render_extent) {
        (true, Some(rect), Some(prev_extent)) => {
            let rect_mm = rect_min_max(field, rect);
            let folded = (prev_extent.0.min(rect_mm.0), prev_extent.1.max(rect_mm.1));
            (folded == prev_extent).then_some((rect, prev_extent))
        }
        _ => None,
    };

    if let Some((rect, extent)) = partial {
        // Expand the changed rect by one cell so the hillshade of the border
        // cells (which reads their now-changed neighbours) is recomputed too.
        let halo = expand_rect(rect, 1, fw, fh);
        let (hx0, _, hy0, _) = halo;
        let sub = render_field_rect(field, halo, extent.0, extent.1);
        let tex = state.texture.as_mut().expect("partial path has a texture");
        tex.set_partial([hx0 as usize, hy0 as usize], sub, opts);
        return;
    }

    // Full render: recompute the true global extent and cache it.
    let extent = field.min_max();
    let image = render_field_rect(field, (0, fw - 1, 0, fh - 1), extent.0, extent.1);
    match state.texture.as_mut() {
        Some(tex) => tex.set(image, opts),
        None => state.texture = Some(ctx.load_texture("sculpt_terrain", image, opts)),
    }
    state.render_extent = Some(extent);
}

/// Min/max cell height over the inclusive rect `(x0, x1, y0, y1)`. Bounded scan
/// of just that rect — used to fold a dirtied region's extent into the cached
/// global without a full-field rescan.
fn rect_min_max(field: &HeightField, rect: (u32, u32, u32, u32)) -> (f32, f32) {
    let (x0, x1, y0, y1) = rect;
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    for y in y0..=y1 {
        for x in x0..=x1 {
            let v = field.at(x, y);
            min = min.min(v);
            max = max.max(v);
        }
    }
    if min.is_finite() && max.is_finite() {
        (min, max)
    } else {
        (FLOOR_M, FLOOR_M)
    }
}

/// Expand an inclusive cell rect by `pad` cells on every side, clamped to the
/// field bounds `[0, fw-1] × [0, fh-1]`.
fn expand_rect(rect: (u32, u32, u32, u32), pad: u32, fw: u32, fh: u32) -> (u32, u32, u32, u32) {
    let (x0, x1, y0, y1) = rect;
    (
        x0.saturating_sub(pad),
        (x1 + pad).min(fw - 1),
        y0.saturating_sub(pad),
        (y1 + pad).min(fh - 1),
    )
}

/// Render the inclusive cell rect `(x0, x1, y0, y1)` of `field` into a
/// `ColorImage` sized exactly to that rect: a hypsometric tint by height
/// normalized against `(min, max)`, modulated by a Lambert hillshade from the
/// surface gradient. Neighbour reads for the hillshade clamp to the *field*
/// edges (not the rect), so a sub-rect renders identically to the same cells in
/// a full-field render — the partial-upload path is pixel-identical to a full
/// rebuild for the cells it covers.
fn render_field_rect(
    field: &HeightField,
    rect: (u32, u32, u32, u32),
    min: f32,
    max: f32,
) -> egui::ColorImage {
    let (x0, x1, y0, y1) = rect;
    let rw = (x1 - x0 + 1) as usize;
    let rh = (y1 - y0 + 1) as usize;
    let span = (max - min).max(1e-6);
    let last_x = field.width - 1;
    let last_y = field.height - 1;
    // Light direction (top-left, slightly elevated) for the hillshade.
    let light = normalize3([-0.5, -0.5, 0.7]);

    let mut rgba = vec![0u8; rw * rh * 4];
    for (ry, y) in (y0..=y1).enumerate() {
        for (rx, x) in (x0..=x1).enumerate() {
            let height = field.at(x, y);
            let t = ((height - min) / span).clamp(0.0, 1.0);
            let base = hypsometric(t);

            // Surface normal from central differences (cells are heights in
            // meters; the cell pitch cancels into a constant slope scale).
            let slope = 1.0_f32; // relative; visual only
            let xl = field.at(x.saturating_sub(1), y);
            let xr = field.at((x + 1).min(last_x), y);
            let yu = field.at(x, y.saturating_sub(1));
            let yd = field.at(x, (y + 1).min(last_y));
            let nx = (xl - xr) * slope;
            let ny = (yu - yd) * slope;
            let normal = normalize3([nx, ny, 2.0]);
            let lambert = dot3(normal, light).clamp(0.0, 1.0);
            // Blend the flat base toward shaded so even flat ground keeps color.
            let shade = 0.55 + 0.45 * lambert;

            let idx = (ry * rw + rx) * 4;
            rgba[idx] = (base[0] as f32 * shade) as u8;
            rgba[idx + 1] = (base[1] as f32 * shade) as u8;
            rgba[idx + 2] = (base[2] as f32 * shade) as u8;
            rgba[idx + 3] = 0xFF;
        }
    }
    egui::ColorImage::from_rgba_unmultiplied([rw, rh], &rgba)
}

/// A coarse hypsometric ramp: deep green lowlands → tan → brown → grey/white
/// peaks. `t` is normalized height in `[0, 1]`. Floor (t≈0) reads as a flat
/// green so a blank canvas is visibly the ground plane.
fn hypsometric(t: f32) -> [u8; 3] {
    // Piecewise linear over four stops.
    const STOPS: [(f32, [f32; 3]); 5] = [
        (0.00, [60.0, 110.0, 70.0]),   // lowland green
        (0.30, [120.0, 150.0, 80.0]),  // grass/tan
        (0.55, [165.0, 140.0, 95.0]),  // tan/brown
        (0.80, [140.0, 120.0, 110.0]), // rocky brown-grey
        (1.00, [235.0, 235.0, 240.0]), // snow/peak
    ];
    let mut lo = STOPS[0];
    let mut hi = STOPS[STOPS.len() - 1];
    for w in STOPS.windows(2) {
        if t >= w[0].0 && t <= w[1].0 {
            lo = w[0];
            hi = w[1];
            break;
        }
    }
    let seg = (hi.0 - lo.0).max(1e-6);
    let f = ((t - lo.0) / seg).clamp(0.0, 1.0);
    [
        (lo.1[0] + (hi.1[0] - lo.1[0]) * f) as u8,
        (lo.1[1] + (hi.1[1] - lo.1[1]) * f) as u8,
        (lo.1[2] + (hi.1[2] - lo.1[2]) * f) as u8,
    ]
}

fn normalize3(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-6);
    [v[0] / len, v[1] / len, v[2] / len]
}

fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

// ----- Load heightmap image ------------------------------------------------

/// Open a grayscale (or any) image as a `HeightField` via the native file
/// dialog. Decoding happens synchronously here — image headers/sizes are small
/// at sculpt scales, and this is a one-shot user action, not a per-frame path.
fn load_heightmap_image(state: &mut SculptState) {
    let result = native_dialog::DialogBuilder::file()
        .add_filter("Image Files", ["png", "jpg", "jpeg"])
        .open_single_file()
        .show();
    let path = match result {
        Ok(Some(p)) => p,
        Ok(None) => return, // user cancelled
        Err(e) => {
            state.last_error = Some(format!("file dialog failed: {e}"));
            return;
        }
    };
    let img = match image::ImageReader::open(&path)
        .and_then(|r| r.decode().map_err(std::io::Error::other))
    {
        Ok(im) => im.to_luma8(),
        Err(e) => {
            state.last_error = Some(format!("could not read image: {e}"));
            return;
        }
    };
    if img.width() == 0 || img.height() == 0 {
        state.last_error = Some("image has zero dimensions".to_owned());
        return;
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sculpt".to_owned());
    let mut meta = blank_meta(state.new_cell_m);
    meta.source_name = stem;
    state.set_field(HeightField::from_image(&img, meta));
    state.last_error = None;
}

// ----- Convert worker ------------------------------------------------------

fn start_convert(state: &mut SculptState) {
    let Some(field) = state.field.clone() else {
        state.last_error = Some("internal: Convert with no field".into());
        return;
    };
    // Stamp the current output name into the field metadata so the written file
    // is named from the UI, not the field's original source.
    let mut field = field;
    field.meta.source_name = state.output_name.clone();

    let out = state.out;
    state.last_outcome = None;
    state.last_error = None;
    state.convert_cancel.store(false, Ordering::Relaxed);
    if let Ok(mut g) = state.convert_progress.lock() {
        *g = (BuildStage::GeneratingBricks, 0.0);
    }
    let progress_arc = Arc::clone(&state.convert_progress);
    let cancel_arc = Arc::clone(&state.convert_cancel);
    let progress_fn: build::ProgressFn = Arc::new(move |stage, f| {
        if let Ok(mut g) = progress_arc.lock() {
            *g = (stage, f);
        }
    });
    let (sender, promise) = Promise::new();
    match std::thread::Builder::new()
        .name("h2brz-sculpt-convert".into())
        .spawn(move || {
            let result = convert_heightfield(&field, out, progress_fn, cancel_arc);
            sender.send(result);
        }) {
        Ok(_handle) => state.convert_promise = Some(promise),
        Err(e) => {
            state.last_error = Some(format!("could not start convert thread: {e}"));
        }
    }
}

fn poll_convert_promise(state: &mut SculptState) {
    let Some(promise) = state.convert_promise.as_ref() else {
        return;
    };
    if promise.ready().is_none() {
        return;
    }
    let promise = state.convert_promise.take().expect("just verified Some");
    match promise.try_take() {
        Ok(Ok(outcome)) => {
            state.last_outcome = Some(outcome);
            state.last_error = None;
        }
        Ok(Err(err)) => {
            state.last_error = Some(err.to_string());
            state.last_outcome = None;
        }
        Err(_) => {
            state.convert_promise = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> FieldMeta {
        FieldMeta {
            cell_m: 4.0,
            studs_per_meter: 4.0,
            vertical_exaggeration: 1.0,
            micro: false,
            centroid_lat: 0.0,
            source_name: "undo-test".to_string(),
        }
    }

    /// Snapshot a rect, edit it, then undo — the field must return to its exact
    /// prior state (bit-for-bit f32), and redo must re-apply the edit exactly.
    #[test]
    fn undo_restores_exact_prior_state() {
        let mut state = SculptState::new();
        // A varied field so an off-by-one in the snapshot rect would show.
        let mut field = HeightField::flat(40, 40, meta());
        for y in 0..40 {
            for x in 0..40 {
                field.set(x, y, ((x * 3 + y * 7) % 17) as f32);
            }
        }
        let original = field.clone();
        state.set_field(field);
        state.tool = SculptTool::Raise;
        state.brush = Brush {
            shape: BrushShape::Circle,
            radius_cells: 6.0,
            strength: 12.0,
            falloff: Falloff::Smoothstep,
        };

        // Simulate a multi-dab stroke: begin, several dabs along a path, commit.
        apply_one_dab(&mut state, (10.0, 10.0), 40, 40);
        apply_one_dab(&mut state, (18.0, 14.0), 40, 40);
        apply_one_dab(&mut state, (25.0, 22.0), 40, 40);
        commit_stroke(&mut state);

        let after_edit = state.field.as_ref().unwrap().clone();
        assert_ne!(
            after_edit.cells, original.cells,
            "the stroke must have actually changed the field",
        );
        assert_eq!(state.undo.len(), 1, "one stroke → one undo entry");

        // Undo: exact restoration to the original.
        do_undo(&mut state);
        let restored = state.field.as_ref().unwrap().clone();
        assert_eq!(
            restored.cells, original.cells,
            "undo must restore the EXACT prior state (bit-for-bit)",
        );
        assert!(state.undo.is_empty(), "undo stack drained");
        assert_eq!(state.redo.len(), 1, "undo pushes a redo entry");

        // Redo: re-apply the exact post-stroke state.
        do_redo(&mut state);
        let redone = state.field.as_ref().unwrap().clone();
        assert_eq!(
            redone.cells, after_edit.cells,
            "redo must reproduce the exact post-stroke state",
        );
    }

    /// A single dab outside the field is a no-op (off-grid rect → None).
    #[test]
    fn off_grid_dab_is_noop() {
        let mut state = SculptState::new();
        let field = HeightField::flat(10, 10, meta());
        let original = field.clone();
        state.set_field(field);
        state.brush.radius_cells = 2.0;
        apply_one_dab(&mut state, (100.0, 100.0), 10, 10);
        assert_eq!(
            state.field.as_ref().unwrap().cells,
            original.cells,
            "an off-grid dab must not touch any cell",
        );
        assert!(state.active_stroke.is_empty(), "off-grid dab captures no snapshot");
    }

    /// Undo history is bounded: committing more than UNDO_CAP strokes drops the
    /// oldest, never growing without limit.
    #[test]
    fn undo_history_is_bounded() {
        let mut state = SculptState::new();
        state.set_field(HeightField::flat(20, 20, meta()));
        state.brush.radius_cells = 3.0;
        state.brush.strength = 1.0;
        for i in 0..(UNDO_CAP + 10) {
            let c = (5.0 + (i % 10) as f32, 5.0 + (i % 7) as f32);
            apply_one_dab(&mut state, c, 20, 20);
            commit_stroke(&mut state);
        }
        assert!(
            state.undo.len() <= UNDO_CAP,
            "undo deque must stay within the cap, got {}",
            state.undo.len(),
        );
    }

    /// Finding-1 guard: a stroke whose dabs OVERLAP each other must still undo to
    /// the exact pre-stroke state. The per-dab snapshot list is collapsed with
    /// "earliest dab wins" for any cell several dabs touched, so the union
    /// snapshot holds each cell's true pre-stroke value — even though a later
    /// dab's snapshot captured an already-edited value for the overlap.
    #[test]
    fn overlapping_dab_stroke_undo_is_exact() {
        let mut state = SculptState::new();
        let mut field = HeightField::flat(30, 30, meta());
        for y in 0..30 {
            for x in 0..30 {
                field.set(x, y, ((x * 5 + y * 11) % 13) as f32);
            }
        }
        let original = field.clone();
        state.set_field(field);
        state.tool = SculptTool::Raise;
        state.brush = Brush {
            shape: BrushShape::Circle,
            radius_cells: 5.0,
            strength: 9.0,
            falloff: Falloff::Smoothstep,
        };

        // Heavily overlapping dabs: each center is within a radius of the last, so
        // their rects share many cells. A later dab thus snapshots cells the
        // earlier dab already raised — only earliest-wins collapse restores them.
        for c in [(12.0, 12.0), (13.5, 12.5), (15.0, 13.0), (16.0, 14.0)] {
            apply_one_dab(&mut state, c, 30, 30);
        }
        commit_stroke(&mut state);

        let after = state.field.as_ref().unwrap().clone();
        assert_ne!(after.cells, original.cells, "the overlapping stroke must edit the field");
        assert_eq!(state.undo.len(), 1, "a stroke collapses to one undo entry");

        do_undo(&mut state);
        assert_eq!(
            state.field.as_ref().unwrap().cells,
            original.cells,
            "undo of an overlapping-dab stroke must restore the exact pre-stroke field",
        );
        do_redo(&mut state);
        assert_eq!(
            state.field.as_ref().unwrap().cells,
            after.cells,
            "redo must reproduce the exact post-stroke field",
        );
    }

    /// Finding-2 guard: rendering a SUB-RECT of the field (the partial-upload
    /// path) is pixel-identical, for the cells it covers, to a full-field render
    /// at the same global extent. This is the correctness precondition for
    /// `set_partial` — a dragged frame uploads only the brush rect yet the result
    /// is bit-for-bit what a full rebuild would have produced there.
    #[test]
    fn partial_render_matches_full_render() {
        let mut field = HeightField::flat(48, 36, meta());
        // Relief so the hillshade (neighbour-dependent) actually varies per pixel.
        for y in 0..36 {
            for x in 0..48 {
                let v = ((x as f32 * 0.4).sin() * 10.0 + (y as f32 * 0.6).cos() * 6.0 + 30.0)
                    .max(FLOOR_M);
                field.set(x, y, v);
            }
        }
        let (min, max) = field.min_max();
        let full = render_field_rect(&field, (0, field.width - 1, 0, field.height - 1), min, max);

        // A halo-expanded interior sub-rect, exactly as the drag path would build.
        let sub_rect = expand_rect((20, 27, 14, 19), 1, field.width, field.height);
        let (sx0, sx1, sy0, sy1) = sub_rect;
        let sub = render_field_rect(&field, sub_rect, min, max);

        let fw = field.width as usize;
        for (ry, y) in (sy0..=sy1).enumerate() {
            for (rx, x) in (sx0..=sx1).enumerate() {
                let full_px = full.pixels[(y as usize) * fw + (x as usize)];
                let sub_px = sub.pixels[ry * (sub_rect.1 - sub_rect.0 + 1) as usize + rx];
                assert_eq!(
                    full_px, sub_px,
                    "partial render differs from full render at cell ({x},{y})",
                );
            }
        }
    }

    /// `mark_dirty_rect` unions sub-rects but a pending FULL rebuild (no rect)
    /// subsumes them, so a partial upload never silently overrides a needed full
    /// re-render (e.g. after undo).
    #[test]
    fn dirty_rect_union_and_full_subsumption() {
        let mut state = SculptState::new();
        state.set_field(HeightField::flat(64, 64, meta()));
        // set_field forces a full rebuild (dirty, no rect).
        assert!(state.dirty && state.dirty_rect.is_none());
        // A sub-rect mark while a full rebuild is pending stays full.
        state.mark_dirty_rect((10, 12, 10, 12));
        assert!(state.dirty_rect.is_none(), "a pending full rebuild subsumes sub-rects");

        // After a render clears the gate, sub-rects union together.
        state.dirty = false;
        state.dirty_rect = None;
        state.mark_dirty_rect((10, 12, 20, 22));
        state.mark_dirty_rect((30, 32, 8, 9));
        assert_eq!(
            state.dirty_rect,
            Some((10, 32, 8, 22)),
            "two dirtied dab rects must union to their bounding rect",
        );
    }
}
