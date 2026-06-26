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

use egui::{Color32, Pos2, Rect, Sense, Stroke, TextEdit, Ui, Vec2};
use poll_promise::Promise;

use crate::gui::build::{self, BuildError, BuildOutcome, BuildStage};
use crate::gui::theme::{STATUS_ERROR_FG, STATUS_WARN_FG};
use crate::gui::zones::{Zone, ZoneMode};

use super::brush::{Brush, BrushShape, Falloff};
use super::convert::{convert_heightfield, convert_heightfield_tiled, OutputOptions};
use super::heightfield::{FieldMeta, HeightField, FLOOR_M};
use super::tools::{Flatten, Lower, Raise, SetHeight, Smooth, Tool};

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

/// Which level a sampled eyedropper height (meters) is routed into (spec §3).
/// The active keyboard modifier at click time selects the destination: a plain
/// click sets the Flatten/Set target, Alt the floor level, Ctrl the omit level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PickTarget {
    /// Plain click → the Flatten/Set `target_height`.
    Target,
    /// Alt-click → `floor_level_m` (the base plane).
    Floor,
    /// Ctrl-click → `omit_below_m` (the omit-water level).
    Omit,
}

/// Route an eyedropper sample by the held modifiers (spec §3): **plain ⇒
/// target**, **Alt ⇒ floor**, **Ctrl ⇒ omit**. Precedence when BOTH are held:
/// **Ctrl wins** (omit) — mirroring [`modifier_step`]'s "Ctrl wins" rule so the
/// whole UI shares one modifier precedence. Pure so the routing is unit-tested
/// directly without an egui context.
fn pick_target(m: DragModifiers) -> PickTarget {
    if m.ctrl {
        PickTarget::Omit
    } else if m.alt {
        PickTarget::Floor
    } else {
        PickTarget::Target
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
    /// Freedraw omit/include zones — not a brush; it draws loops, not heights.
    Zone,
}

/// How a zone loop is captured on the canvas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZoneStyle {
    /// Press-drag a freehand loop; auto-closed on release.
    Lasso,
    /// Click vertices; close on a click near the first vertex or a double-click.
    Polygon,
}

impl SculptTool {
    const ALL: [SculptTool; 6] =
        [Self::Raise, Self::Lower, Self::Smooth, Self::Flatten, Self::Set, Self::Zone];

    const fn label(self) -> &'static str {
        match self {
            Self::Raise => "Raise",
            Self::Lower => "Lower",
            Self::Smooth => "Smooth",
            Self::Flatten => "Flatten",
            Self::Set => "Set height",
            Self::Zone => "Zone (omit/include)",
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
            // Zone is not a brush; the canvas routes it to `handle_zone_input`
            // before any dab dispatch, so this arm is never reached.
            Self::Zone => {}
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

/// One reversible edit on the unified undo/redo timeline. Height strokes and
/// zone-mask edits interleave in a single history so undo walks back through
/// whatever the user did last, in order.
enum UndoEntry {
    /// A per-stroke cell snapshot (the existing height-edit path).
    Height(RectSnapshot),
    /// The zones list *before* a zone add/delete/clear. Restoring swaps it back
    /// in; touches no cells, so the terrain texture stays clean.
    Zones(Vec<Zone>),
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
    /// subdivides the field into shared-edge sub-fields stitched into one save.
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
    /// Eyedropper height-picker mode (spec §3). When on, a primary click on the
    /// canvas samples the hovered cell's height (meters) and routes it by the
    /// held modifier (plain → target, Alt → floor, Ctrl → omit) instead of
    /// laying a brush dab — pick mode SUPPRESSES brush painting so the two never
    /// fire on the same click. Toggled from the Export panel; default off.
    pick_mode: bool,
    /// The last height sampled by the eyedropper (meters above floor), shown as a
    /// small on-canvas readout. `None` until the first pick.
    last_pick: Option<f32>,
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
            pick_mode: false,
            last_pick: None,
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

    /// Store an eyedropper sample (meters) into the level its [`PickTarget`]
    /// selects (spec §3): the Flatten/Set target, the floor level, or the omit
    /// level. Also records it as the `last_pick` for the on-canvas readout. Pure
    /// state mutation (no egui), so the routing is unit-testable directly.
    fn apply_pick(&mut self, target: PickTarget, meters: f32) {
        match target {
            PickTarget::Target => self.target_height = meters,
            PickTarget::Floor => self.floor_level_m = meters,
            PickTarget::Omit => self.omit_below_m = meters,
        }
        self.last_pick = Some(meters);
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

    draw_tool_section(state, ui);
    ui.add_space(8.0);
    // The Zone tool draws loops, not heights — show its mask controls instead of
    // the brush radius/strength/falloff.
    if state.tool == SculptTool::Zone {
        draw_zone_section(state, ui);
    } else {
        draw_brush_section(state, ui);
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
            "Large canvases are intended to be tiled at export — see “Tile this \
             export” in the Export panel.",
        );
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
                    // Leaving the Zone tool discards any in-progress polygon draft
                    // so it can't silently become the prefix of the next loop on
                    // return (the draft is invisible + un-cancellable off-tool).
                    if t != SculptTool::Zone {
                        state.zone_draft.clear();
                    }
                    state.tool = t;
                }
            }
        });
    if matches!(state.tool, SculptTool::Flatten | SculptTool::Set) {
        ui.horizontal(|ui| {
            ui.label("Target height (m)");
            modifier_drag(ui, &mut state.target_height, 0.5, 0.0..=100_000.0);
        });
    }
}

fn draw_brush_section(state: &mut SculptState, ui: &mut Ui) {
    ui.label("Brush");
    // Radius/strength keep the slider track but route the value-box drag through
    // the F2 modifier scaling (spec §2 / DoD §8): Ctrl ⇒ fine, Alt ⇒ coarse.
    modifier_slider(ui, &mut state.brush.radius_cells, 1.0..=200.0, 0.2, "radius (cells)", true);
    let strength_range = if state.tool.strength_is_blend() {
        0.0..=1.0
    } else {
        0.0..=200.0
    };
    // Blend tools live in 0..=1 (fine base step); meter tools in 0..=200.
    let strength_base = if state.tool.strength_is_blend() { 0.005 } else { 0.2 };
    modifier_slider(ui, &mut state.brush.strength, strength_range, strength_base, "strength", false);
    ui.horizontal(|ui| {
        ui.label("Falloff");
        ui.selectable_value(&mut state.brush.falloff, Falloff::Smoothstep, "Smooth");
        ui.selectable_value(&mut state.brush.falloff, Falloff::Linear, "Linear");
        ui.selectable_value(&mut state.brush.falloff, Falloff::Constant, "Hard");
    });
}

fn draw_zone_section(state: &mut SculptState, ui: &mut Ui) {
    ui.label("Zone mask (XY cookie-cutter)");
    ui.horizontal(|ui| {
        ui.label("Mode");
        ui.selectable_value(&mut state.zone_mode, ZoneMode::Omit, "🚫 Omit");
        ui.selectable_value(&mut state.zone_mode, ZoneMode::Include, "✅ Include");
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
                ZoneMode::Omit => ("🚫", "Omit"),
                ZoneMode::Include => ("✅", "Include"),
            };
            ui.label(format!("{icon} {name} · {} pts", z.polygon.len()));
            if ui.small_button("✖").on_hover_text("Delete zone").clicked() {
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

/// Tiles along one axis for a `tile_cells`-cell grid-tiled export: shared-edge
/// sub-fields step by `tile_cells` cells, so `ceil(extent / tile_cells)` tiles
/// (always ≥ 1). Mirrors `convert::tile_bounds`'s count without allocating —
/// the single source of truth for the readout's tile figure.
fn tiles_on_axis(extent: u32, tile_cells: u32) -> u32 {
    extent.max(1).div_ceil(tile_cells.max(1))
}

/// Compute the grid-tiled estimate (spec §1 + §5): a `w × h` field split into
/// `ceil(w/tile)·ceil(h/tile)` shared-edge sub-fields stitched into ONE save.
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

    // Aggregate brick ceiling: ≤ 1 brick / field cell (the shared edge cells add
    // a thin duplicate seam, conservatively folded into the per-tile ceiling
    // below). The field's own cell count bounds the union from below; the tiled
    // sum (with shared edges) is the honest aggregate the stitch produces.
    let body = u64::from(tile_cells.max(1)) + 1; // shared-edge tile width on an axis
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
    let dims = state.field.as_ref().map_or((0, 0), |f| (f.width, f.height));
    let cells = u64::from(dims.0) * u64::from(dims.1);
    // RAM is read from a coarse-cadence cache (see refresh_available_ram), not the
    // raw /proc/meminfo scan every frame — it only gates the button + a GiB
    // readout, so sub-second staleness is fine.
    refresh_available_ram(state, ui);
    let available_ram = state.available_ram;
    // The estimate (and the button gate) follows the SAME path the convert will
    // take: the grid math when tiling (tile count + per-tile peak + aggregate
    // cap), the cheap single-mesh figure otherwise.
    let estimate = if state.tile_export {
        tiled_estimate(dims.0, dims.1, state.tile_cells, available_ram)
    } else {
        single_mesh_estimate(cells, available_ram)
    };

    egui::CollapsingHeader::new("Export").default_open(true).show(ui, |ui| {
        // ---- Scale (drives FieldMeta) ----
        ui.label("Scale");
        ui.horizontal(|ui| {
            ui.label("Studs / meter");
            modifier_drag(ui, &mut state.studs_per_meter, 0.1, 0.01..=64.0);
        });
        ui.horizontal(|ui| {
            ui.label("Vertical exaggeration");
            modifier_drag(ui, &mut state.vertical_exaggeration, 0.1, 0.1..=64.0);
        });
        draw_scale_readout(state, ui);
        ui.checkbox(&mut state.micro, "Micro bricks (fine detail)").on_hover_text(
            "On: SmoothTile → Micro — ~5× finer cells at the same physical scale, for crisp \
             relief detail. Off: standard SmoothTile bricks.",
        );

        ui.separator();
        // ---- Output name + formats ----
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

        ui.separator();
        // ---- Floor / omit (meter-space, drive the convert) ----
        ui.horizontal(|ui| {
            ui.label("Floor level (m)");
            modifier_drag(ui, &mut state.floor_level_m, 0.5, 0.0..=100_000.0);
        });
        ui.horizontal(|ui| {
            ui.label("Omit below (m)");
            modifier_drag(ui, &mut state.omit_below_m, 0.5, 0.0..=100_000.0);
        })
        .response
        .on_hover_text(
            "Columns whose source height is at or below this level emit no bricks (native \
             floor / 'omit water'). Default 0 drops only true-floor columns.",
        );
        // ---- Eyedropper height-picker (spec §3) ----
        ui.checkbox(&mut state.pick_mode, "🔬 Pick height (eyedropper)").on_hover_text(
            "On: click the canvas to sample the hovered cell's height (meters) instead of \
             painting. The held modifier routes the sample — plain → Target height, Alt → \
             Floor level, Ctrl → Omit below. The sampled value shows on the canvas.",
        );
        if let Some(m) = state.last_pick {
            ui.small(format!("last sample: {m:.2} m"));
        }

        ui.separator();
        // ---- Manual tiling (toggle + size) ----
        ui.checkbox(&mut state.tile_export, "Tile this export").on_hover_text(
            "Off: one single mesh. On: subdivide the field into grid-tiled sub-exports \
             stitched into one save.",
        );
        if state.tile_export {
            ui.horizontal(|ui| {
                ui.label("Tile size (cells)");
                modifier_drag(ui, &mut state.tile_cells, 8.0, 16..=4096);
            });
        }

        ui.separator();
        // ---- Live estimate ----
        draw_estimate_readout(state, ui, estimate);
    });

    estimate
}

/// Live "≈ N studs/m · relief · ~M m/cell" readout (spec §1), mirroring the Map
/// tab's `draw_output_estimate`. The achieved studs/m is the integer-`derive_scale`
/// realized value (may differ slightly from the set target), so the user sees the
/// true scale the convert will build at — including the cap when a coarse cell
/// can't reach the requested studs/m.
fn draw_scale_readout(state: &SculptState, ui: &mut Ui) {
    let Some(field) = state.field.as_ref() else { return };
    let cell_m = field.meta.cell_m;
    if !cell_m.is_finite() || cell_m <= 0.0 {
        return;
    }
    let (hscale, vertical) = build_derive_scale(
        cell_m,
        state.studs_per_meter,
        state.vertical_exaggeration,
        state.micro,
    );
    let upf = if state.micro { 1.0 } else { 5.0 };
    // studs/m = 2*hscale*upf / cell_m / 5 (BrickSize half-extent → 2*size pitch).
    let achieved = 2.0 * f64::from(hscale) * upf / cell_m / 5.0;
    let relief = if (state.vertical_exaggeration - 1.0).abs() < 0.05 {
        "1:1".to_owned()
    } else {
        format!("×{:.1}", state.vertical_exaggeration)
    };
    ui.small(format!(
        "≈ {achieved:.1} studs/m · relief {relief} · ~{cell_m:.0} m/cell · vert {vertical:.1} u/m"
    ));
}

/// `derive_scale` lives on the map tab; thin re-export shim so the scale readout
/// reads the SAME integer-brick scale the convert derives (single source of truth).
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
        // The cap differs by path: a single mesh trips MAX_BRICKS (the remedy is to
        // tile); a tiled stitch trips the aggregate MAX_GRID_BRICKS (the remedy is
        // a smaller canvas/scale — tiling can't grow past the stitch ceiling).
        let (cap, remedy) = if state.tile_export {
            (build::MAX_GRID_BRICKS, "shrink the canvas or lower the scale")
        } else {
            (build::MAX_BRICKS, "enable 'Tile this export' or shrink the canvas")
        };
        ui.colored_label(STATUS_WARN_FG, format!("Over the {cap}-brick cap — {remedy}."));
    } else if !est.fits_ram {
        ui.colored_label(
            STATUS_WARN_FG,
            "Estimated peak RAM exceeds what's free — shrink the canvas, lower the scale, or \
             tile the export.",
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
        if let Some(dest) = &outcome.installed_path {
            ui.small(format!("installed → {}", dest.display()));
        } else {
            ui.small(format!("wrote → {}", outcome.brdb_path.display()));
        }
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
    handle_view_input(state, &response, ui, rect);

    // Paint the terrain texture at the current pan/zoom.
    paint_terrain(state, ui, rect, fw, fh);

    if state.tool == SculptTool::Zone {
        // Zone tool: the primary button draws loops (no dabs, no brush ring).
        handle_zone_input(state, &response, rect);
    } else {
        // Apply sculpt dabs from a primary-button drag.
        handle_sculpt_input(state, &response, fw, fh);

        // Brush overlay + smooth radius animation. Animating every frame while the
        // pointer is over the canvas keeps the resize buttery regardless of grid
        // size (it touches no texture). Suppressed in eyedropper pick mode: a pick
        // samples a SINGLE cell, so a radius ring would imply area painting that
        // does not happen — the pick readout is the cursor UI then.
        if !state.pick_mode {
            paint_brush_overlay(state, ctx, ui, &response, rect);
        }
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
/// a small label in the canvas's top-left shows the last sampled height (meters)
/// and the live hovered cell's height, so the value is visible without looking
/// at the side panel. No-op when pick mode is off.
fn paint_pick_readout(state: &SculptState, ui: &Ui, response: &egui::Response, rect: Rect) {
    if !state.pick_mode {
        return;
    }
    let Some(field) = state.field.as_ref() else { return };
    let mut text = match state.last_pick {
        Some(m) => format!("eyedropper · last {m:.2} m"),
        None => "eyedropper · click to sample".to_owned(),
    };
    if response.hovered()
        && let Some(ptr) = response.hover_pos()
    {
        let (cx, cy) = screen_to_cell(state, ptr);
        text = format!("{text}  ·  hover {:.2} m", field.sample_cell_meters(cx, cy));
    }
    let painter = ui.painter_at(rect);
    let pos = rect.min + Vec2::new(8.0, 8.0);
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
    // Eyedropper pick mode SUPPRESSES brush painting (spec §3): a primary click
    // samples the hovered cell and routes it, then returns so no dab is laid —
    // the two never fire on the same click. Reuses the SAME map `Response`
    // hit-test (no second interact widget).
    if state.pick_mode {
        handle_pick_input(state, response);
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

/// Eyedropper pick (spec §3): on a primary click, hit-test the pointer to a cell
/// via the SAME view transform the brush uses (`screen_to_cell`), sample that
/// cell's height in meters, and route it by the modifier held at click time
/// (plain → target, Alt → floor, Ctrl → omit). Only `clicked_by(Primary)` fires
/// — a middle/secondary drag still pans, and no brush dab is laid (the caller
/// returned before the sculpt path).
fn handle_pick_input(state: &mut SculptState, response: &egui::Response) {
    if !response.clicked_by(egui::PointerButton::Primary) {
        return;
    }
    let Some(ptr) = response.interact_pointer_pos() else { return };
    let mods = response.ctx.input(|i| i.modifiers);
    let target = pick_target(DragModifiers { ctrl: mods.ctrl, alt: mods.alt });
    let (cx, cy) = screen_to_cell(state, ptr);
    let Some(field) = state.field.as_ref() else { return };
    let meters = field.sample_cell_meters(cx, cy);
    state.apply_pick(target, meters);
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
    let mut meta = blank_meta(state, state.new_cell_m);
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
    // Clone the zones into the worker alongside the field. Converting READS them
    // (rasterized to a keep-mask) but never clears `state.zones` — they persist
    // so the user can re-export, refine, or clear explicitly.
    let zones = state.zones.clone();
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
            // stitched save built from shared-edge sub-fields); off = single mesh.
            let result = if tile_export {
                convert_heightfield_tiled(
                    &field, out, tile_cells, floor_level_m, omit_below_m, &zones, progress_fn,
                    cancel_arc,
                )
            } else {
                convert_heightfield(
                    &field, out, floor_level_m, omit_below_m, &zones, progress_fn, cancel_arc,
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

    /// Eyedropper modifier routing (spec §3): plain ⇒ target, Alt ⇒ floor,
    /// Ctrl ⇒ omit, and BOTH held ⇒ Ctrl wins (omit) — the documented precedence
    /// shared with `modifier_step`. Pure routing, no egui context.
    #[test]
    fn pick_target_modifier_routing() {
        assert_eq!(
            pick_target(DragModifiers { ctrl: false, alt: false }),
            PickTarget::Target,
            "plain click must route to the target height",
        );
        assert_eq!(
            pick_target(DragModifiers { ctrl: false, alt: true }),
            PickTarget::Floor,
            "Alt must route to the floor level",
        );
        assert_eq!(
            pick_target(DragModifiers { ctrl: true, alt: false }),
            PickTarget::Omit,
            "Ctrl must route to the omit level",
        );
        assert_eq!(
            pick_target(DragModifiers { ctrl: true, alt: true }),
            PickTarget::Omit,
            "Ctrl+Alt: Ctrl wins (omit) — documented precedence",
        );
    }

    /// Eyedropper end-to-end on real state (spec §3): a pick at a cell samples
    /// that cell's height in meters and writes it into the level the modifier
    /// routes to, recording it as `last_pick`. Drives `apply_pick` with the
    /// sampler the canvas handler uses, so the click → sample → route path is
    /// exercised without an egui pointer event.
    #[test]
    fn eyedropper_samples_cell_meters() {
        let mut state = SculptState::new();
        // A field with distinct, known cell heights (meters above floor).
        let mut field = HeightField::flat(4, 4, meta());
        field.set(2, 1, 42.5);
        field.set(0, 3, 7.0);
        state.set_field(field);

        // The sampler the canvas handler calls returns the hovered cell's meters.
        let f = state.field.as_ref().unwrap();
        assert_eq!(f.sample_cell_meters(2.0, 1.0), 42.5);
        // A fractional pointer inside the same cell floors to it (nearest-cell).
        assert_eq!(f.sample_cell_meters(2.9, 1.1), 42.5);

        // Plain click → target_height.
        let m = state.field.as_ref().unwrap().sample_cell_meters(2.0, 1.0);
        state.apply_pick(pick_target(DragModifiers { ctrl: false, alt: false }), m);
        assert_eq!(state.target_height, 42.5, "plain pick must set the target height");
        assert_eq!(state.last_pick, Some(42.5), "the sample must be recorded for the readout");

        // Alt click → floor_level_m.
        let m = state.field.as_ref().unwrap().sample_cell_meters(0.0, 3.0);
        state.apply_pick(pick_target(DragModifiers { ctrl: false, alt: true }), m);
        assert_eq!(state.floor_level_m, 7.0, "Alt pick must set the floor level");

        // Ctrl click → omit_below_m.
        let m = state.field.as_ref().unwrap().sample_cell_meters(2.0, 1.0);
        state.apply_pick(pick_target(DragModifiers { ctrl: true, alt: false }), m);
        assert_eq!(state.omit_below_m, 42.5, "Ctrl pick must set the omit-below level");
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
