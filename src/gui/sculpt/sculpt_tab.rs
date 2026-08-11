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
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use egui::{Color32, Pos2, Rect, Sense, Stroke, StrokeKind, TextEdit, Ui, Vec2};
use poll_promise::Promise;

use crate::gui::build::{self, BuildError, BuildOutcome, BuildStage};
use crate::gui::theme::{STATUS_ERROR_FG, STATUS_WARN_FG};
use crate::gui::zones::{Zone, ZoneMode};

use super::brush::{shape_distance, Brush, BrushShape, Falloff};
use super::convert::{convert_heightfield, convert_heightfield_tiled, export_layer_parts, OutputOptions};
use super::layers;
use super::paint::{
    default_palette, gradient_palette, splatmap_to_indices, PaintGrid, PaintLayer, GRADIENT_BANDS,
    MAX_SWATCHES,
};
use super::heightfield::{terrace_height, FieldMeta, HeightField, FLOOR_M};
use super::tools::{Flatten, Lower, Raise, SetHeight, Smooth, Stamp, StampKind, Tool};

/// Cap on the undo/redo history depth (spec §6, "deque capped at ~32").
const UNDO_CAP: usize = 32;
/// Dab spacing along a drag, as a fraction of the brush radius — closely spaced
/// dabs make a continuous stroke without redundant O(radius²) work per pixel.
const DAB_SPACING: f32 = 0.25;
/// Minimum world-space cells between dabs, so a tiny radius still advances.
const MIN_DAB_STEP_CELLS: f32 = 0.5;

/// Lasso decimation: skip a sampled point closer than this (in cells) to the last
/// kept one, so a freehand drag stores a sparse loop, not every pixel.
const ZONE_LASSO_MIN_CELL_DIST: f32 = 0.75;
/// Polygon close tolerance: a click within this many SCREEN pixels of the first
/// vertex closes the loop.
const ZONE_POLY_CLOSE_PX: f32 = 12.0;
/// How fast the overlay brush radius eases toward its target, in cells/second of
/// animation — large enough to feel instant, small enough to read as a smooth
/// grow/shrink rather than a snap.
const BRUSH_ANIM_SPEED: f32 = 80.0;

/// Default blank-canvas dimensions and pitch.
const DEFAULT_CANVAS_W: u32 = 256;
const DEFAULT_CANVAS_H: u32 = 256;
const DEFAULT_CELL_M: f64 = 4.0;

/// Default scale knobs for a blank canvas / image / send-from-map seed when the
/// source carries none — a faithful 1:1 map build at a walkable studs/m.
const DEFAULT_STUDS_PER_METER: f32 = 4.0;
const DEFAULT_VERTICAL_EXAGGERATION: f32 = 1.0;
/// Default sub-field edge length (cells) for a manually-tiled export — the
/// initial `tile_cells` until the user retunes it.
const DEFAULT_TILE_CELLS: u32 = 256;

/// Fine/coarse step multipliers for [`modifier_step`] (spec §2). Ctrl makes a
/// DragValue ten-times finer, Alt ten-times coarser, none leaves the base.
const MODIFIER_FINE: f64 = 0.1;
const MODIFIER_COARSE: f64 = 10.0;

/// Per-frame keyboard-modifier state read for the DragValue step scaling. A pure
/// value so [`modifier_step`] is testable without an egui context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DragModifiers {
    ctrl: bool,
    alt: bool,
}

/// Choose the step multiplier for a hovered [`egui::DragValue`] from the held
/// modifiers (spec §2). **Ctrl = fine (×0.1)**, **Alt = coarse (×10)**, none =
/// base (×1.0). Precedence when BOTH are held: **Ctrl wins** (fine) — fine
/// adjustment is the safer default when a user fumbles both, and it keeps the
/// rule a simple "Ctrl ⇒ fine" the muscle memory can rely on. Pure so the
/// selection logic is unit-tested directly.
fn modifier_step(m: DragModifiers) -> f64 {
    if m.ctrl {
        MODIFIER_FINE
    } else if m.alt {
        MODIFIER_COARSE
    } else {
        1.0
    }
}

/// A [`egui::DragValue`] whose drag speed scales with the held keyboard
/// modifiers while the widget is hovered (spec §2): Ctrl ⇒ fine (×0.1), Alt ⇒
/// coarse (×10), none ⇒ `base_speed`. `range` clamps the value. Applied to every
/// numeric DragValue in the Sculpt/Export UI so one rule governs all of them.
///
/// The step is read from the live `ui.input` modifiers every frame, but egui
/// only applies a `DragValue`'s `speed` while that widget is being DRAGGED, so a
/// widget that isn't actively dragged keeps its base behavior regardless.
fn modifier_drag<N: egui::emath::Numeric>(
    ui: &mut Ui,
    value: &mut N,
    base_speed: f64,
    range: std::ops::RangeInclusive<N>,
) -> egui::Response {
    let mods = ui.input(|i| i.modifiers);
    let step = modifier_step(DragModifiers { ctrl: mods.ctrl, alt: mods.alt });
    ui.add(egui::DragValue::new(value).range(range).speed(base_speed * step))
}

/// Effective value-box drag speed for a [`modifier_slider`] given the held
/// modifiers: `base_speed` scaled by [`modifier_step`] (Ctrl ⇒ ×0.1, Alt ⇒ ×10,
/// none ⇒ ×1). Pure so the brush sliders' modifier scaling is unit-tested
/// directly — the same selection rule `modifier_drag` applies to a DragValue.
fn slider_drag_speed(base_speed: f64, m: DragModifiers) -> f64 {
    base_speed * modifier_step(m)
}

/// The eyedropper (height pick) key. Bound to holding **E**, not Alt: window
/// managers commonly reserve Alt+click/drag to MOVE the window (Hyprland, GNOME,
/// Windows), so the app never sees an Alt+primary drag and the pick silently
/// fails. A plain letter key is WM-neutral and works the same on every platform.
const EYEDROPPER_KEY: egui::Key = egui::Key::E;

/// True while the eyedropper is engaged (the [`EYEDROPPER_KEY`] is held down).
fn eyedropper_active(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.key_down(EYEDROPPER_KEY))
}

/// An [`egui::Slider`] whose draggable value-box honors the F2 modifier scaling
/// (spec §2 / DoD §8 "every numeric slider"): while a modifier is held, the
/// value box's drag speed is `base_speed` scaled by [`modifier_step`] — Ctrl ⇒
/// fine (×0.1), Alt ⇒ coarse (×10), none ⇒ base. The slider TRACK is kept (so
/// the brush radius/strength stay one-drag controls), and dragging the value box
/// to its right gives the modifier-aware fine/coarse adjustment that the
/// DragValue controls already provide. `base_speed` is the per-point step the
/// value box uses at ×1.
fn modifier_slider<N: egui::emath::Numeric>(
    ui: &mut Ui,
    value: &mut N,
    range: std::ops::RangeInclusive<N>,
    base_speed: f64,
    text: &str,
    logarithmic: bool,
) -> egui::Response {
    let mods = ui.input(|i| i.modifiers);
    let speed = slider_drag_speed(base_speed, DragModifiers { ctrl: mods.ctrl, alt: mods.alt });
    ui.add(
        egui::Slider::new(value, range)
            .text(text)
            .logarithmic(logarithmic)
            .drag_value_speed(speed),
    )
}

/// The five MVP height tools, as a UI-dispatchable enum. Each maps to a [`Tool`]
/// impl in `tools.rs`; color/scatter tools (future MVPs) extend the same trait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SculptTool {
    Raise,
    Lower,
    Smooth,
    Flatten,
    Set,
    /// Stamp a parametric terrain primitive (cone/mesa/crater/ramp) on press —
    /// a single dab, not a continuous stroke. Params live in `SculptState::stamp`.
    Stamp,
    /// Paint the active palette swatch into the splat grid (color, not height).
    Paint,
    /// Freedraw omit/include zones — not a brush; it draws loops, not heights.
    Zone,
    /// Export-layer selection — not a brush; it picks grid boxes into layers.
    Layers,
}

/// How a zone loop is captured on the canvas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZoneStyle {
    /// Press-drag a freehand loop; auto-closed on release.
    Lasso,
    /// Click vertices; close on a click near the first vertex or a double-click.
    Polygon,
}

/// Top-level workspace mode (the tab bar). Groups the tools so the panel shows
/// only one mode's controls at a time instead of one long tool dropdown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SculptMode {
    /// Height brushes (Raise/Lower/Smooth/Flatten/Set).
    Shape,
    /// Parametric terrain primitives (cone/mesa/crater/ramp).
    Stamp,
    /// Splat color painting.
    Paint,
    /// Freedraw omit/include zones.
    Zone,
    /// Export layers: carve the map into parts that export separately and overlay.
    Layers,
}

/// The paint method inside Paint mode: freehand brush vs flood-fill bucket.
/// (The third method — auto palette from the height gradient — is a one-shot
/// button, not a click-mode.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaintTool {
    /// Freehand brush: drag to paint the active swatch.
    Brush,
    /// Paint bucket: click to flood-fill an elevation region/step.
    Bucket,
}

/// Which height field a click-to-arm map pick writes into. This is the EXPLICIT
/// pick (click a field's eyedropper → click the map → it fills that field),
/// distinct from the momentary hold-E eyedropper that always targets the active
/// Set/Flatten value. Keeping them separate is why nothing gets "mixed up": the
/// armed pick is one-shot, visibly flagged, and aimed at one named field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PickTarget {
    SetHeight,
    SeaLevel,
    FloorLevel,
}

impl PickTarget {
    /// The field's display name (also the on-canvas pick prompt).
    fn field_label(self) -> &'static str {
        match self {
            Self::SetHeight => "Set to",
            Self::SeaLevel => "Sea level",
            Self::FloorLevel => "Floor level",
        }
    }
}

impl SculptMode {
    const ALL: [SculptMode; 5] = [Self::Shape, Self::Stamp, Self::Paint, Self::Zone, Self::Layers];

    // DISPLAY names live in `help()` (each starts with the name, e.g. "Sculpt —
    // …"). The variant names `Shape`/`Zone` stay internal — `Zone` collides with
    // SculptTool::Zone, so a clean rename is risky; the UI says "Sculpt"/"Mask".

    /// Phosphor glyph for this mode's tab-bar button.
    fn icon(self) -> &'static str {
        use egui_phosphor::regular as ph;
        match self {
            Self::Shape => ph::MOUNTAINS,
            Self::Stamp => ph::STAMP,
            Self::Paint => ph::PAINT_BRUSH,
            Self::Zone => ph::SELECTION,
            Self::Layers => ph::STACK,
        }
    }

    /// Full tooltip: what this mode does, in one or two plain sentences.
    fn help(self) -> &'static str {
        match self {
            Self::Shape => {
                "Sculpt — freehand height brushes (Raise, Lower, Smooth, Flatten, Set). \
                 Drag on the map to reshape the land."
            }
            Self::Stamp => {
                "Stamp — drop a ready-made landform (cone, mesa, crater, ramp) in one click. \
                 Fast hills, plateaus, and pits without brushing."
            }
            Self::Paint => {
                "Paint — color the terrain with palette swatches (a splatmap). \
                 Affects brick color only, never height."
            }
            Self::Zone => {
                "Mask — draw regions that limit the export: Omit cuts holes, Include keeps only \
                 what's inside. Heights are untouched; it only decides which bricks are written."
            }
            Self::Layers => {
                "Layers — carve the map into parts that export as separate saves and snap back \
                 together in-game, so you can build worlds too big for one save."
            }
        }
    }
}

impl SculptTool {
    /// The five height brushes selectable inside Sculpt mode.
    const SHAPE_TOOLS: [SculptTool; 5] =
        [Self::Raise, Self::Lower, Self::Smooth, Self::Flatten, Self::Set];

    /// Which workspace mode this tool lives under (drives the active tab).
    fn mode(self) -> SculptMode {
        match self {
            Self::Raise | Self::Lower | Self::Smooth | Self::Flatten | Self::Set => SculptMode::Shape,
            Self::Stamp => SculptMode::Stamp,
            Self::Paint => SculptMode::Paint,
            Self::Zone => SculptMode::Zone,
            Self::Layers => SculptMode::Layers,
        }
    }

    /// Phosphor glyph paired with the label on tool buttons.
    fn icon(self) -> &'static str {
        use egui_phosphor::regular as ph;
        match self {
            Self::Raise => ph::ARROW_FAT_UP,
            Self::Lower => ph::ARROW_FAT_DOWN,
            Self::Smooth => ph::WAVE_SINE,
            Self::Flatten => ph::EQUALS,
            Self::Set => ph::RULER,
            Self::Stamp => ph::STAMP,
            Self::Paint => ph::PAINT_BRUSH,
            Self::Zone => ph::SELECTION,
            Self::Layers => ph::STACK,
        }
    }

    /// Full tooltip: purpose + strength + Brickadia effect.
    fn help(self) -> &'static str {
        match self {
            Self::Raise => {
                "Raise — adds real metres under the brush (strength = m per pass).\n\
                 In-game: taller brick stacks (height in flats; 1 brick ≈ 3 flats). \
                 Hold-drag to build hills."
            }
            Self::Lower => {
                "Lower — removes metres under the brush (strength = m per pass).\n\
                 In-game: shorter stacks; can open to native Brickadia floor if you dig to 0 \
                 with skip-floor export."
            }
            Self::Smooth => {
                "Smooth — blends neighbour heights (strength 0–1 blend per pass).\n\
                 In-game: fewer jagged steps; greedy mesher merges larger quads on soft slopes."
            }
            Self::Flatten => {
                "Flatten — eases toward “Set to” height (strength 0–1).\n\
                 In-game: plateaus / build pads at a chosen brick height. Set target first \
                 (E sample or eyedropper)."
            }
            Self::Set => {
                "Set height — hard-stamps “Set to” (no blend).\n\
                 In-game: flat shelves at exact flats/bricks. Hold E to sample, then click."
            }
            Self::Stamp => {
                "Stamp — one-click landform (cone/mesa/crater/ramp).\n\
                 In-game: same as sculpted height — becomes brick columns on export."
            }
            Self::Paint => {
                "Paint — splatmap color only (not material, not height).\n\
                 In-game: brick color; blank paint → default brick color."
            }
            Self::Zone => {
                "Mask — omit/include loops on export (heights unchanged).\n\
                 In-game: holes or cookie-cut pieces; orthogonal to omit-below water."
            }
            Self::Layers => {
                "Layers — box-select regions → separate .brdb/.brz that snap together.\n\
                 In-game: load each part (or prefabs) to beat single-save size limits."
            }
        }
    }

    /// Strength is meters-per-dab for Raise/Lower; a 0..=1 blend factor for the
    /// pull-toward-a-value tools (Smooth/Flatten/Set). Different default ranges
    /// keep one slider sensible across all five.
    const fn strength_is_blend(self) -> bool {
        matches!(self, Self::Smooth | Self::Flatten | Self::Set)
    }

    /// Apply one dab of this tool to `field` at `center`, shaped by `brush`,
    /// using `target` for the value-driven tools and `stamp` for the Stamp tool.
    /// Dispatches to the trait impls.
    fn apply_dab(
        self,
        field: &mut HeightField,
        brush: &Brush,
        center: (f32, f32),
        target: f32,
        stamp: StampParams,
    ) {
        match self {
            Self::Raise => Raise.apply(field, brush, center),
            Self::Lower => Lower.apply(field, brush, center),
            Self::Smooth => Smooth.apply(field, brush, center),
            Self::Flatten => Flatten { target }.apply(field, brush, center),
            Self::Set => SetHeight { target }.apply(field, brush, center),
            Self::Stamp => Stamp {
                kind: stamp.kind,
                peak_m: stamp.peak_m,
                inner_ratio: stamp.inner_ratio,
                angle: stamp.angle_deg.to_radians(),
            }
            .apply(field, brush, center),
            // Paint, Zone, and Layers are not height brushes; the canvas routes
            // them to their own handlers before any dab dispatch, so these arms are
            // never reached.
            Self::Paint | Self::Zone | Self::Layers => {}
        }
    }
}

/// Parameters for the [`SculptTool::Stamp`] primitive, held in sculpt state and
/// edited in the tool panel. `angle_deg` is stored in degrees for a friendlier
/// slider and converted to radians at dispatch.
#[derive(Clone, Copy, Debug, PartialEq)]
struct StampParams {
    kind: StampKind,
    peak_m: f32,
    inner_ratio: f32,
    angle_deg: f32,
}

impl Default for StampParams {
    fn default() -> Self {
        Self { kind: StampKind::Cone, peak_m: 40.0, inner_ratio: 0.4, angle_deg: 0.0 }
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

/// One reversible edit on the unified undo/redo timeline. Height strokes and
/// zone-mask edits interleave in a single history so undo walks back through
/// whatever the user did last, in order.
enum UndoEntry {
    /// A per-stroke cell snapshot (the existing height-edit path).
    Height(RectSnapshot),
    /// The zones list *before* a zone add/delete/clear. Restoring swaps it back
    /// in; touches no cells, so the terrain texture stays clean.
    Zones(Vec<Zone>),
    /// The whole paint grid *before* a paint stroke. Restoring swaps it back in
    /// and forces a texture re-render (the overlay changed).
    ///
    // ponytail: whole-grid snapshot per stroke (mirrors the Zones whole-Vec
    // clone). Ceiling: O(W·H) bytes/entry; if large-canvas paint undo gets heavy,
    // switch to a rect-bounded PaintSnapshot like RectSnapshot.
    Paint(PaintGrid),
}

/// All sculpt-tab state: the editable field, the active tool + brush, the view
/// transform, the cached terrain texture, undo/redo history, output options, and
/// the convert worker handle.
pub(crate) struct SculptState {
    field: Option<HeightField>,
    tool: SculptTool,
    /// The last height brush picked in Shape mode, restored when the Shape tab is
    /// re-entered after visiting Stamp/Paint/Zone.
    shape_tool: SculptTool,
    brush: Brush,
    /// Target height (meters above floor) for Flatten/Set.
    target_height: f32,
    /// Armed click-to-arm map pick: while `Some`, the next canvas click reads the
    /// hovered cell's height into this field and disarms (one-shot). `None` = off.
    /// Independent of the hold-E eyedropper (see [`PickTarget`]).
    armed_pick: Option<PickTarget>,
    /// Export layers (Layers mode): the box-grid selection stack. Reset on a field
    /// swap (cell-space, like zones/paint). MVP: box selections, single resolution.
    layers: layers::LayerStack,
    /// Last "Export All Parts" result line (parts written / error), shown in the
    /// Layers panel.
    layer_status: Option<String>,
    /// Parameters for the Stamp primitive tool (cone/mesa/crater/ramp).
    stamp: StampParams,
    /// Per-cell splat-paint palette indices, parallel to the height field (same
    /// dims; re-blanked on every field load). An unpainted grid colors bricks
    /// exactly as today (byte-identical). See [`super::paint`].
    paint: PaintGrid,
    /// Editable color palette; slot 0 is the unpainted color.
    palette: Vec<[u8; 4]>,
    /// The palette index the Paint tool writes.
    active_swatch: u8,
    /// Splat resolution: brush/gradient writes snap to `splat_res × splat_res`
    /// cell blocks (1 = per-cell, higher = coarser color = fewer brick splits).
    splat_res: u32,
    /// Paint method in Paint mode (freehand brush vs flood-fill bucket).
    paint_tool: PaintTool,
    /// Bucket flood-fill height tolerance (meters): cells within this of the
    /// clicked cell's height fill. Pairs naturally with the terrace step.
    bucket_tolerance_m: f32,
    /// Bucket fills every matching cell globally instead of only the contiguous
    /// region the click landed in.
    bucket_global: bool,

    // View transform: `pan` is the screen-space top-left of cell (0,0); `zoom`
    // is screen pixels per cell. Both are user-driven (drag-pan with the middle
    // button, scroll to zoom).
    pan: Vec2,
    zoom: f32,
    /// View rotation in radians (CCW). Free-angle: rotates the preview + the
    /// scan/slice overlay; baked into the export by a resample at convert time.
    view_rot: f32,
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

    // Bounded edit history (height strokes + zone edits interleaved). `undo`
    // holds pre-edit entries; `redo` holds the entries produced by undoing.
    undo: VecDeque<UndoEntry>,
    redo: VecDeque<UndoEntry>,
    /// Per-dab pre-edit snapshots of the in-progress stroke. Each entry captures
    /// only its own dab's rect (bounded `O(radius²)`) **before** that dab edits,
    /// so growth cost stays `O(new cells)` per dab instead of re-capturing the
    /// whole accumulated union every dab (the super-linear trap). They are
    /// collapsed into one union snapshot, earliest-pre-stroke-value-wins, by
    /// `commit_stroke`. Empty between strokes.
    active_stroke: Vec<RectSnapshot>,
    /// Last dab center (cells) during a drag, to space dabs along the path.
    last_dab: Option<(f32, f32)>,
    /// Freedraw omit/include zones in CELL space, applied as an XY keep-mask at
    /// convert (see [`crate::gui::zones`]). In memory only in Phase 1a; cleared
    /// when a new field loads (cell coords don't survive a field swap).
    zones: Vec<Zone>,
    /// Draw mode for new zones (Omit cuts a hole, Include keeps only its inside).
    zone_mode: ZoneMode,
    /// Capture style for new zones (freehand lasso vs click-polygon).
    zone_style: ZoneStyle,
    /// In-progress loop vertices (cell space): lasso accumulates along a drag;
    /// polygon accumulates clicked vertices until closed or cancelled. Empty when
    /// not mid-draw; never enters undo history until committed as a `Zone`.
    zone_draft: Vec<(f32, f32)>,

    // Blank-canvas + output controls.
    new_w: u32,
    new_h: u32,
    new_cell_m: f64,
    output_name: String,
    out: OutputOptions,
    /// Studs of brick width per real meter (the horizontal play-scale knob) the
    /// Export panel drives into the convert's [`FieldMeta`]. A blank canvas /
    /// image seeds it from here; a DEM/send-from-map seeds it from the source
    /// (then the panel can retune). Replaces the old hardcoded `blank_meta` 4.0.
    studs_per_meter: f32,
    /// Vertical exaggeration multiplier (1.0 = faithful 1:1 relief) the Export
    /// panel drives into the convert's [`FieldMeta`]. Replaces the old hardcoded
    /// `blank_meta` 1.0.
    vertical_exaggeration: f32,
    /// Micro-brick mode (fine detail) the Export panel drives into the convert's
    /// [`FieldMeta`]; mirrors `BlockType::Micro`. Replaces the hardcoded
    /// `blank_meta` `false`.
    micro: bool,
    /// Manual grid-tiled export toggle (spec §5). Default off = single mesh.
    /// On routes the convert through `convert_heightfield_tiled`, which
    /// subdivides the field into partition sub-fields stitched into one save.
    tile_export: bool,
    /// Sub-field edge length (cells) for a tiled export — drives the tile count
    /// and per-tile budget of `convert_heightfield_tiled`.
    tile_cells: u32,
    /// Base plane (meters above the field floor) terrain fills DOWN to. Maps to
    /// `convert_heightfield`'s `floor_level_m` → brick-Z `base_override`. Default
    /// `0.0` keeps today's floor (base plane at brick-Z 0). The Export panel
    /// binds a DragValue + eyedropper to this.
    floor_level_m: f32,
    /// Omit level (meters): a column whose source height (m) is at or below it
    /// emits no bricks — native floor / "omit water". Maps to
    /// `convert_heightfield`'s `omit_below_m`. Default `0.0` drops only true-floor
    /// columns (byte-identical to the prior skip).
    omit_below_m: f32,
    /// Terrace (stepped) mode: when on, heights snap to `terrace_step_m` multiples
    /// at render + export — discrete plateaus instead of smooth relief, accurate
    /// altitude preserved. Non-destructive (the stored field stays smooth), so it
    /// flips per project. Default off.
    terrace: bool,
    /// Vertical step size (meters) for terrace mode.
    terrace_step_m: f32,
    /// Max merged-brick footprint (world units) the greedy mesher may emit.
    /// Brickadia silently drops procedural bricks above its render limit, leaving
    /// holes — lower this until they vanish. Default 250 (50 studs).
    max_brick_units: u16,
    /// Show the greedy-mesher scan/slice direction overlay on the canvas.
    slice_overlay: bool,
    /// Manual tiling override (Advanced): force grid-tiling even for a world that
    /// fits a single mesh. Auto-tiling still engages on its own when over-cap.
    force_tile: bool,
    /// Cached available-RAM bytes for the Export estimate's button gate + GiB
    /// readout, refreshed on a coarse cadence (see [`refresh_available_ram`])
    /// instead of every frame — a per-frame `/proc/meminfo` scan is wasted sync
    /// file I/O when sub-second staleness can't change a gate the user only acts
    /// on at click time.
    available_ram: u64,
    /// egui frame time (seconds since start) of the last `available_ram` refresh,
    /// or `None` before the first one. Throttles the refresh to ~once a second.
    ram_refreshed_at: Option<f64>,

    // Convert worker (same Promise pattern as the Map tab).
    convert_promise: Option<Promise<Result<BuildOutcome, BuildError>>>,
    convert_progress: Arc<Mutex<(BuildStage, f32)>>,
    convert_cancel: Arc<AtomicBool>,
    last_outcome: Option<BuildOutcome>,
    last_error: Option<String>,
    /// Path of the most recently exported heightmap PNG, surfaced in the result
    /// line. Independent of the brick convert outcome.
    last_export: Option<PathBuf>,
}

impl Default for SculptState {
    fn default() -> Self {
        Self {
            field: None,
            tool: SculptTool::Raise,
            shape_tool: SculptTool::Raise,
            brush: Brush {
                shape: BrushShape::Circle,
                radius_cells: 12.0,
                strength: 5.0,
                falloff: Falloff::Smoothstep,
            },
            target_height: 20.0,
            armed_pick: None,
            layers: layers::LayerStack::new(0, 0),
            layer_status: None,
            stamp: StampParams::default(),
            // No field yet → a 0×0 grid; `set_field` re-blanks it to the field dims.
            paint: PaintGrid::blank(0, 0),
            palette: default_palette(),
            active_swatch: 1,
            splat_res: 1,
            paint_tool: PaintTool::Brush,
            bucket_tolerance_m: 5.0,
            bucket_global: false,
            pan: Vec2::ZERO,
            zoom: 4.0,
            view_rot: 0.0,
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
            zones: Vec::new(),
            zone_mode: ZoneMode::Omit,
            zone_style: ZoneStyle::Lasso,
            zone_draft: Vec::new(),
            new_w: DEFAULT_CANVAS_W,
            new_h: DEFAULT_CANVAS_H,
            new_cell_m: DEFAULT_CELL_M,
            output_name: "sculpt".to_string(),
            out: OutputOptions::default(),
            studs_per_meter: DEFAULT_STUDS_PER_METER,
            vertical_exaggeration: DEFAULT_VERTICAL_EXAGGERATION,
            micro: false,
            tile_export: false,
            tile_cells: DEFAULT_TILE_CELLS,
            floor_level_m: 0.0,
            omit_below_m: 0.0,
            terrace: false,
            terrace_step_m: 10.0,
            max_brick_units: 250,
            slice_overlay: false,
            force_tile: false,
            // Seed generous so a pre-first-refresh gate never blocks; the first
            // panel draw refreshes it from the live figure.
            available_ram: u64::MAX,
            ram_refreshed_at: None,
            convert_promise: None,
            convert_progress: Arc::new(Mutex::new((BuildStage::GeneratingBricks, 0.0))),
            convert_cancel: Arc::new(AtomicBool::new(false)),
            last_outcome: None,
            last_error: None,
            last_export: None,
        }
    }
}

impl SculptState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Load a field into the tab (from Send-to-Sculpt or a fresh canvas/image),
    /// resetting view + history. Marks dirty so the texture rebuilds.
    ///
    /// Seeds the Export-panel scale knobs from the incoming field's metadata so a
    /// DEM / send-from-map field primes the panel with the source's studs/m ·
    /// exaggeration · micro (the user can then retune); a blank canvas / image
    /// carries the panel's own current values back in (round-trip stable).
    pub(crate) fn set_field(&mut self, field: HeightField) {
        self.studs_per_meter = field.meta.studs_per_meter;
        self.vertical_exaggeration = field.meta.vertical_exaggeration;
        self.micro = field.meta.micro;
        // The paint grid is cell-aligned to the field; a new field of different
        // dims would misindex it, so re-blank to the incoming dims (like zones +
        // undo, which also drop on a field swap).
        self.paint = PaintGrid::blank(field.width, field.height);
        // Export layers are cell-space; a new field of different dims would
        // misindex them, so reset to one empty Base layer (like zones/paint).
        self.layers = layers::LayerStack::new(field.width, field.height);
        self.layer_status = None;
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
        // Zones are cell-space; a new field of different dimensions would
        // misregister them, so a field load drops them (like undo history).
        self.zones.clear();
        self.zone_draft.clear();
    }

    /// Snapshot the current zones onto the undo stack before a zone mutation,
    /// clearing redo (same discipline as a height stroke commit). Bounded by the
    /// shared `UNDO_CAP`.
    fn record_zone_edit(&mut self) {
        self.undo.push_back(UndoEntry::Zones(self.zones.clone()));
        while self.undo.len() > UNDO_CAP {
            self.undo.pop_front();
        }
        self.redo.clear();
    }

    /// Snapshot the whole paint grid onto the undo stack before a paint stroke,
    /// clearing redo (same discipline as a height/zone commit). Bounded by the
    /// shared `UNDO_CAP`.
    fn record_paint_edit(&mut self) {
        self.undo.push_back(UndoEntry::Paint(self.paint.clone()));
        while self.undo.len() > UNDO_CAP {
            self.undo.pop_front();
        }
        self.redo.clear();
    }

    /// Append a freedraw zone (undoable).
    fn add_zone(&mut self, zone: Zone) {
        self.record_zone_edit();
        self.zones.push(zone);
    }

    /// Delete the zone at `idx` (undoable); no-op if out of range.
    fn delete_zone(&mut self, idx: usize) {
        if idx < self.zones.len() {
            self.record_zone_edit();
            self.zones.remove(idx);
        }
    }

    /// Drop every zone (undoable); no-op if already empty (so it never pushes an
    /// empty no-op entry onto the history).
    fn clear_zones(&mut self) {
        if !self.zones.is_empty() {
            self.record_zone_edit();
            self.zones.clear();
        }
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

    /// Hold-E eyedropper: sample the hovered cell's height (meters above floor)
    /// into the active Set/Flatten target. No-op without a field. Pure (no egui),
    /// so the sample → target path is unit-testable directly.
    fn sample_height_into_target(&mut self, cx: f32, cy: f32) {
        if let Some(field) = self.field.as_ref() {
            self.target_height = field.sample_cell_meters(cx, cy);
        }
    }

    /// Click-to-arm pick: read the hovered cell's height (meters above floor) into
    /// the named height field. Routes the same sampled value the hold-E eyedropper
    /// uses, but to the specific field the user armed. No-op without a field. Pure.
    fn sample_into(&mut self, target: PickTarget, cx: f32, cy: f32) {
        let Some(field) = self.field.as_ref() else { return };
        let h = field.sample_cell_meters(cx, cy);
        match target {
            PickTarget::SetHeight => self.target_height = h,
            PickTarget::SeaLevel => self.omit_below_m = h,
            PickTarget::FloorLevel => self.floor_level_m = h,
        }
    }

    /// Push the panel's live scale knobs into the field's `FieldMeta` so every
    /// studs-from-meta display (World width, brush studs) stays current frame-to-
    /// frame. `start_convert` performs the identical sync, so this is display-only.
    fn sync_scale_to_field_meta(&mut self) {
        let (spm, vexag, micro) = (self.studs_per_meter, self.vertical_exaggeration, self.micro);
        if let Some(f) = self.field.as_mut() {
            f.meta.studs_per_meter = spm;
            f.meta.vertical_exaggeration = vexag;
            f.meta.micro = micro;
        }
    }
}

/// Blank-canvas / image metadata seeded from the Export-panel scale state. The
/// pitch comes from the New-canvas control; `studs_per_meter` /
/// `vertical_exaggeration` / `micro` come from the panel (no longer hardcoded
/// 4.0 / 1.0 / false) so a blank canvas reaches true 1:1 and any relief the user
/// dials. `source_name` is overwritten at convert time from the output-name box.
fn blank_meta(state: &SculptState, cell_m: f64) -> FieldMeta {
    FieldMeta {
        cell_m,
        studs_per_meter: state.studs_per_meter,
        vertical_exaggeration: state.vertical_exaggeration,
        micro: state.micro,
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

    // Keep the field's scale metadata in lockstep with the live panel knobs each
    // frame, so studs-sourced displays (World width, brush studs, "1 brick = N
    // studs") reflect edits IMMEDIATELY instead of reading the pre-edit scale and
    // snapping back (meta was previously synced to state only at convert). The
    // convert re-applies the same sync, so this changes display only, not export.
    state.sync_scale_to_field_meta();

    draw_mode_bar(state, ui);
    // View controls: free-angle rotation of the preview (and the scan overlay),
    // plus the slice-direction toggle. Rotating re-orients the terrain relative to
    // the brick slice axis — align a long ridge with it for longer, fewer bricks.
    ui.horizontal(|ui| {
        ui.label("Rotate terrain");
        let mut deg = state.view_rot.to_degrees();
        if ui
            .add(egui::DragValue::new(&mut deg).suffix("°").speed(1.0).range(-180.0..=180.0))
            .on_hover_text(
                "Spin the depthmap against the FIXED screen-down slice direction. Line a ridge up \
                 with the slice lanes, then export — the rotation bakes in (height, paint, and \
                 zones together) so it slices into long bricks. Resampling softens detail off-axis.",
            )
            .changed()
        {
            state.view_rot = deg.to_radians();
        }
        if ui.button(format!("{} Reset", egui_phosphor::regular::ARROW_COUNTER_CLOCKWISE)).clicked() {
            state.view_rot = 0.0;
        }
        ui.checkbox(&mut state.slice_overlay, "Slice dir").on_hover_text(
            "Show the fixed screen-down slice direction (cyan = slice, amber = widen). The terrain \
             rotates under it — align ridges to the lanes for longer, fewer bricks.",
        );
    });
    ui.add_space(8.0);
    // Each mode shows only its own controls (no catch-all dropdown).
    match state.tool.mode() {
        SculptMode::Shape => {
            draw_shape_tools(state, ui);
            draw_brush_section(state, ui);
        }
        SculptMode::Stamp => {
            draw_brush_section(state, ui);
            ui.add_space(6.0);
            draw_stamp_section(state, ui);
        }
        SculptMode::Paint => {
            draw_brush_section(state, ui);
            ui.add_space(6.0);
            draw_paint_section(state, ui);
        }
        SculptMode::Zone => draw_zone_section(state, ui),
        SculptMode::Layers => draw_layers_section(state, ui),
    }
    ui.add_space(8.0);
    draw_history_section(state, ui);
    ui.add_space(8.0);
    let estimate = draw_export_section(state, ui);
    ui.add_space(8.0);
    draw_convert_section(state, ui, estimate);
    ui.add_space(6.0);
    draw_last_result(state, ui);
}

fn draw_new_canvas_section(state: &mut SculptState, ui: &mut Ui) {
    ui.collapsing("New / load", |ui| {
        ui.horizontal(|ui| {
            ui.label("Width");
            modifier_drag(ui, &mut state.new_w, 1.0, 2..=4096);
            ui.label("Height");
            modifier_drag(ui, &mut state.new_h, 1.0, 2..=4096);
        });
        ui.horizontal(|ui| {
            ui.label("Cell size (m)");
            modifier_drag(ui, &mut state.new_cell_m, 0.1, 0.1..=1000.0);
        });
        // No creation-time cell cap: a blank canvas is meant to grow into a big
        // detailed world, which the Export panel handles by tiling (the live
        // estimate's MAX_BRICKS / MAX_GRID_BRICKS + RAM gate remedies an oversized
        // export via "Tile this export"). The DragValue 2..=4096 range above is the
        // only sane bound; the per-tile/per-mesh `enforce_cell_budget` still guards
        // each tile at Convert.
        if ui.button("New blank canvas").clicked() {
            let meta = blank_meta(state, state.new_cell_m);
            state.set_field(HeightField::flat(state.new_w, state.new_h, meta));
        }
        if ui.button("Load heightmap image…").clicked() {
            load_heightmap_image(state);
        }
        ui.small(
            "Large canvases are fine — the export auto-splits a too-big world into \
             stitched tiles for you.",
        );
    });
}

/// The top mode tab bar: Shape / Stamp / Paint / Zone. Switching modes sets the
/// active tool to that mode's tool (Shape restores the last height brush) and
/// drops any in-progress zone draft when leaving Zone.
/// A GIMP/Photoshop-style icon-only tool button: a fixed square with the phosphor
/// glyph centered (22px), a hover tooltip (no text label), and the theme's
/// high-contrast selected state. Returns the click Response.
fn icon_button(ui: &mut Ui, glyph: &str, selected: bool, tooltip: &str) -> egui::Response {
    let txt = egui::RichText::new(glyph).size(22.0);
    let resp = ui.add_sized([34.0, 32.0], egui::Button::selectable(selected, txt));
    // Fade-in hover glow: an ember outline that eases in over ~0.16 s while the
    // button is hovered (and eases back out), so the cursor's target lights up
    // smoothly instead of snapping. Selected buttons keep their fill; the glow
    // layers on top. `animate_bool_with_time` drives the 0→1 alpha per button id.
    let t = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hover_glow"), resp.hovered(), 0.16);
    if t > 0.0 {
        let glow = crate::gui::theme::ACCENT.gamma_multiply(0.65 * t);
        ui.painter().rect_stroke(
            resp.rect.expand(1.0),
            egui::CornerRadius::same(4),
            egui::Stroke::new(1.5, glow),
            egui::StrokeKind::Outside,
        );
    }
    resp.on_hover_text(tooltip)
}

/// A group section header — a slightly larger, strong title in the ember accent,
/// with a little space above. Gives the panel real visual hierarchy instead of a
/// flat wall of same-weight labels.
fn section_header(ui: &mut Ui, title: &str) {
    ui.add_space(6.0);
    ui.label(
        egui::RichText::new(title)
            .size(14.0)
            .strong()
            .color(crate::gui::theme::ACCENT),
    );
}

fn brush_shape_icon(s: BrushShape) -> &'static str {
    use egui_phosphor::regular as ph;
    match s {
        BrushShape::Circle => ph::CIRCLE,
        BrushShape::Square => ph::SQUARE,
        BrushShape::Diamond => ph::DIAMOND,
        BrushShape::Hexagon => ph::HEXAGON,
    }
}
fn stamp_kind_icon(k: StampKind) -> &'static str {
    use egui_phosphor::regular as ph;
    match k {
        StampKind::Cone => ph::TRIANGLE,
        StampKind::Mesa => ph::SQUARE,
        StampKind::Crater => ph::BOWL_FOOD,
        StampKind::Ramp => ph::STAIRS,
    }
}

fn draw_mode_bar(state: &mut SculptState, ui: &mut Ui) {
    let current = state.tool.mode();
    ui.horizontal(|ui| {
        for m in SculptMode::ALL {
            let btn = icon_button(ui, m.icon(), current == m, m.help());
            if btn.clicked() && current != m {
                if m != SculptMode::Zone {
                    state.zone_draft.clear();
                }
                state.tool = match m {
                    SculptMode::Shape => state.shape_tool,
                    SculptMode::Stamp => SculptTool::Stamp,
                    SculptMode::Paint => SculptTool::Paint,
                    SculptMode::Zone => SculptTool::Zone,
                    SculptMode::Layers => SculptTool::Layers,
                };
            }
        }
    });
}

/// Shape mode's height-brush sub-selector (Raise/Lower/Smooth/Flatten/Set) plus
/// the target-height field the value-driven brushes use.
fn draw_shape_tools(state: &mut SculptState, ui: &mut Ui) {
    ui.horizontal_wrapped(|ui| {
        for t in SculptTool::SHAPE_TOOLS {
            if icon_button(ui, t.icon(), state.tool == t, t.help()).clicked() {
                state.tool = t;
                state.shape_tool = t; // remember for the next Shape-tab entry
            }
        }
    });
    if matches!(state.tool, SculptTool::Flatten | SculptTool::Set) {
        let vscale = state.field.as_ref().map_or(1.0, |f| vertical_units_per_meter(&f.meta));
        height_drag_pickable(
            ui, "Set to", &mut state.target_height, vscale,
            PickTarget::SetHeight, &mut state.armed_pick,
        )
        .on_hover_text(
            "Target height in Brickadia bricks + flats (1 brick = 3 flats). Hold E and click the \
             terrain to sample, or click the eyedropper chip to pick straight into this box.",
        );
    }
    ui.small(format!(
        "{}  Hold E + click the terrain to sample a height into Set/Flatten",
        egui_phosphor::regular::EYEDROPPER
    ))
    .on_hover_text(
        "Hold the E key and click/drag the canvas to pick up that spot's height into the \
         Set-height value; release E to sculpt with it. (E, not Alt — Alt+drag moves the \
         window on many systems.)",
    );
}

/// Studs of brick footprint spanned by one field cell at the current scale.
/// A single unmerged cell becomes one brick whose footprint is `2*hscale*upf`
/// world units (see [`crate::gui::map_tab::derive_scale`]); 1 stud = 5 units, so
/// a cell — and thus the smallest representable brick — is `2*hscale*upf/5` studs
/// wide. Uses the *achieved* integer `hscale`, so it reflects what actually
/// exports (low scales snap up to the 1-brick minimum), not the pre-rounding ask.
fn studs_per_cell(meta: &FieldMeta) -> f32 {
    let upf = if meta.micro { 1.0_f32 } else { 5.0 };
    let (hscale, _) = crate::gui::map_tab::derive_scale(
        meta.cell_m,
        meta.studs_per_meter,
        meta.vertical_exaggeration,
        meta.micro,
    );
    2.0 * f32::from(hscale) * upf / 5.0
}

// --- Brickadia vertical parity: heights in BRICKS + FLATS (plates) ---
//
// Vertical UI units: single source of truth in `crate::brick_units` (BWT-F5).
use crate::brick_units::{
    flats_to_meters, fmt_bricks_flats, meters_to_flats, parse_bricks_flats,
};

/// Heightmap units per meter at the current scale (the vertical leg of `derive_scale`).
fn vertical_units_per_meter(meta: &FieldMeta) -> f32 {
    build_derive_scale(meta.cell_m, meta.studs_per_meter, meta.vertical_exaggeration, meta.micro).1
}
/// A height control edited in Brickadia bricks+flats, stored in meters. `vscale`
/// is [`vertical_units_per_meter`] for the live field. `signed` allows negative
/// heights (a Stamp peak that digs); unsigned clamps at the floor (0).
fn height_drag_signed(
    ui: &mut Ui,
    label: &str,
    meters: &mut f32,
    vscale: f32,
    signed: bool,
) -> egui::Response {
    let mut flats = meters_to_flats(*meters, vscale);
    let min = if signed { -1_000_000.0 } else { 0.0 };
    let resp = ui
        .horizontal(|ui| {
            ui.label(label);
            ui.add(
                egui::DragValue::new(&mut flats)
                    .speed(1.0)
                    .range(min..=1_000_000.0)
                    .custom_formatter(|p, _| fmt_bricks_flats(p as f32))
                    .custom_parser(parse_bricks_flats),
            )
        })
        .inner;
    if resp.changed() {
        *meters = flats_to_meters(flats, vscale);
    }
    resp
}

/// Unsigned bricks+flats height control (floor-clamped) — the common case.
fn height_drag(ui: &mut Ui, label: &str, meters: &mut f32, vscale: f32) -> egui::Response {
    height_drag_signed(ui, label, meters, vscale, false)
}

/// A bricks+flats height field with an integrated eyedropper toggle: the label is
/// a clickable [eyedropper + name] chip that ARMS a one-shot map pick into this
/// field (click it, then click the terrain). Clicking again — or the canvas
/// handler after a pick, or Esc — disarms. Returns the DragValue response so the
/// caller can chain `.on_hover_text`/`.changed`. The chip lights up (selected
/// look) while THIS field is the armed target, so it's unmistakable which box the
/// next map click fills.
fn height_drag_pickable(
    ui: &mut Ui,
    label: &str,
    meters: &mut f32,
    vscale: f32,
    target: PickTarget,
    armed: &mut Option<PickTarget>,
) -> egui::Response {
    let is_armed = *armed == Some(target);
    ui.horizontal(|ui| {
        let chip = ui
            .add(egui::Button::selectable(
                is_armed,
                egui::RichText::new(format!("{}  {label}", egui_phosphor::regular::EYEDROPPER)),
            ))
            .on_hover_text(format!(
                "Pick “{label}” from the map — click here, then click the terrain to read that \
                 spot's height into this box (one-shot; Esc cancels). Like holding E, but aimed \
                 at this field instead of Set/Flatten."
            ));
        if chip.clicked() {
            *armed = if is_armed { None } else { Some(target) };
        }
        let mut flats = meters_to_flats(*meters, vscale);
        let dv = ui.add(
            egui::DragValue::new(&mut flats)
                .speed(1.0)
                .range(0.0..=1_000_000.0)
                .custom_formatter(|p, _| fmt_bricks_flats(p as f32))
                .custom_parser(parse_bricks_flats),
        );
        if dv.changed() {
            *meters = flats_to_meters(flats, vscale);
        }
        dv
    })
    .inner
}

fn draw_brush_section(state: &mut SculptState, ui: &mut Ui) {
    section_header(ui, "Brush");
    // Brush size shown + edited in Brickadia STUDS (the export unit), while the
    // stored value stays radius-in-cells (so the overlay ring + dab spacing are
    // unchanged). The slider position is still cells; the formatter/parser convert
    // diameter_studs = 2·cells·studs_per_cell. Drag keeps the Ctrl/Alt fine/coarse.
    let spc = state.field.as_ref().map_or(1.0, |f| studs_per_cell(&f.meta));
    let mods = ui.input(|i| i.modifiers);
    let speed = slider_drag_speed(0.2, DragModifiers { ctrl: mods.ctrl, alt: mods.alt });
    ui.add(
        egui::Slider::new(&mut state.brush.radius_cells, 1.0..=200.0)
            .text("brush size")
            .logarithmic(true)
            .drag_value_speed(speed)
            .custom_formatter(move |n, _| format!("Ø {:.0} studs", n * 2.0 * f64::from(spc)))
            .custom_parser(move |s| {
                s.trim()
                    .trim_end_matches("studs")
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .map(|studs| studs / (2.0 * f64::from(spc)))
            }),
    );
    ui.small(format!("1 brick = {spc:.1} studs")).on_hover_text(
        "Sizes are in Brickadia studs. One field cell is the smallest brick \
         (1 brick = the value shown); keep features at least ~1 brick wide so they \
         survive on export.",
    );
    // Stamp drives height via its own `peak_m`; Paint hard-writes an index with no
    // falloff — neither uses the brush strength, so hide it for both.
    if !matches!(state.tool, SculptTool::Stamp | SculptTool::Paint) {
        let strength_range = if state.tool.strength_is_blend() {
            0.0..=1.0
        } else {
            0.0..=200.0
        };
        // Blend tools live in 0..=1 (fine base step); meter tools in 0..=200.
        let strength_base = if state.tool.strength_is_blend() { 0.005 } else { 0.2 };
        modifier_slider(ui, &mut state.brush.strength, strength_range, strength_base, "strength", false);
    }
    ui.horizontal(|ui| {
        ui.label("Falloff");
        ui.selectable_value(&mut state.brush.falloff, Falloff::Smoothstep, "Smooth");
        ui.selectable_value(&mut state.brush.falloff, Falloff::Linear, "Linear");
        ui.selectable_value(&mut state.brush.falloff, Falloff::Constant, "Hard");
    });
    ui.horizontal(|ui| {
        ui.label("Tip");
        for s in BrushShape::ALL {
            if icon_button(ui, brush_shape_icon(s), state.brush.shape == s, s.help()).clicked() {
                state.brush.shape = s;
            }
        }
    });
}

fn draw_stamp_section(state: &mut SculptState, ui: &mut Ui) {
    section_header(ui, "Primitive");
    ui.horizontal(|ui| {
        ui.label("Form");
        for k in StampKind::ALL {
            if icon_button(ui, stamp_kind_icon(k), state.stamp.kind == k, k.help()).clicked() {
                state.stamp.kind = k;
            }
        }
    });
    // Peak height in Brickadia bricks+flats (signed: a negative peak digs a
    // crater pit / inverted cone, clamped to floor at apply time).
    let vscale = state.field.as_ref().map_or(1.0, |f| vertical_units_per_meter(&f.meta));
    height_drag_signed(ui, "Peak", &mut state.stamp.peak_m, vscale, true)
        .on_hover_text("Stamp height in bricks + flats. Negative digs a pit.");
    // Mesa plateau width and crater rim position both ride `inner_ratio`; Cone is
    // radially uniform and Ramp is directional, so the knob only matters for the
    // first two — show it for all but label its role.
    if matches!(state.stamp.kind, StampKind::Mesa | StampKind::Crater) {
        modifier_slider(ui, &mut state.stamp.inner_ratio, 0.05..=0.95, 0.01, "inner ratio", false);
    }
    if state.stamp.kind == StampKind::Ramp {
        modifier_slider(ui, &mut state.stamp.angle_deg, 0.0..=360.0, 1.0, "angle (°)", false);
    }
}

fn draw_paint_section(state: &mut SculptState, ui: &mut Ui) {
    // Paint method: freehand brush, flood-fill bucket, or auto palette (the
    // gradient button below). Brush/Bucket pick how a click behaves on the canvas.
    ui.horizontal(|ui| {
        ui.label("Method");
        use egui_phosphor::regular as ph;
        if icon_button(ui, ph::PAINT_BRUSH, state.paint_tool == PaintTool::Brush,
            "Brush — drag to paint the active swatch onto cells.").clicked() {
            state.paint_tool = PaintTool::Brush;
        }
        if icon_button(ui, ph::PAINT_BUCKET, state.paint_tool == PaintTool::Bucket,
            "Bucket — click a cell to flood-fill every connected cell within the height Tolerance. \
             Match Tolerance to the terrace step to fill a whole plateau.").clicked() {
            state.paint_tool = PaintTool::Bucket;
        }
    });
    if state.paint_tool == PaintTool::Bucket {
        let vscale = state.field.as_ref().map_or(1.0, |f| vertical_units_per_meter(&f.meta));
        height_drag(ui, "Tolerance", &mut state.bucket_tolerance_m, vscale).on_hover_text(
            "Click a cell: fills cells within this height (bricks + flats) of it. Match \
             the terrace step to flood a whole plateau.",
        );
        ui.checkbox(&mut state.bucket_global, "Global (all matching cells)")
            .on_hover_text("Off: only the contiguous region you click. On: every matching cell.");
    }
    ui.separator();

    section_header(ui, "Palette");
    // Swatch grid: click to select the active swatch the brush writes.
    ui.horizontal_wrapped(|ui| {
        for i in 0..state.palette.len() {
            let col = state.palette[i];
            let (rect, resp) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::click());
            ui.painter().rect_filled(
                rect,
                3.0,
                Color32::from_rgba_unmultiplied(col[0], col[1], col[2], col[3]),
            );
            if i == state.active_swatch as usize {
                ui.painter().rect_stroke(
                    rect,
                    3.0,
                    Stroke::new(2.0, Color32::from_rgb(0xFF, 0xE0, 0x60)),
                    StrokeKind::Inside,
                );
            }
            if resp.clicked() {
                state.active_swatch = i as u8;
            }
            let hint = if i == 0 { "unpainted (default brick color)".into() } else { format!("swatch {i}") };
            resp.on_hover_text(hint);
        }
    });
    // Edit the active swatch's color; recoloring updates the overlay.
    let i = state.active_swatch as usize;
    if i < state.palette.len()
        && ui.horizontal(|ui| {
            ui.label(format!("Edit swatch {i}"));
            ui.color_edit_button_srgba_unmultiplied(&mut state.palette[i]).changed()
        }).inner
    {
        state.mark_dirty_all();
    }
    ui.horizontal(|ui| {
        if state.palette.len() < MAX_SWATCHES && ui.button("+ swatch").clicked() {
            state.palette.push([0xC0, 0xC0, 0xC0, 0xFF]);
            state.active_swatch = (state.palette.len() - 1) as u8;
        }
        if ui.button("Load splatmap…").clicked() {
            load_splatmap(state);
        }
    });
    ui.label("Splatmap: RGBA channels → layers 1–4 (dominant channel per pixel).");

    ui.separator();
    draw_gradient_section(state, ui);
    ui.separator();
    draw_splat_resolution_section(state, ui);
}

/// Height-gradient splat: preview the hypsometric ramp and one-click fill the
/// whole splat grid from the terrain's heatmap (so the painted colors match the
/// canvas). Sets the palette to the gradient bands and fills every cell by band.
fn draw_gradient_section(state: &mut SculptState, ui: &mut Ui) {
    section_header(ui, "Height gradient");
    // Preview the ramp as a continuous strip (the same hypsometric the canvas uses).
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width().min(240.0), 16.0), Sense::hover());
    let n = 64u32;
    let seg = rect.width() / n as f32;
    for i in 0..n {
        let t = i as f32 / (n - 1) as f32;
        let c = hypsometric(t);
        let x = rect.left() + i as f32 * seg;
        let band = Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(seg + 1.0, rect.height()));
        ui.painter().rect_filled(band, 0.0, Color32::from_rgb(c[0], c[1], c[2]));
    }
    if ui
        .button("Fill splat from height gradient")
        .on_hover_text("Color every cell by its elevation using the heatmap ramp (8 bands)")
        .clicked()
    {
        fill_splat_from_gradient(state);
    }
}

/// Splat resolution selector with a live grid preview: writes snap to
/// `res × res` cell blocks, so a coarser splat means blockier color and far
/// fewer brick color-splits. The preview draws the resulting splat-cell grid.
fn draw_splat_resolution_section(state: &mut SculptState, ui: &mut Ui) {
    section_header(ui, "Splat resolution");
    ui.horizontal(|ui| {
        for r in [1u32, 2, 4, 8] {
            let label = if r == 1 { "1× (per cell)".to_string() } else { format!("1∕{r}") };
            ui.selectable_value(&mut state.splat_res, r, label);
        }
    });
    let r = state.splat_res.max(1);
    if let Some(field) = state.field.as_ref() {
        let (cols, rows) = (field.width.div_ceil(r), field.height.div_ceil(r));
        ui.small(format!("≈ {cols} × {rows} splat cells ({r} terrain-cell block)"));
        draw_splat_grid_preview(ui, cols, rows);
    }
}

/// A small grid icon showing the splat-cell layout: line spacing reflects the
/// chosen resolution, so a coarse splat draws few big cells and a fine one draws
/// many. Division count is capped so a huge field stays legible.
fn draw_splat_grid_preview(ui: &mut Ui, cols: u32, rows: u32) {
    let side = 72.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(side), Sense::hover());
    let stroke = Stroke::new(1.0, Color32::from_gray(110));
    ui.painter().rect_filled(rect, 2.0, Color32::from_gray(30));
    ui.painter().rect_stroke(rect, 2.0, stroke, StrokeKind::Inside);
    // Cap drawn divisions so the preview is readable regardless of field size.
    let cx = cols.clamp(1, 16);
    let cy = rows.clamp(1, 16);
    for i in 1..cx {
        let x = rect.left() + rect.width() * i as f32 / cx as f32;
        ui.painter().line_segment([egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())], stroke);
    }
    for j in 1..cy {
        let y = rect.top() + rect.height() * j as f32 / cy as f32;
        ui.painter().line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)], stroke);
    }
}

/// Set the palette to the hypsometric gradient bands and fill every cell by its
/// elevation band (snapped to the splat resolution). Undoable; re-renders.
fn fill_splat_from_gradient(state: &mut SculptState) {
    let Some(field) = state.field.as_ref() else { return };
    let (min, max) = field.min_max();
    state.record_paint_edit();
    state.palette = gradient_palette(hypsometric);
    state.active_swatch = state.active_swatch.min(GRADIENT_BANDS as u8);
    let r = state.splat_res.max(1);
    // Borrow the field immutably to read heights while filling the grid.
    let field = state.field.as_ref().expect("field present");
    state.paint.fill_from_gradient(r, min, max, |x, y| field.at(x, y));
    state.mark_dirty_all();
}

fn draw_zone_section(state: &mut SculptState, ui: &mut Ui) {
    section_header(ui, "Mask");
    ui.horizontal(|ui| {
        ui.label("Mode");
        use egui_phosphor::regular as ph;
        if icon_button(ui, ph::PROHIBIT, state.zone_mode == ZoneMode::Omit,
            "Omit — draw a loop; terrain inside it is cut from the export (a hole or removed region).").clicked() {
            state.zone_mode = ZoneMode::Omit;
        }
        if icon_button(ui, ph::CHECK_CIRCLE, state.zone_mode == ZoneMode::Include,
            "Include — draw loops; only terrain inside them is exported, everything else is dropped.").clicked() {
            state.zone_mode = ZoneMode::Include;
        }
    });
    ui.horizontal(|ui| {
        ui.label("Style");
        // Switching capture style mid-draw drops the in-progress draft so a
        // half-placed polygon can't bleed into a lasso (or vice-versa).
        if ui.selectable_value(&mut state.zone_style, ZoneStyle::Lasso, "Lasso").changed() {
            state.zone_draft.clear();
        }
        if ui.selectable_value(&mut state.zone_style, ZoneStyle::Polygon, "Polygon").changed() {
            state.zone_draft.clear();
        }
    });
    ui.small(match state.zone_style {
        ZoneStyle::Lasso => "Drag a loop on the canvas; release to close it.",
        ZoneStyle::Polygon => {
            "Click to place vertices; click the first dot (or double-click) to \
             close. Esc cancels."
        }
    });

    ui.add_space(4.0);
    if state.zones.is_empty() {
        ui.small("No zones yet — omit cuts holes, include keeps only its inside.");
        return;
    }

    // Render the list, capturing a delete request to apply after the borrow ends.
    let mut delete: Option<usize> = None;
    for (i, z) in state.zones.iter().enumerate() {
        ui.horizontal(|ui| {
            let (icon, name) = match z.mode {
                ZoneMode::Omit => (egui_phosphor::regular::PROHIBIT, "Omit"),
                ZoneMode::Include => (egui_phosphor::regular::CHECK_CIRCLE, "Include"),
            };
            ui.label(format!("{icon} {name} · {} pts", z.polygon.len()));
            if ui.small_button("✖").on_hover_text("Delete region").clicked() {
                delete = Some(i);
            }
        });
    }
    if let Some(i) = delete {
        state.delete_zone(i);
    }
    if ui.button("Clear all zones").clicked() {
        state.clear_zones();
    }
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

/// A cheap, PURE single-mesh export estimate (spec §1, "Live estimate"). The
/// greedy mesher emits at most one brick per non-floor column, so `est_bricks`
/// is the conservative cell-count ceiling; peak RAM reuses the grid pipeline's
/// own calibration (`est_tile_mesh_bytes` + `BRICK_OWNED_BYTES` ×
/// `WRITE_PEAK_FACTOR` for the single combined write). `fits_ram` holds the
/// working set under `available - RAM_RESERVE_BYTES`; `over_brick_cap` flags a
/// field whose ceiling exceeds the per-mesh [`build::MAX_BRICKS`]. This
/// single-mesh figure is what the panel gates on when tiling is off;
/// [`tiled_estimate`] (which subdivides the field) governs the tiled path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExportEstimate {
    est_bricks: u64,
    peak_bytes: u64,
    over_brick_cap: bool,
    fits_ram: bool,
    /// Tiles the export will produce: `1` for a single mesh, or
    /// `ceil(w/tile)·ceil(h/tile)` for the grid-tiled path. Surfaced in the
    /// readout so the user sees the subdivision a tiled export will perform.
    tile_count: u32,
}

/// Compute the single-mesh estimate for a `cells`-cell field. `available_ram` is
/// injected (the live `/proc/meminfo` read happens at the call site) so this stays
/// a pure, testable function. Mirrors `estimate_grid`'s memory model for ONE tile:
/// the mesh peak plus the write-peak brick vec, with no resident multi-tile
/// raster set (a single mesh holds just its own raster, already counted in the
/// mesh-bytes model).
fn single_mesh_estimate(cells: u64, available_ram: u64) -> ExportEstimate {
    use crate::gui::grid::{
        BRICK_OWNED_BYTES, RAM_RESERVE_BYTES, WRITE_PEAK_FACTOR, est_tile_mesh_bytes,
    };
    let est_bricks = cells; // greedy mesh ceiling: ≤ 1 brick/non-floor column
    let mesh_bytes = est_tile_mesh_bytes(cells, false); // sculpt uses a flat colormap
    let write_vec_bytes = est_bricks
        .saturating_mul(BRICK_OWNED_BYTES)
        .saturating_mul(WRITE_PEAK_FACTOR);
    let peak_bytes = mesh_bytes.saturating_add(write_vec_bytes);
    let over_brick_cap = est_bricks > build::MAX_BRICKS as u64;
    let budget = available_ram.saturating_sub(RAM_RESERVE_BYTES);
    let fits_ram = peak_bytes <= budget && !over_brick_cap;
    ExportEstimate { est_bricks, peak_bytes, over_brick_cap, fits_ram, tile_count: 1 }
}

/// Tiles along one axis for a `tile_cells`-cell grid-tiled export: partition
/// sub-fields step by `tile_cells` cells, so `ceil(extent / tile_cells)` tiles
/// (always ≥ 1). Mirrors `convert::tile_bounds`'s count without allocating —
/// the single source of truth for the readout's tile figure.
fn tiles_on_axis(extent: u32, tile_cells: u32) -> u32 {
    extent.max(1).div_ceil(tile_cells.max(1))
}

/// Compute the grid-tiled estimate (spec §1 + §5): a `w × h` field split into
/// `ceil(w/tile)·ceil(h/tile)` partition sub-fields stitched into ONE save.
/// Reuses `estimate_grid`'s memory model — the peak working set is ONE tile's
/// mesh (meshing is sequential) plus the stitched write-vec over the aggregate
/// brick count — and the aggregate cap is `MAX_GRID_BRICKS` (NOT the per-mesh
/// `MAX_BRICKS`, since exceeding it is the whole point of tiling). `available_ram`
/// is injected so this stays pure and testable.
fn tiled_estimate(width: u32, height: u32, tile_cells: u32, available_ram: u64) -> ExportEstimate {
    use crate::gui::grid::{
        BRICK_OWNED_BYTES, RAM_RESERVE_BYTES, WRITE_PEAK_FACTOR, est_tile_mesh_bytes,
    };
    let cols = tiles_on_axis(width, tile_cells);
    let rows = tiles_on_axis(height, tile_cells);
    let tile_count = cols.saturating_mul(rows);

    // Aggregate brick ceiling: ≤ 1 brick / field cell. Tiles partition the field
    // (no shared/duplicated seam cell), so a full tile is exactly `tile_cells`
    // wide; the last tile may be narrower, making this a slight over-estimate —
    // the safe direction for a RAM/cap budget.
    let body = u64::from(tile_cells.max(1)); // partition tile width on an axis
    let per_tile_cells = body.saturating_mul(body);
    let est_bricks = per_tile_cells.saturating_mul(u64::from(tile_count));

    // Peak RAM: one tile's mesh (sequential meshing) + the stitched write-vec
    // over the aggregate brick count (bricks_to_save runs once at the end).
    let mesh_bytes = est_tile_mesh_bytes(per_tile_cells, false);
    let write_vec_bytes = est_bricks
        .saturating_mul(BRICK_OWNED_BYTES)
        .saturating_mul(WRITE_PEAK_FACTOR);
    let peak_bytes = mesh_bytes.saturating_add(write_vec_bytes);

    // Tiling's cap is the AGGREGATE MAX_GRID_BRICKS, not the per-mesh MAX_BRICKS.
    let over_brick_cap = est_bricks > build::MAX_GRID_BRICKS as u64;
    let budget = available_ram.saturating_sub(RAM_RESERVE_BYTES);
    let fits_ram = peak_bytes <= budget && !over_brick_cap;
    ExportEstimate { est_bricks, peak_bytes, over_brick_cap, fits_ram, tile_count }
}

/// Refresh `state.available_ram` from the live `/proc/meminfo` figure on a coarse
/// cadence: on the first call (no prior refresh) and then throttled to at most
/// once per second, using egui's frame clock (`ui.input(|i| i.time)`, no
/// wall-clock `Date::now`). Sub-second staleness can't change the button gate the
/// user only acts on at click time, so this replaces the per-frame sync file read.
/// A read failure falls back to a generous figure so the gate never blocks on it.
fn refresh_available_ram(state: &mut SculptState, ui: &Ui) {
    const REFRESH_INTERVAL_S: f64 = 1.0;
    let now = ui.input(|i| i.time);
    let stale = state.ram_refreshed_at.is_none_or(|last| now - last >= REFRESH_INTERVAL_S);
    if stale {
        state.available_ram = crate::gui::grid::available_ram_bytes().unwrap_or(u64::MAX);
        state.ram_refreshed_at = Some(now);
    }
}

/// The collapsing "Export" section (spec §1): scale knobs that drive `FieldMeta`,
/// a micro toggle, formats + install + overwrite, floor/omit levels, the manual
/// tiling toggle + size, and a live brick/RAM estimate. Returns the estimate so
/// the convert section can gate its button on it.
fn draw_export_section(state: &mut SculptState, ui: &mut Ui) -> ExportEstimate {
    // Use the EXPORT dims for the estimate + auto-tile gate: a non-zero view
    // rotation bakes a larger rotated bounding box at convert, so estimating on the
    // unrotated grid would under-count bricks/RAM and skip auto-tiling. Mirror
    // start_convert's rotated_dims. Audit 2026-06-30 (rotated-dims estimate).
    let dims = state.field.as_ref().map_or((0, 0), |f| {
        if state.view_rot != 0.0 {
            super::heightfield::rotated_dims(f.width, f.height, state.view_rot)
        } else {
            (f.width, f.height)
        }
    });
    let cells = u64::from(dims.0) * u64::from(dims.1);
    // RAM is read from a coarse-cadence cache (see refresh_available_ram), not the
    // raw /proc/meminfo scan every frame — it only gates the button + a GiB
    // readout, so sub-second staleness is fine.
    refresh_available_ram(state, ui);
    let available_ram = state.available_ram;
    // The estimate (and the button gate) follows the SAME path the convert will
    // take: the grid math when tiling (tile count + per-tile peak + aggregate
    // cap), the cheap single-mesh figure otherwise.
    // Auto-tiling: a single mesh that would trip the in-game brick cap or RAM is
    // split into stitched tiles AUTOMATICALLY — the user never toggles it (the old
    // "Tiling" knob nobody understood). `tile_cells` (sub-tile size) stays tunable
    // in Advanced; a notice in the panel says when a split happens.
    let single = single_mesh_estimate(cells, available_ram);
    // !single.fits_ram already subsumes over_brick_cap (fits_ram = within RAM AND
    // !over_brick_cap), so the auto trigger is simply "single mesh doesn't fit";
    // OR the Advanced manual override.
    state.tile_export = !single.fits_ram || state.force_tile;
    let estimate = if state.tile_export {
        tiled_estimate(dims.0, dims.1, state.tile_cells, available_ram)
    } else {
        single
    };

    egui::CollapsingHeader::new("Export").default_open(true).show(ui, |ui| {
        // Vertical scale (heightmap units/m) for the bricks+flats height controls,
        // from the live (frame-synced) field meta.
        let vscale = state.field.as_ref().map_or(1.0, |f| vertical_units_per_meter(&f.meta));

        // ---- Surface: how the terrain builds (flat — no dropdowns) ----
        section_header(ui, "Surface");
        // "Fill flat ground" is the inverse of skip_floor (off → native floor shows
        // through flat areas; on → a base plate under everything).
        let mut fill_flat = !state.out.skip_floor;
        if ui
            .checkbox(&mut fill_flat, "Fill flat ground")
            .on_hover_text(
                "Off (default): flat areas emit no bricks, so the native Brickadia floor shows \
                 through. On: a watertight base plate is built under everything.",
            )
            .changed()
        {
            state.out.skip_floor = !fill_flat;
        }
        height_drag_pickable(
            ui, "Sea level", &mut state.omit_below_m, vscale,
            PickTarget::SeaLevel, &mut state.armed_pick,
        )
        .on_hover_text(
            "Terrain at or below this height (bricks + flats) emits no bricks — drop water / \
             low ground. 0 drops only the true floor. Use the eyedropper chip to pick a \
             shoreline height off the map.",
        );
        let terr_toggle = ui
            .checkbox(&mut state.terrace, "Stepped relief")
            .on_hover_text(
                "Snap heights to discrete plateaus (stepped look) in the preview AND export. \
                 Off = smooth. The stored field is untouched, so you can flip per project.",
            )
            .changed();
        let mut step_changed = false;
        if state.terrace {
            step_changed = height_drag(ui, "Step height", &mut state.terrace_step_m, vscale)
                .on_hover_text("Plateau step in bricks + flats — terrain snaps to multiples of this.")
                .changed();
            // A terrace step must stay positive (it divides the snap); clamp to at
            // least one flat so a drag-to-zero can't make a degenerate step.
            let one_flat = flats_to_meters(1.0, vscale);
            state.terrace_step_m = state.terrace_step_m.max(one_flat.max(f32::MIN_POSITIVE));
        }
        if terr_toggle || step_changed {
            state.mark_dirty_all();
        }

        ui.separator();
        // ---- Build size (in studs — the unit you build in) ----
        section_header(ui, "Build size");
        // World width is typed in STUDS; it back-solves studs/meter (kept internal).
        // world_studs = field.width · studs_per_cell ≈ width · studs/m · cell_m, so
        // studs/m = target_width / (width · cell_m). Read field data as owned values
        // first so the studs/meter write below isn't a borrow conflict.
        if let Some((fw, fh, cell_m, spc)) = state.field.as_ref().map(|f| {
            (f.width as f32, f.height as f32, f.meta.cell_m as f32, studs_per_cell(&f.meta))
        }) {
            let mut world_w = fw * spc;
            ui.horizontal(|ui| {
                ui.label("World width");
                if ui
                    .add(
                        egui::DragValue::new(&mut world_w)
                            .suffix(" studs")
                            .speed(f64::from(spc).max(1.0))
                            .range(2.0..=200_000.0),
                    )
                    .on_hover_text("How wide the build is, in studs. Sets the scale; height follows.")
                    .changed()
                {
                    state.studs_per_meter = (world_w / (fw * cell_m)).clamp(0.01, 64.0);
                }
            });
            ui.small(format!("World: {:.0} × {:.0} studs", fw * spc, fh * spc));
        }
        ui.horizontal(|ui| {
            ui.label("Terrain height");
            modifier_drag(ui, &mut state.vertical_exaggeration, 0.1, 0.1..=64.0);
        });
        ui.checkbox(&mut state.micro, "Fine detail (micro bricks)").on_hover_text(
            "On: ~5× finer bricks at the same physical scale, for crisp relief detail. \
             Off: standard bricks.",
        );

        ui.separator();
        // ---- Output name + formats ----
        section_header(ui, "Output");
        ui.add(
            TextEdit::singleline(&mut state.output_name)
                .hint_text("sculpt")
                .desired_width(260.0),
        );
        ui.checkbox(&mut state.out.brdb, "World (.brdb → Worlds/)");
        ui.checkbox(&mut state.out.brz, "Prefab (.brz → Prefabs/)");
        ui.checkbox(&mut state.out.install_to_brickadia, "Install into Brickadia");
        ui.checkbox(&mut state.out.overwrite, "Overwrite existing");

        ui.separator();
        // ---- Bricks (flat, plain studs) ----
        ui.horizontal(|ui| {
            ui.label("Max brick size");
            ui.add(
                egui::DragValue::new(&mut state.max_brick_units)
                    .range(40..=12_000)
                    .speed(5.0)
                    .custom_formatter(|n, _| format!("{:.0} studs", n / 5.0))
                    .custom_parser(|s| {
                        s.trim()
                            .trim_end_matches("studs")
                            .trim()
                            .parse::<f64>()
                            .ok()
                            .map(|studs| studs * 5.0)
                    }),
            );
        })
        .response
        .on_hover_text(
            "Largest brick the export emits, in studs. Lower this until in-game holes in flat \
             areas vanish (50 studs is a safe start).",
        );

        ui.separator();
        // ---- Live estimate + auto-tiling notice ----
        draw_estimate_readout(state, ui, estimate);
        if state.tile_export {
            // Informational (not a warning): auto-tiling is intended behaviour.
            ui.small(format!(
                "Big world — auto-split into {} stitched tiles on export.",
                estimate.tile_count
            ));
        }

        // ---- Advanced: the ONLY collapsible left ----
        ui.collapsing("Advanced", |ui| {
            height_drag_pickable(
                ui, "Floor level", &mut state.floor_level_m, vscale,
                PickTarget::FloorLevel, &mut state.armed_pick,
            )
            .on_hover_text(
                "Base-plane height the build sits on (bricks + flats). Usually 0. Click the \
                 eyedropper chip, then a spot on the map, to seat the floor at that height.",
            );
            ui.checkbox(&mut state.force_tile, "Always tile (force split)").on_hover_text(
                "Force grid-tiling even for a world that fits a single mesh. A too-big world \
                 auto-tiles on its own regardless.",
            );
            ui.horizontal(|ui| {
                ui.label("Tile size (studs)");
                // Tile size stored in cells; shown in studs (cell ≈ studs_per_cell).
                let spc = state.field.as_ref().map_or(1.0, |f| studs_per_cell(&f.meta));
                let mut tile_studs = state.tile_cells as f32 * spc;
                if ui
                    .add(egui::DragValue::new(&mut tile_studs).suffix(" studs").speed(spc.max(1.0)))
                    .changed()
                {
                    state.tile_cells = ((tile_studs / spc.max(0.001)).round() as u32).clamp(16, 4096);
                }
            })
            .response
            .on_hover_text("Sub-tile size used when a big world splits.");
        });
    });

    estimate
}

/// `derive_scale` lives on the map tab; thin re-export shim so the scale helpers
/// read the SAME integer-brick scale the convert derives (single source of truth).
fn build_derive_scale(cell_m: f64, studs_per_meter: f32, exaggeration: f32, micro: bool) -> (u16, f32) {
    crate::gui::map_tab::derive_scale(cell_m, studs_per_meter, exaggeration, micro)
}

/// Live brick / peak-RAM estimate readout + over-budget remedy (spec §1). Green
/// when it fits; a warning + remedy when the single mesh would exceed the
/// per-mesh brick cap or available RAM (the Export button is gated on the same
/// `estimate.fits_ram`).
fn draw_estimate_readout(state: &SculptState, ui: &mut Ui, est: ExportEstimate) {
    if state.field.is_none() {
        return;
    }
    let ram_gib = est.peak_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if state.tile_export {
        ui.small(format!(
            "≈ {} bricks · {} tiles · peak ~{ram_gib:.1} GiB (stitched)",
            est.est_bricks, est.tile_count,
        ));
    } else {
        ui.small(format!("≈ {} bricks · peak ~{ram_gib:.1} GiB", est.est_bricks));
    }
    if est.over_brick_cap {
        // Auto-tiling already engages whenever a single mesh would trip the cap, so
        // over_brick_cap here means even the STITCHED grid exceeds MAX_GRID_BRICKS —
        // tiling can't grow past the stitch ceiling, so the remedy is a smaller
        // canvas/scale. (The old "enable Tile this export" branch is gone — tiling
        // is automatic now.)
        ui.colored_label(
            STATUS_WARN_FG,
            format!(
                "Over the {}-brick cap — shrink the canvas or lower the scale.",
                build::MAX_GRID_BRICKS
            ),
        );
    } else if !est.fits_ram {
        ui.colored_label(
            STATUS_WARN_FG,
            "Estimated peak RAM exceeds what's free — shrink the canvas or lower the scale.",
        );
    }
}

fn draw_convert_section(state: &mut SculptState, ui: &mut Ui, estimate: ExportEstimate) {
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
    // Gate the Export button on the live estimate too: a single mesh that blows
    // the brick cap or the RAM budget must not be startable (spec §1).
    let over_budget = !estimate.fits_ram;
    let enabled = !name_empty && !no_format && !over_budget;
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
    if over_budget {
        ui.colored_label(STATUS_WARN_FG, "• Over the brick/RAM budget (see the Export estimate)");
    }
    if enabled && resp.clicked() {
        start_convert(state);
    }

    // Export the editable terrain itself as a heightmap PNG — independent of the
    // brick convert above (no output-format/budget gate; it writes one image).
    // Round-trips through the CLI/map pipeline's rgba-encoded `HeightmapPNG`.
    let export = ui.add(
        egui::Button::new("🖼  Export heightmap PNG").min_size(Vec2::new(260.0, 28.0)),
    );
    if export.clicked() {
        export_heightmap_png(state);
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
        // Show the filename only (the full path wraps into an ugly multi-line blob);
        // the complete path is on hover.
        let (verb, path) = match &outcome.installed_path {
            Some(dest) => ("installed", dest),
            None => ("wrote", &outcome.brdb_path),
        };
        let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
        ui.small(format!("{verb} → {name}")).on_hover_text(path.display().to_string());
        if let Some(warn) = &outcome.install_warning {
            ui.colored_label(STATUS_ERROR_FG, format!("⚠ {warn}"));
        }
    }
    if let Some(path) = &state.last_export {
        ui.colored_label(STATUS_WARN_FG, "✔ Heightmap PNG exported");
        ui.small(format!("wrote → {}", path.display()));
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
    handle_view_input(state, &response, ui);

    // Paint the terrain texture at the current pan/zoom.
    paint_terrain(state, ui, rect, fw, fh);

    // Brick slice direction — FIXED screen reference (down). The terrain rotates
    // under it; aligning a ridge to the cyan lanes makes it slice into long bricks.
    if state.slice_overlay {
        paint_slice_overlay(ui, rect);
    }

    if state.tool == SculptTool::Zone {
        // Zone tool: the primary button draws loops (no dabs, no brush ring).
        handle_zone_input(state, &response, rect);
    } else if state.tool == SculptTool::Layers {
        // Layers tool: a primary click toggles the grid box under the pointer into
        // the active layer (no dabs, no brush ring). The overlay draws the grid +
        // each layer's owned boxes in its color.
        handle_layers_input(state, &response, fw, fh);
        paint_layers_overlay(state, ui, &response, rect);
    } else {
        // Apply sculpt dabs from a primary-button drag.
        handle_sculpt_input(state, &response, fw, fh);

        // Brush overlay + smooth radius animation. Animating every frame while the
        // pointer is over the canvas keeps the resize buttery regardless of grid
        // size (it touches no texture). Shown always; the hold-E eyedropper readout
        // (below) layers its own cursor hint over it.
        paint_brush_overlay(state, ctx, ui, &response, rect);
    }

    // Zone overlay: committed zones are always shown (so they're visible while
    // sculpting too); the in-progress draft only while the Zone tool is active.
    paint_zone_overlay(state, ui, &response, rect);

    // Eyedropper on-canvas readout: the last sampled height (spec §3), drawn in
    // the corner so the pick value is visible without leaving the canvas.
    paint_pick_readout(state, ui, &response, rect);
}

/// Capture freedraw zones on the canvas (spec §"Capture UX"). Lasso records a
/// decimated drag; polygon collects clicked vertices. Esc cancels an in-progress
/// polygon before it is committed (so it never enters undo history).
/// Layers-mode canvas input: a primary click toggles the grid box under the
/// pointer into/out of the ACTIVE layer (a fully-owned box clears, otherwise it's
/// added). Box ranges come from `grid_div`; the pointer maps to a cell via
/// `screen_to_cell` (so it's correct at any zoom/pan/rotation).
fn handle_layers_input(state: &mut SculptState, response: &egui::Response, fw: u32, fh: u32) {
    if !response.clicked_by(egui::PointerButton::Primary) {
        return;
    }
    let Some(ptr) = response.interact_pointer_pos() else { return };
    let (cxf, cyf) = screen_to_cell(state, ptr);
    if cxf < 0.0 || cyf < 0.0 {
        return;
    }
    let (cx, cy) = (cxf as u32, cyf as u32);
    if cx >= fw || cy >= fh {
        return;
    }
    let (cols, rows) = (state.layers.grid_div.0.max(1), state.layers.grid_div.1.max(1));
    let bi = (cx * cols / fw).min(cols - 1);
    let bj = (cy * rows / fh).min(rows - 1);
    let on = !state.layers.box_full_in_active(fw, fh, bi, bj);
    state.layers.paint_box(fw, fh, bi, bj, on);
}

/// Draw the box-grid overlay for Layers mode: grid lines + each visible layer's
/// owned boxes tinted in its color (the active layer brighter). All corners go
/// through `cell_to_screen`, so the overlay tracks zoom/pan/view-rotation exactly.
fn paint_layers_overlay(state: &SculptState, ui: &Ui, _response: &egui::Response, rect: Rect) {
    let Some(field) = state.field.as_ref() else { return };
    let (fw, fh) = (field.width, field.height);
    let (cols, rows) = (state.layers.grid_div.0.max(1), state.layers.grid_div.1.max(1));
    let painter = ui.painter_at(rect);

    // Owned-box tints (bottom layer first so the active/top color reads on top).
    for (li, layer) in state.layers.layers.iter().enumerate() {
        if !layer.visible {
            continue;
        }
        let [r, g, b, _] = layer.color;
        let alpha = if li == state.layers.active { 0x55 } else { 0x26 };
        let fill = Color32::from_rgba_unmultiplied(r, g, b, alpha);
        for bj in 0..rows {
            for bi in 0..cols {
                let (x0, y0, x1, y1) = state.layers.box_cell_range(fw, fh, bi, bj);
                if x0 >= x1 || y0 >= y1 {
                    continue;
                }
                // MVP picks whole boxes, so the NW cell's bit == the whole box.
                if !layer.box_mask.get((y0 * fw + x0) as usize).copied().unwrap_or(false) {
                    continue;
                }
                let quad = vec![
                    cell_to_screen(state, x0 as f32, y0 as f32),
                    cell_to_screen(state, x1 as f32, y0 as f32),
                    cell_to_screen(state, x1 as f32, y1 as f32),
                    cell_to_screen(state, x0 as f32, y1 as f32),
                ];
                painter.add(egui::Shape::convex_polygon(quad, fill, Stroke::NONE));
            }
        }
    }

    // Grid lines on top.
    let grid_stroke = Stroke::new(1.0, crate::gui::theme::ACCENT.gamma_multiply(0.35));
    for i in 0..=cols {
        let x = (i * fw / cols) as f32;
        painter.line_segment(
            [cell_to_screen(state, x, 0.0), cell_to_screen(state, x, fh as f32)],
            grid_stroke,
        );
    }
    for j in 0..=rows {
        let y = (j * fh / rows) as f32;
        painter.line_segment(
            [cell_to_screen(state, 0.0, y), cell_to_screen(state, fw as f32, y)],
            grid_stroke,
        );
    }
}

/// Run "Export All Parts" synchronously (MVP): mesh + write one pre-positioned
/// save per visible, non-empty layer, then report the count. Synchronous is fine
/// for the MVP; a worker thread (like `start_convert`) is a follow-up for very
/// large mosaics. View-rotation is NOT baked here yet (Phase 2) — layers export
/// at the stored 0° orientation.
fn export_all_parts(state: &mut SculptState) {
    let Some(field) = state.field.clone() else { return };
    let mut field = field;
    field.meta.source_name = state.output_name.clone();
    field.meta.studs_per_meter = state.studs_per_meter;
    field.meta.vertical_exaggeration = state.vertical_exaggeration;
    field.meta.micro = state.micro;

    let progress: build::ProgressFn = Arc::new(|_, _| {});
    let cancel = Arc::new(AtomicBool::new(false));
    match export_layer_parts(
        &state.layers,
        &field,
        state.out,
        state.floor_level_m,
        state.omit_below_m,
        state.max_brick_units,
        progress,
        cancel,
    ) {
        Ok(parts) => {
            let total: usize = parts.iter().map(|p| p.brick_count).sum();
            let installed = parts.iter().filter(|p| p.installed_path.is_some()).count();
            let warns = parts.iter().filter(|p| p.install_warning.is_some()).count();
            let mut msg = format!(
                "✔ {} part(s) · {total} bricks · {installed} installed",
                parts.len(),
            );
            // Show where the saves landed (the staging dir) + a sample part name.
            if let Some(p) = parts.first() {
                if let Some(dir) = p.primary_path.parent() {
                    msg.push_str(&format!(" → {}", dir.display()));
                }
                msg.push_str(&format!("  (e.g. “{}”)", p.layer_name));
            }
            if warns > 0 {
                msg.push_str(&format!(" · {warns} install warning(s)"));
            }
            state.layer_status = Some(msg);
            state.last_error = None;
        }
        Err(e) => state.layer_status = Some(format!("✗ {e}")),
    }
}

/// Layers panel (Layers mode): the layer list (+ add), the box grid divisions, and
/// "Export All Parts". MVP: box selection + single resolution. Color/eye/recolor
/// (Phase 3), lasso-in-layer (Phase 2), and per-layer resolution (Phase 1) extend
/// this panel without changing the export contract.
fn draw_layers_section(state: &mut SculptState, ui: &mut Ui) {
    section_header(ui, "Layers");
    let active = state.layers.active;
    let mut select: Option<usize> = None;
    for (i, layer) in state.layers.layers.iter().enumerate() {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
            let [r, g, b, _] = layer.color;
            ui.painter().rect_filled(rect, egui::CornerRadius::same(3), Color32::from_rgb(r, g, b));
            if ui.selectable_label(i == active, &layer.name).clicked() {
                select = Some(i);
            }
        });
    }
    if let Some(i) = select {
        state.layers.active = i;
    }
    if ui.button("+ Add layer").clicked()
        && let Some(f) = state.field.as_ref()
    {
        let (fw, fh) = (f.width, f.height);
        state.layers.add_layer(fw, fh);
    }

    ui.separator();
    section_header(ui, "Grid");
    ui.horizontal(|ui| {
        ui.label("Columns");
        let mut c = state.layers.grid_div.0;
        if ui.add(egui::DragValue::new(&mut c).range(1..=64)).changed() {
            state.layers.grid_div.0 = c;
        }
        ui.label("Rows");
        let mut r = state.layers.grid_div.1;
        if ui.add(egui::DragValue::new(&mut r).range(1..=64)).changed() {
            state.layers.grid_div.1 = r;
        }
    });
    ui.small("Click boxes on the map to add them to the selected layer.");

    ui.separator();
    let has_field = state.field.is_some();
    if ui
        .add_enabled(
            has_field,
            egui::Button::new("⬛  Export All Parts").min_size(Vec2::new(220.0, 28.0)),
        )
        .on_hover_text(
            "Export each layer as its own save, written at true world coordinates — load them all \
             in Brickadia and they snap together into one world.",
        )
        .clicked()
    {
        export_all_parts(state);
    }
    if let Some(msg) = &state.layer_status {
        ui.colored_label(STATUS_WARN_FG, msg);
    }
}

fn handle_zone_input(state: &mut SculptState, response: &egui::Response, _rect: Rect) {
    match state.zone_style {
        ZoneStyle::Lasso => handle_zone_lasso(state, response),
        ZoneStyle::Polygon => handle_zone_polygon(state, response),
    }
}

/// Lasso: a primary press-drag records points, decimated to `>= ~0.75` cell
/// spacing; release auto-closes into a `Zone` (discarded if < 3 points survive).
fn handle_zone_lasso(state: &mut SculptState, response: &egui::Response) {
    if response.drag_started_by(egui::PointerButton::Primary) {
        state.zone_draft.clear();
        if let Some(ptr) = response.interact_pointer_pos() {
            state.zone_draft.push(screen_to_cell(state, ptr));
        }
    } else if response.dragged_by(egui::PointerButton::Primary)
        && let Some(ptr) = response.interact_pointer_pos()
    {
        let pt = screen_to_cell(state, ptr);
        let far = state.zone_draft.last().is_none_or(|&(lx, ly)| {
            let (dx, dy) = (pt.0 - lx, pt.1 - ly);
            (dx * dx + dy * dy).sqrt() >= ZONE_LASSO_MIN_CELL_DIST
        });
        if far {
            state.zone_draft.push(pt);
        }
    }

    if response.drag_stopped_by(egui::PointerButton::Primary) {
        commit_zone_draft(state);
    }
}

/// Polygon: each primary click appends a vertex; a click within
/// `ZONE_POLY_CLOSE_PX` of the first vertex (with >= 3 placed) or a double-click
/// closes the loop. The double-click is handled before the append so the pairing
/// click doesn't drop a stray vertex on the closing frame.
fn handle_zone_polygon(state: &mut SculptState, response: &egui::Response) {
    // Esc cancels an in-progress polygon before it commits (never enters undo).
    // Scoped to Polygon: in Lasso the same-frame drag would immediately re-fill it.
    if response.ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        state.zone_draft.clear();
        return;
    }
    if response.double_clicked_by(egui::PointerButton::Primary) {
        commit_zone_draft(state);
        return;
    }
    if response.clicked_by(egui::PointerButton::Primary)
        && let Some(ptr) = response.interact_pointer_pos()
    {
        if state.zone_draft.len() >= 3 {
            let (fx, fy) = state.zone_draft[0];
            let first_screen = cell_to_screen(state, fx, fy);
            if (ptr - first_screen).length() <= ZONE_POLY_CLOSE_PX {
                commit_zone_draft(state);
                return;
            }
        }
        state.zone_draft.push(screen_to_cell(state, ptr));
    }
}

/// Close the in-progress draft into a committed (undoable) zone, or discard it if
/// it has fewer than 3 vertices (a degenerate loop encloses nothing).
fn commit_zone_draft(state: &mut SculptState) {
    let polygon = std::mem::take(&mut state.zone_draft);
    if polygon.len() >= 3 {
        let mode = state.zone_mode;
        state.add_zone(Zone { mode, polygon });
    }
}

/// Translucent fill + solid outline colors for a zone mode (omit red, include
/// green).
fn zone_colors(mode: ZoneMode) -> (Color32, Color32) {
    match mode {
        ZoneMode::Omit => (
            Color32::from_rgba_unmultiplied(0xFF, 0x40, 0x40, 38),
            Color32::from_rgb(0xFF, 0x50, 0x50),
        ),
        ZoneMode::Include => (
            Color32::from_rgba_unmultiplied(0x40, 0xFF, 0x60, 38),
            Color32::from_rgb(0x50, 0xFF, 0x70),
        ),
    }
}

/// Draw the closed outline of a screen-space polygon (last → first edge
/// included), correct for concave loops (unlike a convex-hull stroke).
fn paint_closed_outline(painter: &egui::Painter, pts: &[Pos2], color: Color32) {
    let n = pts.len();
    for i in 0..n {
        painter.line_segment([pts[i], pts[(i + 1) % n]], Stroke::new(2.0, color));
    }
}

/// True if the closed polygon is convex (all turns the same sign). egui's
/// `convex_polygon` fan-fills concavities — which would tint a notch the OPPOSITE
/// of what the PNPOLY mask actually keeps — so the fill is only safe to draw when
/// this holds. A degenerate (<3) loop is treated as non-convex (no fill).
fn is_convex(pts: &[Pos2]) -> bool {
    let n = pts.len();
    if n < 3 {
        return false;
    }
    let mut sign = 0i32;
    for i in 0..n {
        let a = pts[i];
        let b = pts[(i + 1) % n];
        let c = pts[(i + 2) % n];
        let cross = (b.x - a.x) * (c.y - b.y) - (b.y - a.y) * (c.x - b.x);
        if cross.abs() > f32::EPSILON {
            let s = if cross > 0.0 { 1 } else { -1 };
            if sign == 0 {
                sign = s;
            } else if s != sign {
                return false;
            }
        }
    }
    true
}

/// Render committed zones (filled + outlined) and, while the Zone tool is active,
/// the in-progress draft (placed vertices + rubber-band to the cursor).
fn paint_zone_overlay(state: &SculptState, ui: &Ui, response: &egui::Response, rect: Rect) {
    let painter = ui.painter_at(rect);

    for z in &state.zones {
        if z.polygon.len() < 3 {
            continue;
        }
        let pts: Vec<Pos2> = z.polygon.iter().map(|&(cx, cy)| cell_to_screen(state, cx, cy)).collect();
        let (fill, line) = zone_colors(z.mode);
        // Fill only convex loops — egui's fan-fill would tint a concave notch the
        // INVERSE of the real mask. The TRUE closed outline is always drawn and is
        // exact for any loop, so concave zones are still clearly shown.
        if is_convex(&pts) {
            painter.add(egui::Shape::convex_polygon(pts.clone(), fill, Stroke::NONE));
        }
        paint_closed_outline(&painter, &pts, line);
    }

    if state.tool == SculptTool::Zone && !state.zone_draft.is_empty() {
        let (_, line) = zone_colors(state.zone_mode);
        let pts: Vec<Pos2> =
            state.zone_draft.iter().map(|&(cx, cy)| cell_to_screen(state, cx, cy)).collect();
        for w in pts.windows(2) {
            painter.line_segment([w[0], w[1]], Stroke::new(2.0, line));
        }
        for p in &pts {
            painter.circle_filled(*p, 3.0, line);
        }
        // Rubber-band from the last placed vertex to the cursor.
        if let (Some(&last), Some(ptr)) = (pts.last(), response.hover_pos()) {
            painter.line_segment([last, ptr], Stroke::new(1.0, line));
        }
    }
}

/// Draw the eyedropper's on-canvas readout (spec §3): while pick mode is active,
/// a small label in the canvas's top-left shows the hovered cell's height (in
/// bricks + flats) while E is held, so the eyedropper value is visible without
/// looking at the side panel. No-op unless the eyedropper key is held.
fn paint_pick_readout(state: &SculptState, ui: &Ui, response: &egui::Response, rect: Rect) {
    let armed = state.armed_pick;
    // Active when the hold-E eyedropper is down OR a field pick is armed.
    if armed.is_none() && !eyedropper_active(ui.ctx()) {
        return;
    }
    let Some(field) = state.field.as_ref() else { return };
    // Where the sampled value lands: the armed field, else Set (hold-E target).
    let dest = armed.map_or("Set", PickTarget::field_label);
    let tail = if armed.is_some() { "Esc cancels" } else { "release E to sculpt" };
    let mut text = format!("click samples height → {dest}");
    if response.hovered()
        && let Some(ptr) = response.hover_pos()
    {
        let (cx, cy) = screen_to_cell(state, ptr);
        let vscale = vertical_units_per_meter(&field.meta);
        let flats = meters_to_flats(field.sample_cell_meters(cx, cy), vscale);
        text = format!("sample {} → {dest}  ·  {tail}", fmt_bricks_flats(flats));
    }
    let painter = ui.painter_at(rect);
    // Offset below the slice-overlay legend so the two don't overlap.
    let pos = rect.min + Vec2::new(8.0, 28.0);
    painter.text(
        pos,
        egui::Align2::LEFT_TOP,
        text,
        egui::FontId::monospace(13.0),
        Color32::from_rgb(0xFF, 0xE0, 0x60),
    );
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
fn handle_view_input(state: &mut SculptState, response: &egui::Response, ui: &Ui) {
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
            // Keep the cell under the pointer stationary across the zoom change,
            // routed through the SAME rotation-aware transform the tools use (no
            // hand-rolled affine that would drift once view_rot != 0). Solve pan
            // so cell_to_screen(cell_under_ptr) == ptr at the new zoom.
            let (cx, cy) = screen_to_cell(state, ptr);
            state.zoom = new_zoom;
            let (rx, ry) = rotate_vec(state.view_rot, cx, cy);
            state.pan = egui::vec2(ptr.x - rx * new_zoom, ptr.y - ry * new_zoom);
        }
    }
}

/// Rotate a 2D vector by `theta` radians (CCW). The view-rotation primitive both
/// transform helpers share, so they stay exact inverses (`R(θ)` then `R(−θ)`).
#[inline]
fn rotate_vec(theta: f32, x: f32, y: f32) -> (f32, f32) {
    let (s, c) = theta.sin_cos();
    (x * c - y * s, x * s + y * c)
}

/// Screen position of cell-space coordinate `(cx, cy)`.
/// `screen = pan + zoom · R(view_rot) · cell`.
fn cell_to_screen(state: &SculptState, cx: f32, cy: f32) -> Pos2 {
    let (rx, ry) = rotate_vec(state.view_rot, cx, cy);
    Pos2::new(state.pan.x + rx * state.zoom, state.pan.y + ry * state.zoom)
}

/// Cell-space coordinate under screen position `p` — the exact inverse of
/// [`cell_to_screen`]: `cell = R(−view_rot) · ((p − pan) / zoom)`.
fn screen_to_cell(state: &SculptState, p: Pos2) -> (f32, f32) {
    let ux = (p.x - state.pan.x) / state.zoom;
    let uy = (p.y - state.pan.y) / state.zoom;
    rotate_vec(-state.view_rot, ux, uy)
}

fn paint_terrain(state: &SculptState, ui: &Ui, rect: Rect, fw: u32, fh: u32) {
    let Some(tex) = state.texture.as_ref() else { return };
    let painter = ui.painter_at(rect);
    // `painter.image()` fills an axis-aligned Rect only, so a rotated view needs an
    // explicit textured quad: the four field corners through the rotation-aware
    // transform, UVs 0..1, two triangles. At view_rot == 0 this is the same blit.
    let corners = [
        (cell_to_screen(state, 0.0, 0.0), Pos2::new(0.0, 0.0)),
        (cell_to_screen(state, fw as f32, 0.0), Pos2::new(1.0, 0.0)),
        (cell_to_screen(state, fw as f32, fh as f32), Pos2::new(1.0, 1.0)),
        (cell_to_screen(state, 0.0, fh as f32), Pos2::new(0.0, 1.0)),
    ];
    let mut mesh = egui::Mesh::with_texture(tex.id());
    for (pos, uv) in corners {
        mesh.vertices.push(egui::epaint::Vertex { pos, uv, color: Color32::WHITE });
    }
    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
    painter.add(mesh);
}

/// Overlay the brick slice direction as a FIXED screen-space reference: bricks
/// slice DOWN-screen (the exported +Y after the view-rotation bake) and widen to
/// the RIGHT. Deliberately NOT drawn in cell space — it must stay put while the
/// terrain rotates under it, so the user can spin the depthmap to line a ridge up
/// with these cyan lanes (→ long bricks along the slice). Truthful once the export
/// bakes `view_rot` (see `start_convert`); at 0° it is the literal export axis.
fn paint_slice_overlay(ui: &Ui, rect: Rect) {
    let painter = ui.painter_at(rect);
    let slice = Stroke::new(2.0, Color32::from_rgba_unmultiplied(120, 210, 255, 200));
    let widen = Stroke::new(1.6, Color32::from_rgba_unmultiplied(255, 180, 120, 170));
    // Cyan slice lanes: vertical (screen-down) with a downward arrowhead.
    let lanes = 6;
    for i in 0..lanes {
        let x = rect.left() + rect.width() * (i as f32 + 0.5) / lanes as f32;
        let top = Pos2::new(x, rect.top() + rect.height() * 0.12);
        let bot = Pos2::new(x, rect.top() + rect.height() * 0.88);
        painter.line_segment([top, bot], slice);
        painter.line_segment([bot, bot + Vec2::new(-6.0, -9.0)], slice);
        painter.line_segment([bot, bot + Vec2::new(6.0, -9.0)], slice);
    }
    // Amber widen arrow (screen-right) near the top edge.
    let wy = rect.top() + rect.height() * 0.055;
    let (wa, wb) = (
        Pos2::new(rect.left() + rect.width() * 0.12, wy),
        Pos2::new(rect.left() + rect.width() * 0.30, wy),
    );
    painter.line_segment([wa, wb], widen);
    painter.line_segment([wb, wb + Vec2::new(-9.0, -6.0)], widen);
    painter.line_segment([wb, wb + Vec2::new(-9.0, 6.0)], widen);
    painter.text(
        rect.min + Vec2::new(8.0, 8.0),
        egui::Align2::LEFT_TOP,
        "bricks slice ↓ — rotate the terrain to align ridges with these lanes   ·   widen →",
        egui::FontId::proportional(12.0),
        Color32::from_rgb(180, 220, 255),
    );
}

/// Lay sculpt dabs along a primary-button drag, spaced ~`radius * DAB_SPACING`.
/// Snapshots the affected rect at stroke start (for undo) and commits it when the
/// stroke ends.
fn handle_sculpt_input(state: &mut SculptState, response: &egui::Response, fw: u32, fh: u32) {
    // Armed click-to-arm pick (clicked a height field's eyedropper chip): the next
    // primary click reads that spot's height into THAT field, then disarms. This
    // is a deliberate, one-shot mode — it suppresses sculpting while armed and is
    // separate from hold-E, so the two never collide. Esc cancels.
    if let Some(target) = state.armed_pick {
        response.ctx.set_cursor_icon(egui::CursorIcon::Crosshair);
        if response.ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            state.armed_pick = None;
            return;
        }
        if (response.clicked_by(egui::PointerButton::Primary)
            || response.drag_started_by(egui::PointerButton::Primary))
            && let Some(ptr) = response.interact_pointer_pos()
        {
            let (cx, cy) = screen_to_cell(state, ptr);
            state.sample_into(target, cx, cy);
            state.armed_pick = None;
        }
        return; // never sculpt while a pick is armed
    }

    // Hold E = eyedropper (spec §5, Photoshop-style): a primary click/drag
    // samples the hovered cell's height into the active Set/Flatten target, no dab
    // — release E to sculpt. Reuses the SAME map `Response` hit-test. (E, not Alt:
    // window managers grab Alt+drag to move the window — see `eyedropper_active`.)
    if eyedropper_active(&response.ctx) {
        response.ctx.set_cursor_icon(egui::CursorIcon::Crosshair);
        // If a sculpt stroke was in progress and the primary releases this frame
        // while E is now held, COMMIT it first — otherwise its already-applied
        // dabs are dropped from the undo timeline (audit 2026-06-30). Don't sample
        // on that release frame (it's the end of a stroke, not a pick).
        if response.drag_stopped_by(egui::PointerButton::Primary) {
            commit_stroke(state);
        } else if (response.clicked_by(egui::PointerButton::Primary)
            || response.dragged_by(egui::PointerButton::Primary))
            && let Some(ptr) = response.interact_pointer_pos()
        {
            let (cx, cy) = screen_to_cell(state, ptr);
            state.sample_height_into_target(cx, cy);
        }
        return;
    }

    // The Stamp primitive places a single dab on press, not a continuous stroke —
    // overlapping cone/ramp dabs along a drag would compound incoherently.
    if state.tool == SculptTool::Stamp {
        handle_stamp_input(state, response, fw, fh);
        return;
    }

    // Paint writes palette indices into the grid (color), not heights — its own
    // stroke + undo path, parallel to the height tools.
    if state.tool == SculptTool::Paint {
        handle_paint_input(state, response, fw, fh);
        return;
    }

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

/// Place a single Stamp primitive at the pressed cell. Fires once per press —
/// `drag_started` (press began) or `clicked` (press+release, no drag) — so a
/// drag can't smear multiple overlapping stamps. Wraps the one dab in its own
/// stroke snapshot + commit so it lands as a single undoable edit.
fn handle_stamp_input(state: &mut SculptState, response: &egui::Response, fw: u32, fh: u32) {
    let fired = response.drag_started_by(egui::PointerButton::Primary)
        || response.clicked_by(egui::PointerButton::Primary);
    if !fired {
        return;
    }
    let Some(ptr) = response.interact_pointer_pos() else { return };
    let center = screen_to_cell(state, ptr);
    state.active_stroke.clear();
    state.last_dab = None;
    apply_one_dab(state, center, fw, fh);
    commit_stroke(state);
}

/// Paint the active palette swatch along a primary-button drag, hard-writing the
/// index into every grid cell the brush footprint covers. One undo entry per
/// stroke: the whole grid is snapshotted at press (mirrors the zone-edit undo).
fn handle_paint_input(state: &mut SculptState, response: &egui::Response, fw: u32, fh: u32) {
    // Bucket is a click-to-fill, not a drag stroke — route it separately.
    if state.paint_tool == PaintTool::Bucket {
        handle_bucket_input(state, response, fw, fh);
        return;
    }
    if response.drag_started_by(egui::PointerButton::Primary) {
        // Snapshot the pre-stroke grid once, so the entire stroke undoes as a unit.
        state.record_paint_edit();
        state.last_dab = None;
    }
    let primary_down = response.dragged_by(egui::PointerButton::Primary)
        || response.drag_started_by(egui::PointerButton::Primary);
    if primary_down
        && let Some(ptr) = response.interact_pointer_pos()
    {
        let (cx, cy) = screen_to_cell(state, ptr);
        paint_dabs_along(state, (cx, cy), fw, fh);
    }
    if response.drag_stopped_by(egui::PointerButton::Primary) {
        state.last_dab = None;
    }
}

/// Paint-bucket: on a click, flood-fill from the hovered cell by height tolerance
/// (contiguous region, or global). One undoable edit per click.
fn handle_bucket_input(state: &mut SculptState, response: &egui::Response, fw: u32, fh: u32) {
    let fired = response.drag_started_by(egui::PointerButton::Primary)
        || response.clicked_by(egui::PointerButton::Primary);
    if !fired {
        return;
    }
    let Some(ptr) = response.interact_pointer_pos() else { return };
    let (cx, cy) = screen_to_cell(state, ptr);
    if cx < 0.0 || cy < 0.0 {
        return;
    }
    let (x, y) = (cx.floor() as u32, cy.floor() as u32);
    if x >= fw || y >= fh {
        return;
    }
    state.record_paint_edit();
    let idx = state.active_swatch;
    let tol = state.bucket_tolerance_m;
    let contiguous = !state.bucket_global;
    let r = state.splat_res.max(1);
    let field = state.field.as_ref().expect("field present during bucket fill");
    state.paint.flood_fill(x, y, idx, tol, contiguous, r, |x, y| field.at(x, y));
    state.mark_dirty_all();
}

/// Step from the last paint dab to `(cx, cy)`, painting a dab at each step so a
/// fast drag still lays a continuous band. Mirrors `apply_dabs_along`.
fn paint_dabs_along(state: &mut SculptState, to: (f32, f32), fw: u32, fh: u32) {
    let step = (state.brush.radius_cells * DAB_SPACING).max(MIN_DAB_STEP_CELLS);
    let from = state.last_dab.unwrap_or(to);
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let dist = (dx * dx + dy * dy).sqrt();
    let n = (dist / step).floor() as u32;
    let n = n.min(4096); // bounded (Rule 2): cap dabs per teleporting frame
    for i in 1..=n {
        let t = (i as f32) * step / dist.max(f32::EPSILON);
        paint_one_dab(state, (from.0 + dx * t, from.1 + dy * t), fw, fh);
    }
    paint_one_dab(state, to, fw, fh);
    state.last_dab = Some(to);
}

/// Hard-write the active swatch into every grid cell inside the brush footprint
/// at `center`. Index assignment isn't blendable, so there is no falloff — the
/// footprint shape (circle/square/diamond/hex) is the edge. Marks the painted
/// rect dirty so the overlay re-renders just that sub-rect.
fn paint_one_dab(state: &mut SculptState, center: (f32, f32), fw: u32, fh: u32) {
    let Some((rx, ry, rw, rh)) = stroke_rect(center, state.brush.radius_cells, fw, fh) else {
        return; // entirely off-grid
    };
    let brush = state.brush;
    let idx = state.active_swatch;
    let r = state.splat_res.max(1);
    for y in ry..ry + rh {
        for x in rx..rx + rw {
            let dx = x as f32 - center.0;
            let dy = y as f32 - center.1;
            if shape_distance(brush.shape, dx, dy, brush.radius_cells) < 1.0 {
                // Snap to the splat-resolution block (r=1 → per-cell).
                state.paint.set_block(x, y, r, idx);
            }
        }
    }
    // Block writes can spill up to r-1 cells past the footprint; expand the dirty
    // rect to block boundaries so the overlay re-render covers them.
    let x0 = (rx / r) * r;
    let y0 = (ry / r) * r;
    let x1 = ((rx + rw - 1) / r * r + r - 1).min(fw - 1);
    let y1 = ((ry + rh - 1) / r * r + r - 1).min(fh - 1);
    state.mark_dirty_rect((x0, x1, y0, y1));
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
    let stamp = state.stamp;
    tool.apply_dab(field, &brush, center, target, stamp);
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
        state.undo.push_back(UndoEntry::Height(snap));
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
    let Some(entry) = state.undo.pop_back() else { return };
    let inverse = invert_entry(state, entry);
    state.redo.push_back(inverse);
    while state.redo.len() > UNDO_CAP {
        state.redo.pop_front();
    }
}

fn do_redo(state: &mut SculptState) {
    let Some(entry) = state.redo.pop_back() else { return };
    let inverse = invert_entry(state, entry);
    state.undo.push_back(inverse);
    while state.undo.len() > UNDO_CAP {
        state.undo.pop_front();
    }
}

/// Apply one history entry to `state` and return the inverse entry (to push onto
/// the opposite deque). Shared by undo and redo — the operation is its own
/// inverse, so the only difference between the two is which deque is the source.
fn invert_entry(state: &mut SculptState, entry: UndoEntry) -> UndoEntry {
    match entry {
        UndoEntry::Height(snap) => {
            let Some(field) = state.field.as_mut() else {
                // No field to restore into (can't happen with height history) —
                // hand the entry back unchanged rather than lose it.
                return UndoEntry::Height(snap);
            };
            let inverse = snap.restore_into(field);
            // A height change can move the global extent (e.g. removing the
            // tallest peak), rescaling the colormap — force a full re-render.
            state.mark_dirty_all();
            UndoEntry::Height(inverse)
        }
        UndoEntry::Zones(prev) => {
            // Swap the stored list in; the displaced current list becomes the
            // inverse. Touches no cells, so the terrain texture stays clean —
            // the zone overlay redraws every frame regardless.
            let current = std::mem::replace(&mut state.zones, prev);
            UndoEntry::Zones(current)
        }
        UndoEntry::Paint(prev) => {
            // Swap the stored grid in; the displaced grid becomes the inverse. The
            // paint overlay is baked into the terrain texture, so force a rebuild.
            let current = std::mem::replace(&mut state.paint, prev);
            state.mark_dirty_all();
            UndoEntry::Paint(current)
        }
    }
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
        let terr = state.terrace.then_some(state.terrace_step_m);
        let sub = render_field_rect(field, &state.paint, &state.palette, halo, extent.0, extent.1, terr);
        let tex = state.texture.as_mut().expect("partial path has a texture");
        tex.set_partial([hx0 as usize, hy0 as usize], sub, opts);
        return;
    }

    // Full render: recompute the true global extent and cache it.
    let extent = field.min_max();
    let terr = state.terrace.then_some(state.terrace_step_m);
    let image = render_field_rect(field, &state.paint, &state.palette, (0, fw - 1, 0, fh - 1), extent.0, extent.1, terr);
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
    paint: &PaintGrid,
    palette: &[[u8; 4]],
    rect: (u32, u32, u32, u32),
    min: f32,
    max: f32,
    terrace_step: Option<f32>,
) -> egui::ColorImage {
    let (x0, x1, y0, y1) = rect;
    let rw = (x1 - x0 + 1) as usize;
    let rh = (y1 - y0 + 1) as usize;
    let span = (max - min).max(1e-6);
    let last_x = field.width - 1;
    let last_y = field.height - 1;
    // Light direction (top-left, slightly elevated) for the hillshade.
    let light = normalize3([-0.5, -0.5, 0.7]);
    // Terrace preview: snap sampled heights to steps so the canvas shows the same
    // stepped plateaus the export will build (sharp risers light up in hillshade).
    let sample = |x: u32, y: u32| match terrace_step {
        Some(step) => terrace_height(field.at(x, y), step),
        None => field.at(x, y),
    };

    let mut rgba = vec![0u8; rw * rh * 4];
    for (ry, y) in (y0..=y1).enumerate() {
        for (rx, x) in (x0..=x1).enumerate() {
            let height = sample(x, y);
            let t = ((height - min) / span).clamp(0.0, 1.0);
            let base = hypsometric(t);

            // Surface normal from central differences (cells are heights in
            // meters; the cell pitch cancels into a constant slope scale).
            let slope = 1.0_f32; // relative; visual only
            let xl = sample(x.saturating_sub(1), y);
            let xr = sample((x + 1).min(last_x), y);
            let yu = sample(x, y.saturating_sub(1));
            let yd = sample(x, (y + 1).min(last_y));
            let nx = (xl - xr) * slope;
            let ny = (yu - yd) * slope;
            let normal = normalize3([nx, ny, 2.0]);
            let lambert = dot3(normal, light).clamp(0.0, 1.0);
            // Blend the flat base toward shaded so even flat ground keeps color.
            let shade = 0.55 + 0.45 * lambert;

            let mut col = [base[0] as f32 * shade, base[1] as f32 * shade, base[2] as f32 * shade];
            // Splat overlay: tint painted cells toward their swatch (still shaded,
            // so relief reads through). Unpainted (index 0) cells are unchanged,
            // keeping the no-paint view identical to before.
            let pidx = paint.at(x, y) as usize;
            if pidx != 0
                && let Some(pc) = palette.get(pidx)
            {
                const A: f32 = 0.6;
                for c in 0..3 {
                    col[c] = col[c] * (1.0 - A) + (pc[c] as f32 * shade) * A;
                }
            }

            let idx = (ry * rw + rx) * 4;
            rgba[idx] = col[0] as u8;
            rgba[idx + 1] = col[1] as u8;
            rgba[idx + 2] = col[2] as u8;
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

// ----- Export heightmap PNG ------------------------------------------------

/// Save the current sculpted field as an rgba-encoded heightmap PNG via the
/// native save dialog. Encoding + write are synchronous (sculpt grids are small
/// and this is a one-shot user action). The PNG round-trips through the CLI/map
/// pipeline's `HeightmapPNG` decoder — see [`HeightField::to_heightmap_png`].
fn export_heightmap_png(state: &mut SculptState) {
    let Some(field) = state.field.as_ref() else {
        state.last_error = Some("internal: export with no field".into());
        return;
    };
    let trimmed = state.output_name.trim();
    let stem = if trimmed.is_empty() { "heightmap" } else { trimmed };
    let result = native_dialog::DialogBuilder::file()
        .add_filter("PNG heightmap", ["png"])
        .set_filename(format!("{stem}.png"))
        .save_single_file()
        .show();
    let path = match result {
        Ok(Some(p)) => p,
        Ok(None) => return, // user cancelled
        Err(e) => {
            state.last_error = Some(format!("file dialog failed: {e}"));
            return;
        }
    };
    match field.to_heightmap_png().save(&path) {
        Ok(()) => {
            state.last_error = None;
            state.last_export = Some(path);
        }
        Err(e) => state.last_error = Some(format!("could not write heightmap PNG: {e}")),
    }
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
    let dynimg = match image::ImageReader::open(&path)
        .and_then(|r| r.decode().map_err(std::io::Error::other))
    {
        Ok(im) => im,
        Err(e) => {
            state.last_error = Some(format!("could not read image: {e}"));
            return;
        }
    };
    if dynimg.width() == 0 || dynimg.height() == 0 {
        state.last_error = Some("image has zero dimensions".to_owned());
        return;
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sculpt".to_owned());
    let mut meta = blank_meta(state, state.new_cell_m);
    meta.source_name = stem;
    // Our own "Export heightmap PNG" writes a 4-channel RGBA image that packs
    // `round(meters*100)` as a big-endian u32 — decoding THAT as 8-bit luminance
    // crushes every height ~0.045× (the round-trip bug). Branch on the channel
    // layout: a 4-channel image is treated as our packed heightmap and decoded
    // losslessly; a grayscale image is an external heightmap (luminance = meters).
    let field = if matches!(dynimg.color(), image::ColorType::Rgba8 | image::ColorType::Rgba16) {
        HeightField::from_heightmap_png(&dynimg.to_rgba8(), meta)
    } else {
        HeightField::from_image(&dynimg.to_luma8(), meta)
    };
    state.set_field(field);
    state.last_error = None;
}

/// Load an RGBA splatmap and decode it into the paint grid: each pixel's dominant
/// channel selects a palette layer (1–4), nearest-resampled to the field dims.
/// Undoable (snapshots the prior grid). No-op with no field loaded.
fn load_splatmap(state: &mut SculptState) {
    let (fw, fh) = match state.field.as_ref() {
        Some(f) => (f.width, f.height),
        None => return,
    };
    let result = native_dialog::DialogBuilder::file()
        .add_filter("Splatmap (PNG)", ["png"])
        .open_single_file()
        .show();
    let path = match result {
        Ok(Some(p)) => p,
        Ok(None) => return, // cancelled
        Err(e) => {
            state.last_error = Some(format!("file dialog failed: {e}"));
            return;
        }
    };
    let img = match image::ImageReader::open(&path)
        .and_then(|r| r.decode().map_err(std::io::Error::other))
    {
        Ok(im) => im.to_rgba8(),
        Err(e) => {
            state.last_error = Some(format!("could not read splatmap: {e}"));
            return;
        }
    };
    if img.width() == 0 || img.height() == 0 {
        state.last_error = Some("splatmap has zero dimensions".to_owned());
        return;
    }
    // Snapshot for undo, then replace the grid with the decoded indices.
    state.record_paint_edit();
    let cells = splatmap_to_indices(&img, fw, fh);
    state.paint = PaintGrid { width: fw, height: fh, cells };
    state.mark_dirty_all();
    state.last_error = None;
}

// ----- Convert worker ------------------------------------------------------

fn start_convert(state: &mut SculptState) {
    let Some(field) = state.field.clone() else {
        state.last_error = Some("internal: Convert with no field".into());
        return;
    };
    // Stamp the current output name + the Export-panel scale knobs into the
    // field metadata so the written file is named from the UI and the convert
    // honors the panel's studs/m · exaggeration · micro (not the field's
    // original/blank-canvas seed). This is the single point where panel state
    // becomes the FieldMeta the convert reads — blank canvas, loaded image, and
    // send-from-map all flow through here.
    let mut field = field;
    field.meta.source_name = state.output_name.clone();
    field.meta.studs_per_meter = state.studs_per_meter;
    field.meta.vertical_exaggeration = state.vertical_exaggeration;
    field.meta.micro = state.micro;

    let out = state.out;
    let floor_level_m = state.floor_level_m;
    let omit_below_m = state.omit_below_m;
    let tile_export = state.tile_export;
    let tile_cells = state.tile_cells;
    // Stepped/terraced export: snap heights to the step size, or None for smooth.
    let terrace_step_m = state.terrace.then_some(state.terrace_step_m);
    let max_brick_units = state.max_brick_units;
    // Clone the zones into the worker alongside the field. Converting READS them
    // (rasterized to a keep-mask) but never clears `state.zones` — they persist
    // so the user can re-export, refine, or clear explicitly.
    let zones = state.zones.clone();
    // Clone the paint grid + palette into the worker (owned, so the borrowed
    // PaintLayer can be built inside the thread). An unpainted grid keeps the
    // build byte-identical to today.
    let paint_grid = state.paint.clone();
    let palette = state.palette.clone();

    // Bake the preview view-rotation into the export grid: the height field, its
    // parallel paint grid, AND the zone polygons rotate by the SAME angle onto the
    // SAME rotated dims (`rotated_dims`), so a feature aligned with the screen-down
    // slice lanes exports along +Y and the colormap + keep-mask stay registered to
    // the rotated terrain. θ == 0 leaves all three byte-identical.
    let (field, zones, paint_grid) = if state.view_rot != 0.0 {
        let theta = state.view_rot;
        let (sw, sh) = (field.width, field.height);
        let (nw, nh) = super::heightfield::rotated_dims(sw, sh, theta);
        (
            field.rotated(theta),
            crate::gui::zones::rotate_zones(&zones, sw, sh, nw, nh, theta),
            paint_grid.rotated(theta),
        )
    } else {
        (field, zones, paint_grid)
    };

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
            // "Tile this export" routes through the grid-tiled stitch path (one
            // stitched save built from partition sub-fields); off = single mesh.
            let paint_layer = Some(PaintLayer { grid: &paint_grid, palette: &palette });
            let result = if tile_export {
                convert_heightfield_tiled(
                    &field, out, tile_cells, floor_level_m, omit_below_m, &zones, paint_layer,
                    terrace_step_m, max_brick_units, progress_fn, cancel_arc,
                )
            } else {
                convert_heightfield(
                    &field, out, floor_level_m, omit_below_m, &zones, paint_layer, terrace_step_m,
                    max_brick_units, progress_fn, cancel_arc,
                )
            };
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
    use crate::gui::zones::ZoneMode;

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

    fn test_zone(mode: ZoneMode) -> Zone {
        Zone {
            mode,
            polygon: vec![(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0)],
        }
    }

    /// Adding a zone is undoable: undo restores the prior (empty) list, redo
    /// re-adds it.
    #[test]
    fn zone_add_undo_redo() {
        let mut state = SculptState::new();
        state.add_zone(test_zone(ZoneMode::Omit));
        assert_eq!(state.zones.len(), 1, "zone added");
        assert_eq!(state.undo.len(), 1, "one zone edit → one undo entry");

        do_undo(&mut state);
        assert!(state.zones.is_empty(), "undo restores the empty zones list");
        assert_eq!(state.redo.len(), 1, "undo pushes a redo entry");

        do_redo(&mut state);
        assert_eq!(state.zones.len(), 1, "redo re-adds the zone");
    }

    /// Delete and clear-all are undoable.
    #[test]
    fn zone_delete_and_clear_are_undoable() {
        let mut state = SculptState::new();
        state.add_zone(test_zone(ZoneMode::Omit));
        state.add_zone(test_zone(ZoneMode::Include));
        assert_eq!(state.zones.len(), 2);

        state.delete_zone(0);
        assert_eq!(state.zones.len(), 1, "one deleted");
        assert_eq!(state.zones[0].mode, ZoneMode::Include, "the right one survived");
        do_undo(&mut state);
        assert_eq!(state.zones.len(), 2, "delete undone");

        state.clear_zones();
        assert!(state.zones.is_empty(), "cleared");
        do_undo(&mut state);
        assert_eq!(state.zones.len(), 2, "clear undone");
    }

    /// Clearing an already-empty zone list must not push a no-op undo entry.
    #[test]
    fn clear_empty_zones_records_no_history() {
        let mut state = SculptState::new();
        state.clear_zones();
        assert!(state.undo.is_empty(), "clearing nothing records no edit");
    }

    /// A height stroke and a zone edit share one timeline: undo pops the most
    /// recent first (zone), then the height stroke — strict LIFO across types.
    #[test]
    fn interleaved_height_and_zone_undo_is_lifo() {
        let mut state = SculptState::new();
        state.set_field(HeightField::flat(20, 20, meta()));
        let original = state.field.as_ref().unwrap().cells.clone();
        state.tool = SculptTool::Raise;
        state.brush = Brush {
            shape: BrushShape::Circle,
            radius_cells: 3.0,
            strength: 5.0,
            falloff: Falloff::Smoothstep,
        };

        // 1) a height stroke, then 2) a zone add.
        apply_one_dab(&mut state, (5.0, 5.0), 20, 20);
        commit_stroke(&mut state);
        let after_height = state.field.as_ref().unwrap().cells.clone();
        assert_ne!(after_height, original, "stroke changed the field");
        state.add_zone(test_zone(ZoneMode::Omit));
        assert_eq!(state.undo.len(), 2, "height + zone on one timeline");

        // Undo #1 pops the zone (last in), leaving the height edit applied.
        do_undo(&mut state);
        assert!(state.zones.is_empty(), "zone undone first (LIFO)");
        assert_eq!(
            state.field.as_ref().unwrap().cells,
            after_height,
            "the height edit is untouched by the zone undo",
        );

        // Undo #2 pops the height stroke, restoring the original field.
        do_undo(&mut state);
        assert_eq!(
            state.field.as_ref().unwrap().cells,
            original,
            "height stroke undone second, field back to original",
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

    /// Undo history is bounded: committing more than UNDO_CAP strokes saturates
    /// the deque at EXACTLY UNDO_CAP (not fewer — an over-trim regression must
    /// fail) and the retained entries still undo cleanly (the oldest kept stroke
    /// is restorable, i.e. eviction dropped the front, not corrupted the rest).
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
        // Saturated at the cap exactly — over-trim (e.g. to 0) would fail here.
        assert_eq!(
            state.undo.len(),
            UNDO_CAP,
            "undo deque must saturate AT the cap after overflowing it",
        );
        // Integrity: every retained entry undoes without panicking, draining the
        // deque to empty (eviction removed the front, leaving a valid timeline).
        for _ in 0..UNDO_CAP {
            do_undo(&mut state);
        }
        assert!(state.undo.is_empty(), "all retained entries undo cleanly");
        assert_eq!(state.redo.len(), UNDO_CAP, "each undo pushed a redo entry");
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

    /// Paint stroke undo/redo: a recorded paint edit restores the prior grid on
    /// undo and re-applies the painted indices on redo, on the shared timeline.
    #[test]
    fn paint_stroke_undo_redo() {
        let mut state = SculptState::new();
        state.set_field(HeightField::flat(8, 8, meta()));
        assert!(state.paint.is_blank(), "a fresh field starts unpainted");

        // Record (as the stroke press would) then paint two cells with swatch 2.
        state.record_paint_edit();
        state.paint.set_block(3, 3, 1, 2);
        state.paint.set_block(4, 3, 1, 2);
        assert_eq!(state.undo.len(), 1, "one paint stroke = one undo entry");

        do_undo(&mut state);
        assert!(state.paint.is_blank(), "undo restores the unpainted grid");
        do_redo(&mut state);
        assert_eq!(state.paint.at(3, 3), 2, "redo re-applies the painted swatch");
        assert_eq!(state.paint.at(4, 3), 2);

        // A field swap re-blanks the grid to the new dims (cell-aligned invariant).
        state.set_field(HeightField::flat(5, 5, meta()));
        assert_eq!((state.paint.width, state.paint.height), (5, 5), "grid tracks field dims");
        assert!(state.paint.is_blank(), "a new field clears paint");
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
        // Blank paint grid → the overlay is a no-op, so this still pins the pure
        // terrain partial==full invariant.
        let paint = PaintGrid::blank(field.width, field.height);
        let pal = default_palette();
        let full = render_field_rect(&field, &paint, &pal, (0, field.width - 1, 0, field.height - 1), min, max, None);

        // A halo-expanded interior sub-rect, exactly as the drag path would build.
        let sub_rect = expand_rect((20, 27, 14, 19), 1, field.width, field.height);
        let (sx0, sx1, sy0, sy1) = sub_rect;
        let sub = render_field_rect(&field, &paint, &pal, sub_rect, min, max, None);

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

    /// Modifier step-selection (spec §2): Ctrl ⇒ fine (×0.1), Alt ⇒ coarse
    /// (×10), none ⇒ base (×1.0), and BOTH held ⇒ Ctrl wins (fine) — the
    /// documented precedence.
    #[test]
    fn modifier_drag_step_selection() {
        let none = modifier_step(DragModifiers { ctrl: false, alt: false });
        let fine = modifier_step(DragModifiers { ctrl: true, alt: false });
        let coarse = modifier_step(DragModifiers { ctrl: false, alt: true });
        let both = modifier_step(DragModifiers { ctrl: true, alt: true });
        assert_eq!(none, 1.0, "no modifier must leave the base speed (×1)");
        assert_eq!(fine, MODIFIER_FINE, "Ctrl must select the fine (×0.1) step");
        assert_eq!(coarse, MODIFIER_COARSE, "Alt must select the coarse (×10) step");
        assert_eq!(both, MODIFIER_FINE, "Ctrl+Alt: Ctrl wins (fine) — documented precedence");
    }

    /// Brush/shape sizes are surfaced in Brickadia studs (the export unit). One
    /// cell is the smallest brick: footprint `2*hscale*upf` units, 1 stud = 5
    /// units → `studs_per_cell = 2*hscale*upf/5`. Uses the achieved integer
    /// hscale, so a sub-1-brick ask snaps up to the 1-brick minimum.
    #[test]
    fn studs_per_cell_reflects_achieved_brick_scale() {
        let meta = |cell_m: f64, spm: f32, micro: bool| FieldMeta {
            cell_m,
            studs_per_meter: spm,
            vertical_exaggeration: 1.0,
            micro,
            centroid_lat: 0.0,
            source_name: String::new(),
        };
        // Normal: derive_scale(1.0, 6.0) → hscale 3 → 2*3*5/5 = 6 studs/cell.
        assert_eq!(studs_per_cell(&meta(1.0, 6.0, false)), 6.0);
        // Micro reaches the same physical studs via a 5×-finer integer hscale (15).
        assert_eq!(studs_per_cell(&meta(1.0, 6.0, true)), 6.0);
        // A sub-1-brick ask clamps hscale to 1 (the minimum brick): 2*1*5/5 = 2.
        assert_eq!(studs_per_cell(&meta(2.0, 0.5, false)), 2.0);
    }

    /// Brickadia vertical parity: 1 flat = 4 height-units; 1 brick = 3 flats. The
    /// meters↔flats round-trip and the "Nb Mf" format/parse must be exact, so a
    /// height the user types in bricks+flats exports to the matching brick stack.
    #[test]
    fn brickadia_height_bricks_flats_round_trip() {
        // vscale = 4 units/m → 1 m = 1 flat, 3 m = 1 brick.
        assert_eq!(meters_to_flats(1.0, 4.0), 1.0);
        assert_eq!(meters_to_flats(3.0, 4.0), 3.0);
        assert!((flats_to_meters(meters_to_flats(7.5, 4.0), 4.0) - 7.5).abs() < 1e-4);
        assert_eq!(flats_to_meters(6.0, 0.0), 0.0, "guard div-by-zero vscale");
        // Format: bricks + flats, suppressing zero parts.
        assert_eq!(fmt_bricks_flats(7.0), "2b 1f");
        assert_eq!(fmt_bricks_flats(3.0), "1b");
        assert_eq!(fmt_bricks_flats(2.0), "2f");
        assert_eq!(fmt_bricks_flats(0.0), "0f");
        // Parse: spaced, space-LESS, single units, bare flats, and rejects junk.
        assert_eq!(parse_bricks_flats("2b 1f"), Some(7.0));
        assert_eq!(parse_bricks_flats("2b1f"), Some(7.0), "space-less compound must parse");
        assert_eq!(parse_bricks_flats("1b"), Some(3.0));
        assert_eq!(parse_bricks_flats("2f"), Some(2.0));
        assert_eq!(parse_bricks_flats("5"), Some(5.0));
        assert_eq!(parse_bricks_flats("garbage"), None);
        assert_eq!(parse_bricks_flats("3 b"), None, "number then space then unit is junk");
        // Round-trip through format isn't required, but parse∘fmt preserves count.
        assert_eq!(parse_bricks_flats(&fmt_bricks_flats(7.0)), Some(7.0));

        // Signed (the Stamp peak digs craters): the sign prints ONCE over the
        // whole magnitude and round-trips through parse, so a negative peak typed
        // or dragged shows and reads back the same brick stack.
        assert_eq!(fmt_bricks_flats(-7.0), "-2b 1f");
        assert_eq!(fmt_bricks_flats(-3.0), "-1b");
        assert_eq!(fmt_bricks_flats(-2.0), "-2f");
        assert_eq!(parse_bricks_flats("-2b 1f"), Some(-7.0), "leading - negates the whole");
        assert_eq!(parse_bricks_flats("-5"), Some(-5.0), "bare negative flats");
        assert_eq!(parse_bricks_flats(&fmt_bricks_flats(-7.0)), Some(-7.0), "negative round-trip");
        assert_eq!(parse_bricks_flats("-"), None, "a lone minus is junk");
    }

    /// `cell_to_screen` and `screen_to_cell` must stay exact inverses at ANY view
    /// rotation — every pointer tool routes through them, so drift here breaks
    /// every brush/stamp/zone hit-test once the view is rotated.
    #[test]
    fn view_transform_inverse_holds_under_rotation() {
        let mut st = SculptState { pan: egui::vec2(120.0, 80.0), zoom: 6.5, ..Default::default() };
        for &deg in &[0.0_f32, 30.0, 90.0, 180.0, -47.0] {
            st.view_rot = deg.to_radians();
            for &(cx, cy) in &[(0.0_f32, 0.0), (12.3, 4.7), (200.0, 5.0)] {
                let p = cell_to_screen(&st, cx, cy);
                let (rx, ry) = screen_to_cell(&st, p);
                assert!(
                    (rx - cx).abs() < 1e-2 && (ry - cy).abs() < 1e-2,
                    "round-trip failed at {deg}° cell ({cx},{cy}) → ({rx},{ry})",
                );
            }
        }
    }

    /// The scroll-zoom anchor solve (mirrored from `handle_view_input`) keeps the
    /// cell under the pointer fixed across a zoom change even when rotated.
    #[test]
    fn zoom_anchor_keeps_pointer_cell_fixed_under_rotation() {
        let mut st = SculptState {
            pan: egui::vec2(50.0, 60.0),
            zoom: 4.0,
            view_rot: 35.0_f32.to_radians(),
            ..Default::default()
        };
        let ptr = Pos2::new(300.0, 220.0);
        let before = screen_to_cell(&st, ptr);
        // Exactly the anchor math handle_view_input runs on a zoom step.
        let new_zoom = 7.3_f32;
        let (cx, cy) = screen_to_cell(&st, ptr);
        st.zoom = new_zoom;
        let (rx, ry) = rotate_vec(st.view_rot, cx, cy);
        st.pan = egui::vec2(ptr.x - rx * new_zoom, ptr.y - ry * new_zoom);
        let after = screen_to_cell(&st, ptr);
        assert!(
            (after.0 - before.0).abs() < 1e-2 && (after.1 - before.1).abs() < 1e-2,
            "pointer cell drifted across zoom: {before:?} → {after:?}",
        );
    }

    /// The brush radius/strength SLIDERS consume the same modifier scaling the
    /// DragValues do (spec §2 / DoD §8 "every numeric slider"): their value-box
    /// drag speed is `base_speed` scaled by Ctrl(×0.1)/Alt(×10)/none, via the
    /// `slider_drag_speed` helper `modifier_slider` feeds into `drag_value_speed`.
    /// This guards the gap the old `egui::Slider::new` brush controls left — they
    /// ignored the modifiers entirely.
    #[test]
    fn brush_slider_applies_modifier_scaling() {
        let base = 0.2_f64;
        let none = slider_drag_speed(base, DragModifiers { ctrl: false, alt: false });
        let fine = slider_drag_speed(base, DragModifiers { ctrl: true, alt: false });
        let coarse = slider_drag_speed(base, DragModifiers { ctrl: false, alt: true });
        let both = slider_drag_speed(base, DragModifiers { ctrl: true, alt: true });
        assert_eq!(none, base, "no modifier must leave the slider's base drag speed");
        assert_eq!(fine, base * MODIFIER_FINE, "Ctrl must make the brush slider ×0.1 finer");
        assert_eq!(coarse, base * MODIFIER_COARSE, "Alt must make the brush slider ×10 coarser");
        assert_eq!(both, base * MODIFIER_FINE, "Ctrl+Alt on the slider: Ctrl wins (fine)");
        // A clearly different base scales independently — the helper is not pinned
        // to one constant, it is the live brush-control base × the modifier step.
        let strength_fine = slider_drag_speed(0.005, DragModifiers { ctrl: true, alt: false });
        assert!(
            (strength_fine - 0.005 * MODIFIER_FINE).abs() < f64::EPSILON,
            "the blend-strength slider's finer base scales by the same rule",
        );
    }

    /// The Export-panel scale knobs flow into the `FieldMeta` the convert reads
    /// (spec §1/§3): a blank canvas reads studs_per_meter / vertical_exaggeration
    /// / micro from the panel state, not the old hardcoded 4.0 / 1.0 / false. Drive
    /// the panel to non-default values and assert `blank_meta` reflects them.
    #[test]
    fn field_meta_reflects_panel_scale() {
        let mut state = SculptState::new();
        state.studs_per_meter = 7.5;
        state.vertical_exaggeration = 2.5;
        state.micro = true;
        let meta = blank_meta(&state, 12.0);
        assert_eq!(meta.cell_m, 12.0, "cell pitch comes from the New-canvas control");
        assert_eq!(meta.studs_per_meter, 7.5, "studs/m must come from the panel, not a constant");
        assert_eq!(meta.vertical_exaggeration, 2.5, "exaggeration must come from the panel");
        assert!(meta.micro, "micro must come from the panel toggle");
    }

    /// Loading a field seeds the panel scale from the SOURCE meta (send-from-map /
    /// DEM primes the panel), then a convert restamps the field meta from the
    /// (possibly retuned) panel — the round-trip that makes the panel the single
    /// source of truth for the convert's scale.
    #[test]
    fn set_field_seeds_panel_then_panel_drives_convert() {
        let mut state = SculptState::new();
        // A source field carrying a distinct scale (as a DEM/send-from-map would).
        let src_meta = FieldMeta {
            cell_m: 30.0,
            studs_per_meter: 1.25,
            vertical_exaggeration: 3.0,
            micro: true,
            centroid_lat: 0.0,
            source_name: "src".to_string(),
        };
        state.set_field(HeightField::flat(8, 8, src_meta));
        assert_eq!(state.studs_per_meter, 1.25, "set_field must seed studs/m from the source");
        assert_eq!(state.vertical_exaggeration, 3.0, "set_field must seed exaggeration");
        assert!(state.micro, "set_field must seed micro from the source");

        // The user retunes the panel; a convert must stamp the NEW values onto the
        // field meta (the start_convert restamp). Reproduce that mapping here.
        state.studs_per_meter = 4.0;
        state.vertical_exaggeration = 1.0;
        state.micro = false;
        let mut field = state.field.clone().expect("field present");
        field.meta.studs_per_meter = state.studs_per_meter;
        field.meta.vertical_exaggeration = state.vertical_exaggeration;
        field.meta.micro = state.micro;
        assert_eq!(field.meta.studs_per_meter, 4.0, "convert reads the retuned panel studs/m");
        assert_eq!(field.meta.vertical_exaggeration, 1.0, "convert reads the retuned exaggeration");
        assert!(!field.meta.micro, "convert reads the retuned micro");
    }

    /// The export estimate gates over-budget fields (spec §1): a field whose
    /// single-mesh brick ceiling exceeds [`build::MAX_BRICKS`] reports
    /// `over_brick_cap` and `!fits_ram`; a tiny field with ample RAM fits. RAM is
    /// injected so the test is deterministic.
    #[test]
    fn export_estimate_gates_over_budget() {
        let ample_ram = 64u64 * 1024 * 1024 * 1024;
        // A small field: well under the brick cap and the RAM budget → fits.
        let small = single_mesh_estimate(64 * 64, ample_ram);
        assert!(!small.over_brick_cap, "a 4 k-cell field is far under the brick cap");
        assert!(small.fits_ram, "a tiny mesh must fit with 64 GiB free");
        assert_eq!(small.est_bricks, 64 * 64, "est is the cell-count ceiling");

        // A field whose cell count exceeds the per-mesh brick cap → over_brick_cap,
        // and the button-gating predicate `!fits_ram` trips regardless of RAM.
        let huge_cells = build::MAX_BRICKS as u64 + 1;
        let huge = single_mesh_estimate(huge_cells, ample_ram);
        assert!(huge.over_brick_cap, "exceeding MAX_BRICKS must flag over_brick_cap");
        assert!(!huge.fits_ram, "over the brick cap must gate the Export button");

        // Same modest field but with almost no RAM free → fails the RAM budget.
        let starved = single_mesh_estimate(200_000, RAM_RESERVE_BYTES_LOCAL + 1);
        assert!(!starved.fits_ram, "a peak over (available - reserve) must not fit");
    }

    /// The TILED estimate gates on the AGGREGATE cap (spec §1/§5): tile count is
    /// `ceil(w/tile)·ceil(h/tile)`; a field whose stitched union exceeds
    /// `MAX_GRID_BRICKS` (not the per-mesh `MAX_BRICKS`) flags `over_brick_cap` and
    /// `!fits_ram`. A field that trips the single-mesh cap but stitches under the
    /// grid cap FITS when tiled — the panel's "enable tiling" remedy is real.
    #[test]
    fn tiled_export_estimate_gates_over_budget() {
        let ample_ram = 256u64 * 1024 * 1024 * 1024;

        // Tile count math: a 1000×500 field at tile 256 → ceil(1000/256)=4 cols,
        // ceil(500/256)=2 rows = 8 tiles.
        assert_eq!(tiles_on_axis(1000, 256), 4, "ceil(1000/256) = 4 columns");
        assert_eq!(tiles_on_axis(500, 256), 2, "ceil(500/256) = 2 rows");
        let est = tiled_estimate(1000, 500, 256, ample_ram);
        assert_eq!(est.tile_count, 8, "4×2 = 8 tiles");
        assert!(!est.over_brick_cap, "a 0.5 M-cell field stitches well under MAX_GRID_BRICKS");
        assert!(est.fits_ram, "8 modest tiles must fit with 256 GiB free");

        // A field that exceeds the per-mesh MAX_BRICKS as a SINGLE mesh, but whose
        // tiled stitch is still under MAX_GRID_BRICKS → tiling is the remedy: the
        // single estimate gates, the tiled estimate fits.
        let big_side = 2000u32; // 4 M cells > MAX_BRICKS(2 M)
        let single = single_mesh_estimate(u64::from(big_side) * u64::from(big_side), ample_ram);
        assert!(single.over_brick_cap, "4 M cells over the per-mesh cap as one mesh");
        let tiled = tiled_estimate(big_side, big_side, 256, ample_ram);
        assert!(
            !tiled.over_brick_cap && tiled.fits_ram,
            "the same field tiled stitches under MAX_GRID_BRICKS and fits — tiling is the remedy",
        );

        // A field whose tiled stitch itself blows MAX_GRID_BRICKS → over the
        // aggregate cap, gated even when tiled. Per-tile body (256+1)² ≈ 66 k
        // bricks; enough tiles to pass 50 M needs > ~760 tiles → a ~28 k-cell side.
        let huge_side = 30_000u32;
        let huge = tiled_estimate(huge_side, huge_side, 256, ample_ram);
        assert!(huge.over_brick_cap, "a stitch past MAX_GRID_BRICKS must flag over_brick_cap");
        assert!(!huge.fits_ram, "over the aggregate cap must gate the Export button even when tiled");
    }

    /// Mirror of the grid reserve so the RAM-budget test reads the same threshold
    /// the estimate uses, without re-exporting a private constant into the test.
    const RAM_RESERVE_BYTES_LOCAL: u64 = 12 * 1024 * 1024 * 1024;

    /// Hold-E eyedropper: sampling a cell writes that cell's height (meters
    /// above floor) into the active Set/Flatten target. Exercises the pure
    /// `sample_height_into_target` core the canvas handler calls after mapping the
    /// pointer to a cell — including the nearest-cell floor for a fractional point.
    #[test]
    fn eyedropper_sample_sets_target_height() {
        let mut state = SculptState::new();
        let mut field = HeightField::flat(4, 4, meta());
        field.set(2, 1, 42.5);
        state.set_field(field);

        state.sample_height_into_target(2.0, 1.0);
        assert_eq!(state.target_height, 42.5, "the eyedropper samples the hovered height into Set/Flatten");
        // A fractional pointer inside the same cell floors to it (nearest-cell).
        state.target_height = 0.0;
        state.sample_height_into_target(2.9, 1.1);
        assert_eq!(state.target_height, 42.5, "fractional pointer samples the same cell");
        // An untouched cell is at the floor (0 m).
        state.sample_height_into_target(0.0, 0.0);
        assert_eq!(state.target_height, 0.0, "an unedited cell samples to the floor");
    }

    /// Click-to-arm pick routes the sampled height into the SPECIFIC field armed —
    /// Set/Sea level/Floor level — never crossing wires. (The arm/disarm UI lives
    /// in egui; this pins the pure routing core the canvas handler calls.)
    #[test]
    fn armed_pick_routes_height_to_each_field() {
        let mut state = SculptState::new();
        let mut field = HeightField::flat(4, 4, meta());
        field.set(1, 1, 11.0);
        field.set(2, 2, 22.0);
        field.set(3, 3, 33.0);
        state.set_field(field);

        state.sample_into(PickTarget::SetHeight, 1.0, 1.0);
        assert_eq!(state.target_height, 11.0, "SetHeight pick fills target_height");
        state.sample_into(PickTarget::SeaLevel, 2.0, 2.0);
        assert_eq!(state.omit_below_m, 22.0, "SeaLevel pick fills omit_below_m");
        state.sample_into(PickTarget::FloorLevel, 3.0, 3.0);
        assert_eq!(state.floor_level_m, 33.0, "FloorLevel pick fills floor_level_m");

        // Each pick is independent — the others are untouched by the last.
        assert_eq!(state.target_height, 11.0, "earlier picks are not disturbed");
        assert_eq!(state.omit_below_m, 22.0, "earlier picks are not disturbed");
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
