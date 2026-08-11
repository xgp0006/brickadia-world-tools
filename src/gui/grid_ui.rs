//! Grid-build UI (all egui). The single-box "Fetch & Build" flow is untouched
//! when grid mode is OFF (the default): everything here lives behind a collapsing
//! "Grid build (large worlds)" header gated on [`GridUiState::enabled`].
//!
//! The three modes (spec §8) are pure front-ends that reduce to one [`GridPlan`]:
//!
//! * **Mode 1 AutoSubdivide** — `tile_m` DragValue + a live N×M @z readout.
//! * **Mode 2 ClickMask** — paint the lattice over the map and toggle tile
//!   membership by hit-testing the SINGLE map `Response` in [`update_grid_pick`]
//!   (which runs ONLY when `enabled && Overlay && !draw_mode`, honoring map_tab's
//!   single-Response warning — NO second interact widget).
//! * **Mode 3 Explicit** — numeric cols/rows + an anchor combo.
//!
//! Output sinks ([`super::grid::OutputOptions`]) + the pre-commit estimate
//! ([`super::grid::estimate_grid`]) gate a distinct "Grid Build" button that
//! spawns the worker exactly like `start_fetch` (thread + `Promise` +
//! `Arc<Mutex<GridProgress>>` + `Arc<AtomicBool>` cancel).

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use egui::{Color32, Rect, Stroke, StrokeKind, Ui, Vec2};
use poll_promise::Promise;
use walkers::{Projector, lat_lon};

use super::build::BlockType;
use super::config::Config;
use super::dem_sources::DemSource;
use super::grid::{
    AnchorKind, GridError, GridMode, GridOutcome, GridPlan, GridProgress, GridSettings,
    OutputOptions, TileId, available_ram_bytes, estimate_grid, partition, run_grid_build,
};
use super::imagery_sources::ImagerySource;
use super::theme::{BBOX_FILL, BBOX_STROKE, STATUS_ERROR_FG, STATUS_WARN_FG};
use super::tiles::BBoxLatLon;

/// Which of the three grid-definition modes is active (spec §8). Stored as a
/// flat tag plus per-mode parameter fields so the UI can edit each mode's inputs
/// without losing the others; [`GridUiState::mode`] reassembles the real
/// [`GridMode`] from the active tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GridModeKind {
    AutoSubdivide,
    ClickMask,
    Explicit,
}

impl GridModeKind {
    const ALL: [Self; 3] = [Self::AutoSubdivide, Self::ClickMask, Self::Explicit];

    const fn label(self) -> &'static str {
        match self {
            Self::AutoSubdivide => "Auto-subdivide",
            Self::ClickMask => "Click mask (overlay)",
            Self::Explicit => "Explicit N×M",
        }
    }
}

/// Tile-side bounds for the AutoSubdivide / ClickMask `tile_m` DragValue. The
/// spec constrains true-full-res tiles to ~250–1100 m (BINDING CORRECTION #2);
/// the UI allows up to 4000 m per the task brief but the estimate's
/// `over_cell_budget` gate rejects a too-big tile before launch.
const TILE_M_MIN: f64 = 250.0;
const TILE_M_MAX: f64 = 4000.0;
const TILE_M_DEFAULT: f64 = 1000.0;

/// Explicit-mode cols/rows bounds (spec §8).
const EXPLICIT_DIM_MIN: u32 = 1;
const EXPLICIT_DIM_MAX: u32 = 32;

/// All grid-mode UI + worker state, embedded as one field in `MapTabState` so the
/// single-box surface stays unchanged when grid mode is off. Every field defaults
/// to the legacy-off configuration in [`GridUiState::new`].
pub(crate) struct GridUiState {
    /// Master gate. When false the section collapses to its header and NONE of
    /// the per-frame grid work (recompute, overlay paint, pick) runs.
    pub(crate) enabled: bool,
    mode_kind: GridModeKind,
    tile_m: f64,
    /// Excluded tiles (ClickMask). Store EXCLUDED so the default is all-in.
    excluded: HashSet<TileId>,
    explicit_cols: u32,
    explicit_rows: u32,
    explicit_anchor: AnchorKind,
    output: OutputOptions,
    /// Cheap recomputed plan for the current bbox + mode (spec §8). `None` until
    /// a bbox exists; rebuilt only when an input changed (`recompute_grid_plan`).
    plan: Option<GridPlan>,
    /// Signature of the inputs that produced `plan`. The cheap change-guard:
    /// `recompute_grid_plan` skips the `partition` rebuild when the freshly
    /// computed signature matches this (spec §8: a steady frame does no work).
    plan_sig: Option<PlanSig>,
    /// The estimate window is open between "Grid Build" click and confirm/cancel.
    estimate_open: bool,
    /// Worker plumbing — mirrors the single-box `fetch_*` fields exactly.
    promise: Option<Promise<Result<GridOutcome, GridError>>>,
    progress: Arc<Mutex<GridProgress>>,
    pub(crate) cancel: Arc<AtomicBool>,
    last_outcome: Option<GridOutcome>,
    last_error: Option<String>,
}

impl GridUiState {
    pub(crate) fn new() -> Self {
        Self {
            enabled: false,
            mode_kind: GridModeKind::AutoSubdivide,
            tile_m: TILE_M_DEFAULT,
            excluded: HashSet::new(),
            explicit_cols: 2,
            explicit_rows: 2,
            explicit_anchor: AnchorKind::NwCorner,
            output: OutputOptions::default(),
            plan: None,
            plan_sig: None,
            estimate_open: false,
            promise: None,
            progress: Arc::new(Mutex::new(GridProgress::new(0))),
            cancel: Arc::new(AtomicBool::new(false)),
            last_outcome: None,
            last_error: None,
        }
    }

    fn is_running(&self) -> bool {
        self.promise.is_some()
    }

    /// Reassemble the real [`GridMode`] from the active tag + per-mode fields.
    fn mode(&self) -> GridMode {
        match self.mode_kind {
            GridModeKind::AutoSubdivide => GridMode::AutoSubdivide { tile_m: self.tile_m },
            GridModeKind::ClickMask => GridMode::ClickMask {
                tile_m: self.tile_m,
                excluded: self.excluded.clone(),
            },
            GridModeKind::Explicit => GridMode::Explicit {
                tile_m: self.tile_m,
                cols: self.explicit_cols,
                rows: self.explicit_rows,
                anchor: self.explicit_anchor,
            },
        }
    }
}

/// Per-frame inputs the grid section reads from `MapTabState` (the single-box
/// shaping fields) without grid_ui touching `MapTabState`'s private internals.
/// `bbox` is `None` until the user draws one; the section then prompts for it.
pub(crate) struct GridInputs<'a> {
    pub(crate) bbox: Option<BBoxLatLon>,
    pub(crate) dem_source: DemSource,
    pub(crate) imagery_source: ImagerySource,
    pub(crate) block_type: BlockType,
    pub(crate) glow: bool,
    pub(crate) no_collision: bool,
    pub(crate) overwrite: bool,
    pub(crate) studs_per_meter: f32,
    pub(crate) vertical_exaggeration: f32,
    pub(crate) output_name: &'a str,
    pub(crate) config: &'a Config,
}

/// The DEM source's resolution ceiling (the forced grid zoom is a centroid probe
/// through `pick_zoom` clamped to this). Mercator tile sources expose
/// `max_zoom()`; OpenTopography is a single-shot geographic REST API with no tile
/// pyramid, so the spec forces z15 (and grid mode WARNS against it — §9).
fn source_max_zoom_for(dem: DemSource, token: Option<&str>) -> u32 {
    match dem {
        DemSource::OpenTopography | DemSource::OpenTopographyCop30 => 15,
        src => super::dem_sources::tile_source_for(src, token).map_or(15, |s| s.max_zoom()),
    }
}

/// Build the [`GridSettings`] for the current inputs (the non-bbox, non-name
/// shaping fields stamped into every tile, spec §2). The output sinks are owned
/// by the grid state, not the inputs, so they are passed explicitly. Clones the
/// token Strings — call ONLY at launch, not every repaint (spec correction #7).
fn grid_settings(inputs: &GridInputs, output: OutputOptions) -> GridSettings {
    GridSettings {
        dem_source: inputs.dem_source,
        imagery_source: inputs.imagery_source,
        mapbox_token: inputs.config.mapbox_token.clone(),
        opentopo_key: inputs.config.opentopo_api_key.clone(),
        block_type: inputs.block_type,
        glow: inputs.glow,
        no_collision: inputs.no_collision,
        output,
        overwrite: inputs.overwrite,
    }
}

/// A token-free [`GridSettings`] for `estimate_grid` (which reads only
/// `imagery_source` + `output.stitched`). Avoids cloning the `mapbox_token` /
/// `opentopo_key` Strings every repaint while the estimate window is open (spec
/// correction #7) — the tokens are NOT consulted by the pure estimate.
fn estimate_settings(inputs: &GridInputs, output: OutputOptions) -> GridSettings {
    GridSettings {
        dem_source: inputs.dem_source,
        imagery_source: inputs.imagery_source,
        mapbox_token: None,
        opentopo_key: None,
        block_type: inputs.block_type,
        glow: inputs.glow,
        no_collision: inputs.no_collision,
        output,
        overwrite: inputs.overwrite,
    }
}

/// A small, `Copy` digest of every input `recompute_grid_plan` feeds to
/// `partition` (plus the post-partition `name`). Floats are compared by their bit
/// pattern (no float-eq); the `ClickMask` `excluded` set and the output name are
/// folded to a single `u64` each so the signature stays fixed-size and cheap to
/// build + compare per frame. Two equal `PlanSig`s ⇒ `partition` would produce
/// the identical plan, so the rebuild is skipped (spec §8 cheap-guard).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PlanSig {
    north: u64,
    south: u64,
    east: u64,
    west: u64,
    mode_kind: GridModeKind,
    tile_m: u64,
    excluded_fold: u64,
    explicit_cols: u32,
    explicit_rows: u32,
    explicit_anchor: AnchorKind,
    dem_source: DemSource,
    studs_per_meter: u32,
    vertical_exaggeration: u32,
    micro: bool,
    name_hash: u64,
}

impl PlanSig {
    /// Order-independent fold of the `ClickMask` excluded set (zero in the other
    /// modes, which ignore it): XOR each `TileId`'s hash so add/remove flips the
    /// fold, then mix in the count so a removed id that re-XORs to the same value
    /// is still distinguished. Avoids cloning the `HashSet` every frame.
    fn excluded_fold(grid: &GridUiState) -> u64 {
        if grid.mode_kind != GridModeKind::ClickMask {
            return 0;
        }
        let mut fold: u64 = 0;
        for id in &grid.excluded {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            id.hash(&mut h);
            fold ^= h.finish();
        }
        fold.wrapping_add(grid.excluded.len() as u64)
    }

    fn of(grid: &GridUiState, inputs: &GridInputs, bbox: BBoxLatLon) -> Self {
        let mut name_hasher = std::collections::hash_map::DefaultHasher::new();
        inputs.output_name.hash(&mut name_hasher);
        Self {
            north: bbox.north.to_bits(),
            south: bbox.south.to_bits(),
            east: bbox.east.to_bits(),
            west: bbox.west.to_bits(),
            mode_kind: grid.mode_kind,
            tile_m: grid.tile_m.to_bits(),
            excluded_fold: Self::excluded_fold(grid),
            explicit_cols: grid.explicit_cols,
            explicit_rows: grid.explicit_rows,
            explicit_anchor: grid.explicit_anchor,
            dem_source: inputs.dem_source,
            studs_per_meter: inputs.studs_per_meter.to_bits(),
            vertical_exaggeration: inputs.vertical_exaggeration.to_bits(),
            micro: inputs.block_type.micro(),
            name_hash: name_hasher.finish(),
        }
    }
}

/// Rebuild the cheap plan for the current bbox + mode, but only when an input
/// that affects it actually changed (spec §8: `recompute_grid_plan` runs each
/// frame). Cheap-guard: compare a small fixed-size [`PlanSig`] of the inputs
/// against the one that produced the cached plan so a steady frame does no
/// `partition` work (no edge-lattice / prefix-sum / tiles-Vec / String allocs).
/// Clears the plan when there is no bbox.
pub(crate) fn recompute_grid_plan(grid: &mut GridUiState, inputs: &GridInputs) {
    let Some(bbox) = inputs.bbox else {
        grid.plan = None;
        grid.plan_sig = None;
        return;
    };
    let sig = PlanSig::of(grid, inputs, bbox);
    if grid.plan.is_some() && grid.plan_sig == Some(sig) {
        return; // Inputs unchanged since the cached plan — skip the rebuild.
    }
    let token = inputs.config.mapbox_token.as_deref();
    let src_max_zoom = source_max_zoom_for(inputs.dem_source, token);
    let mut plan = partition(
        bbox,
        grid.mode(),
        inputs.dem_source,
        src_max_zoom,
        inputs.studs_per_meter,
        inputs.vertical_exaggeration,
        inputs.block_type.micro(),
    );
    plan.name = inputs.output_name.to_owned();
    grid.plan = Some(plan);
    grid.plan_sig = Some(sig);
}

/// Draw the whole grid section under a collapsing header. Returns nothing; all
/// state lives in `grid`. Recomputes the plan first (cheap), then renders the
/// mode controls, output options, the (poll/progress) worker UI, and the
/// estimate window when open. The single-box flow is visually unchanged when
/// `grid.enabled` is false — only the bare collapsing header shows.
pub(crate) fn draw_grid_section(grid: &mut GridUiState, ui: &mut Ui, inputs: &GridInputs) {
    poll_grid_promise(grid);
    egui::CollapsingHeader::new("Grid build (large worlds)")
        .id_salt("grid_build_section")
        .default_open(false)
        .show(ui, |ui| {
            ui.checkbox(&mut grid.enabled, "Enable grid (tiled) build")
                .on_hover_text(
                    "Subdivide a large area into tiles, mesh each at full resolution, and \
                     combine into one or more Brickadia worlds — for areas too big for a \
                     single mesh. Leave OFF for the normal single-box Fetch & Build.",
                );
            if !grid.enabled {
                return;
            }
            ui.add_space(6.0);
            recompute_grid_plan(grid, inputs);
            draw_mode_picker(grid, ui);
            ui.add_space(6.0);
            draw_mode_controls(grid, ui, inputs);
            ui.add_space(8.0);
            draw_output_options(&mut grid.output, ui);
            ui.add_space(8.0);
            if grid.is_running() {
                draw_grid_in_progress(grid, ui);
            } else {
                draw_grid_build_button(grid, ui, inputs);
            }
            ui.add_space(6.0);
            draw_grid_last_result(grid, ui);
        });
}

fn draw_mode_picker(grid: &mut GridUiState, ui: &mut Ui) {
    ui.label("Grid mode");
    egui::ComboBox::from_id_salt("grid_mode_combo")
        .selected_text(grid.mode_kind.label())
        .width(260.0)
        .show_ui(ui, |ui| {
            for kind in GridModeKind::ALL {
                if ui
                    .add(egui::Button::selectable(grid.mode_kind == kind, kind.label()))
                    .clicked()
                {
                    grid.mode_kind = kind;
                }
            }
        });
}

fn draw_mode_controls(grid: &mut GridUiState, ui: &mut Ui, inputs: &GridInputs) {
    match grid.mode_kind {
        GridModeKind::AutoSubdivide => draw_auto_controls(grid, ui),
        GridModeKind::ClickMask => draw_clickmask_controls(grid, ui),
        GridModeKind::Explicit => draw_explicit_controls(grid, ui),
    }
    if inputs.bbox.is_none() {
        ui.colored_label(
            STATUS_WARN_FG,
            "Draw a bounding box first — the grid subdivides it.",
        );
    }
    draw_grid_readout(grid, ui, inputs);
}

fn draw_auto_controls(grid: &mut GridUiState, ui: &mut Ui) {
    ui.label("Tile size (meters per tile)");
    ui.add(
        egui::DragValue::new(&mut grid.tile_m)
            .range(TILE_M_MIN..=TILE_M_MAX)
            .speed(10.0)
            .suffix(" m"),
    )
    .on_hover_text(
        "Each tile meshes at full resolution. Smaller tiles = more tiles but less \
         peak RAM per tile; ~1 km is a good default. Tiles too large for the \
         per-tile cell budget are rejected by the estimate before launch.",
    );
}

fn draw_clickmask_controls(grid: &mut GridUiState, ui: &mut Ui) {
    draw_auto_controls(grid, ui);
    ui.add_space(4.0);
    ui.small(
        "Click tiles on the map to exclude/include them (Draw Box must be OFF). \
         Excluded tiles are skipped — the rest keep their exact world offsets.",
    );
    let total = grid.plan.as_ref().map_or(0, |p| (p.rows * p.cols) as usize);
    ui.horizontal(|ui| {
        if ui.button("All").clicked() {
            grid.excluded.clear();
        }
        if ui.add_enabled(total > 0, egui::Button::new("None")).clicked()
            && let Some(plan) = &grid.plan
        {
            grid.excluded.clear();
            for r in 0..plan.rows {
                for c in 0..plan.cols {
                    grid.excluded.insert(TileId { row: r, col: c });
                }
            }
        }
        if ui.add_enabled(total > 0, egui::Button::new("Invert")).clicked()
            && let Some(plan) = &grid.plan
        {
            let mut next = HashSet::new();
            for r in 0..plan.rows {
                for c in 0..plan.cols {
                    let id = TileId { row: r, col: c };
                    if !grid.excluded.contains(&id) {
                        next.insert(id);
                    }
                }
            }
            grid.excluded = next;
        }
    });
    if total > 0 {
        let included = total - grid.excluded.len().min(total);
        ui.small(format!("{included}/{total} tiles included"));
    }
}

fn draw_explicit_controls(grid: &mut GridUiState, ui: &mut Ui) {
    draw_auto_controls(grid, ui);
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label("Columns");
        ui.add(
            egui::DragValue::new(&mut grid.explicit_cols)
                .range(EXPLICIT_DIM_MIN..=EXPLICIT_DIM_MAX)
                .speed(0.1),
        );
        ui.label("Rows");
        ui.add(
            egui::DragValue::new(&mut grid.explicit_rows)
                .range(EXPLICIT_DIM_MIN..=EXPLICIT_DIM_MAX)
                .speed(0.1),
        );
    });
    ui.horizontal(|ui| {
        ui.label("Anchor");
        egui::ComboBox::from_id_salt("grid_anchor_combo")
            .selected_text(anchor_label(grid.explicit_anchor))
            .show_ui(ui, |ui| {
                for anchor in [AnchorKind::NwCorner, AnchorKind::Center] {
                    if ui
                        .add(egui::Button::selectable(
                            grid.explicit_anchor == anchor,
                            anchor_label(anchor),
                        ))
                        .clicked()
                    {
                        grid.explicit_anchor = anchor;
                    }
                }
            });
    });
}

const fn anchor_label(anchor: AnchorKind) -> &'static str {
    match anchor {
        AnchorKind::NwCorner => "NW corner of box",
        AnchorKind::Center => "Center of box",
    }
}

/// Live "N×M = T tiles @ z{zoom}, ~{cell_m} m/cell" readout (spec §8 Mode 1).
fn draw_grid_readout(grid: &GridUiState, ui: &mut Ui, inputs: &GridInputs) {
    let Some(plan) = &grid.plan else { return };
    let tiles = plan.tiles.len();
    ui.small(format!(
        "{}×{} = {} tile{} @ z{}, ~{:.0} m/cell",
        plan.cols,
        plan.rows,
        tiles,
        if tiles == 1 { "" } else { "s" },
        plan.zoom,
        plan.cell_m,
    ));
    if matches!(
        inputs.dem_source,
        DemSource::OpenTopography | DemSource::OpenTopographyCop30
    ) {
        ui.colored_label(
            STATUS_WARN_FG,
            "OpenTopography is geographic (non-Mercator) — grid tiles align at seams \
             but the SHAPE is aspect-distorted. Use AWS Terrarium or Mapbox for a \
             faithful grid; the per-day REST quota can also bite a large grid.",
        );
    }
}

/// Output sinks: format(s) × layout(s) × install (spec §7). Each axis needs ≥1
/// selection; the estimate/build gate surfaces `validate()` if not.
pub(crate) fn draw_output_options(opts: &mut OutputOptions, ui: &mut Ui) {
    ui.label("Output");
    ui.horizontal(|ui| {
        ui.label("Formats:");
        ui.checkbox(&mut opts.brdb, ".brdb")
            .on_hover_text("Brickadia world — installs into Worlds/.");
        ui.checkbox(&mut opts.brz, ".brz")
            .on_hover_text("Brickadia prefab — written to the builds folder for manual import.");
    });
    ui.horizontal(|ui| {
        ui.label("Layout:");
        ui.checkbox(&mut opts.stitched, "Stitched")
            .on_hover_text("One combined world with every tile at its world offset.");
        ui.checkbox(&mut opts.individual, "Individual")
            .on_hover_text(
                "One file per tile, pre-offset to its world position (bounded RAM — the \
                 only path that fits very large grids).",
            );
    });
    ui.checkbox(&mut opts.install_to_brickadia, "Install into Brickadia")
        .on_hover_text("Copy .brdb output into the Brickadia Worlds/ folder (.brz stays in builds).");
    if let Err(e) = opts.validate() {
        ui.colored_label(STATUS_WARN_FG, format!("• {e}"));
    }
}

/// The distinct "Grid Build" button (vs the single-box "Fetch & Build"). Lists
/// any blocker, then opens the estimate window which gates the actual launch.
fn draw_grid_build_button(grid: &mut GridUiState, ui: &mut Ui, inputs: &GridInputs) {
    let reasons = grid_disabled_reasons(grid, inputs);
    let enabled = reasons.is_empty();
    if !enabled {
        for reason in &reasons {
            ui.colored_label(STATUS_WARN_FG, format!("• {reason}"));
        }
    }
    let resp = ui.add_enabled(
        enabled,
        egui::Button::new("🧱  Grid Build").min_size(Vec2::new(260.0, 32.0)),
    );
    if !enabled {
        resp.on_disabled_hover_text(format!("Not ready:\n{}", reasons.join("\n")));
    } else if resp
        .on_hover_text("Review the pre-commit estimate, then build the tiled world.")
        .clicked()
    {
        grid.estimate_open = true;
    }
    draw_grid_estimate(grid, ui, inputs);
}

/// Blockers that disable the Grid Build button (mirrors `fetch_disabled_reasons`
/// style, spec §8). The estimate window adds the RAM / cell-budget gates.
fn grid_disabled_reasons(grid: &GridUiState, inputs: &GridInputs) -> Vec<String> {
    let mut reasons: Vec<String> = Vec::new();
    if inputs.bbox.is_none() {
        reasons.push("Select a bounding box first".to_owned());
    }
    if inputs.output_name.trim().is_empty() {
        reasons.push("Output name is empty".to_owned());
    }
    if let Err(e) = grid.output.validate() {
        reasons.push(e.to_string());
    }
    match &grid.plan {
        Some(plan) if plan.tiles.is_empty() => {
            reasons.push("No tiles selected (Click mask excluded them all)".to_owned());
        }
        None if inputs.bbox.is_some() => {
            reasons.push("Grid plan not ready".to_owned());
        }
        _ => {}
    }
    reasons
}

/// The pre-commit estimate window (spec §6/§8): tiles, cells, bricks, peak RAM,
/// time. The Build button inside is DISABLED with remedy text when any single
/// tile is over the cell budget / per-tile brick cap, or the plan does not fit
/// RAM. Confirming launches the worker via [`start_grid_build`].
fn draw_grid_estimate(grid: &mut GridUiState, ui: &mut Ui, inputs: &GridInputs) {
    if !grid.estimate_open {
        return;
    }
    // Borrow the plan for the estimate/display — do NOT deep-clone the whole
    // GridPlan every repaint (spec correction #7). The owned clone happens ONLY
    // in the launch arm below, where it must move into the worker thread.
    let Some(plan) = grid.plan.as_ref() else {
        grid.estimate_open = false;
        return;
    };
    // The estimate only reads `imagery_source` + `output.stitched` (no tokens),
    // so build a borrow-cheap settings view without cloning the token Strings
    // every repaint; the owned, token-carrying settings is built at launch.
    let est_settings = estimate_settings(inputs, grid.output);
    let available = available_ram_bytes().unwrap_or(0);
    let est = estimate_grid(plan, &est_settings, available);

    let mut open = grid.estimate_open;
    let mut launch = false;
    egui::Window::new("Grid build estimate")
        .id(egui::Id::new("grid_estimate_window"))
        .open(&mut open)
        .resizable(false)
        .default_width(380.0)
        .show(ui.ctx(), |ui| {
            ui.label(format!(
                "{}×{} grid · {} tile{} @ z{}",
                plan.cols,
                plan.rows,
                est.tile_count,
                if est.tile_count == 1 { "" } else { "s" },
                plan.zoom,
            ));
            ui.label(format!("~{:.0} m/cell · {} cells total", plan.cell_m, est.total_cells));
            ui.label(format!("≤ {} bricks (greedy mesh reduces this)", est.est_bricks));
            // Honest labelling (spec correction #8): `peak_mesh_bytes` is
            // `parallel_tiles * one-tile-mesh` — the FETCH/resident-tiles peak,
            // NOT N concurrent meshes (meshing is SEQUENTIAL, so the true mesh
            // peak is one tile). Label the term as the fetch-resident figure.
            ui.label(format!(
                "peak RAM ≈ {} (fetch, {} tile{} resident) + {} (write) + {} (rasters)",
                fmt_bytes(est.peak_mesh_bytes),
                est.parallel_tiles,
                if est.parallel_tiles == 1 { "" } else { "s" },
                fmt_bytes(est.est_brick_vec_bytes),
                fmt_bytes(est.est_rasters_bytes),
            ));
            if available > 0 {
                ui.small(format!("free RAM now: {}", fmt_bytes(available)));
            } else {
                ui.colored_label(STATUS_WARN_FG, "free RAM unreadable — using a conservative gate");
            }
            ui.label(format!("estimated time ~{:.0} s", est.est_seconds));
            ui.separator();

            let mut remedies: Vec<&str> = Vec::new();
            if est.over_cell_budget {
                remedies.push(
                    "A tile exceeds the per-tile cell budget — use a SMALLER tile size.",
                );
            }
            if est.over_brick_cap {
                remedies.push(
                    "A single tile exceeds the per-tile brick cap — smaller tiles or less \
                     exaggeration.",
                );
            }
            if !est.fits_ram {
                remedies.push(
                    "Not enough free RAM for a stitched build — use keep-individual-only, or \
                     fewer/finer tiles.",
                );
            }
            let can_build = est.over_cell_budget || est.over_brick_cap || !est.fits_ram;
            for r in &remedies {
                ui.colored_label(STATUS_ERROR_FG, format!("• {r}"));
            }
            let build_btn = ui.add_enabled(
                !can_build,
                egui::Button::new("Build now").min_size(Vec2::new(160.0, 28.0)),
            );
            if can_build {
                build_btn.on_disabled_hover_text("Resolve the blocker(s) above first.");
            } else if build_btn.clicked() {
                launch = true;
            }
        });
    grid.estimate_open = open && !launch;
    if launch {
        // The owned plan + token-carrying settings are materialized ONLY here, at
        // launch, where they must move into the worker thread (spec correction
        // #7). `start_grid_build` clones the plan from this borrow internally.
        if let Some(plan) = grid.plan.clone() {
            let settings = grid_settings(inputs, grid.output);
            start_grid_build(grid, &plan, settings);
        }
    }
}

/// Spawn the grid worker thread (mirrors `start_fetch`, spec §8): reset cancel +
/// progress, register the `Promise` ONLY if the thread actually spawned so a
/// dropped sender can never panic the next poll.
fn start_grid_build(grid: &mut GridUiState, plan: &GridPlan, settings: GridSettings) {
    grid.last_outcome = None;
    grid.last_error = None;
    grid.cancel.store(false, Ordering::Relaxed);
    let tiles_total = plan.tiles.len() as u32;
    if let Ok(mut p) = grid.progress.lock() {
        *p = GridProgress::new(tiles_total);
    }
    let plan = plan.clone();
    let progress_arc = Arc::clone(&grid.progress);
    let cancel_arc = Arc::clone(&grid.cancel);
    let (sender, promise) = Promise::new();
    match std::thread::Builder::new()
        .name("h2brz-grid".into())
        .spawn(move || {
            let result = run_grid_build(plan, settings, progress_arc, cancel_arc);
            sender.send(result);
        }) {
        Ok(_handle) => grid.promise = Some(promise),
        Err(e) => {
            grid.last_error = Some(format!("could not start grid build thread: {e}"));
        }
    }
}

/// Two-level progress bar (spec §8): overall `(tiles_done + stage_fraction) /
/// tiles_total` plus the current per-tile build stage. Cancel sets the shared
/// flag (checked between fetches, at each tile top, and inside the mesher).
fn draw_grid_in_progress(grid: &GridUiState, ui: &mut Ui) {
    let snapshot = grid.progress.lock().ok().map(|p| (*p).clone());
    let Some(p) = snapshot else {
        ui.label("grid build running…");
        return;
    };
    ui.label(p.phase.label());
    let total = p.tiles_total.max(1) as f32;
    let overall = (p.tiles_done as f32 + p.stage_fraction.clamp(0.0, 1.0)) / total;
    ui.add(
        egui::ProgressBar::new(overall.clamp(0.0, 1.0))
            .text(format!("{}/{} tiles", p.tiles_done.min(p.tiles_total), p.tiles_total)),
    );
    if let Some(id) = p.current {
        ui.small(format!("tile r{} c{} · {}", id.row, id.col, p.stage.label()));
    }
    ui.add(egui::ProgressBar::new(p.stage_fraction.clamp(0.0, 1.0)).show_percentage());
    if ui.button("Cancel").clicked() {
        grid.cancel.store(true, Ordering::Relaxed);
    }
}

/// Drain the worker promise when ready (mirrors `poll_fetch_promise`). Safe to
/// call every frame; no-op until the worker finished.
pub(crate) fn poll_grid_promise(grid: &mut GridUiState) {
    let Some(promise) = grid.promise.as_ref() else {
        return;
    };
    if promise.ready().is_none() {
        return;
    }
    let promise = grid.promise.take().expect("just verified Some");
    match promise.try_take() {
        Ok(Ok(outcome)) => {
            grid.last_outcome = Some(outcome);
            grid.last_error = None;
        }
        Ok(Err(err)) => {
            grid.last_error = Some(err.to_string());
            grid.last_outcome = None;
        }
        Err(_) => {
            grid.promise = None;
        }
    }
}

/// True while a grid build is in flight — map_tab requests fast repaints then.
pub(crate) fn is_grid_running(grid: &GridUiState) -> bool {
    grid.is_running()
}

/// Render the finished build's written/installed file list + warnings (spec §7).
fn draw_grid_last_result(grid: &GridUiState, ui: &mut Ui) {
    if let Some(outcome) = &grid.last_outcome {
        ui.colored_label(
            STATUS_WARN_FG,
            format!(
                "✔ {} bricks across {} tile{}",
                outcome.brick_count,
                outcome.tiles,
                if outcome.tiles == 1 { "" } else { "s" },
            ),
        );
        for path in &outcome.written {
            ui.small(format!("wrote → {}", path.display()));
        }
        for path in &outcome.installed {
            ui.small(format!("installed → {}", path.display()));
        }
        for warn in &outcome.warnings {
            ui.colored_label(STATUS_ERROR_FG, format!("⚠ {warn}"));
        }
    }
    if let Some(err) = &grid.last_error {
        ui.colored_label(STATUS_ERROR_FG, format!("✘ {err}"));
    }
}

// ───────────────────────── Map overlay + tile pick (Mode 2, spec §8) ─────────

/// Paint the grid lattice over the map (spec §8 Mode 2): every tile's bbox is
/// projected via the same `paint_bbox` Rect pattern; excluded tiles are dimmed.
/// Runs from `map_tab::draw_bbox_overlay` ONLY when `grid.enabled` (additive — the
/// single-box overlay is untouched when grid is off).
pub(crate) fn draw_grid_overlay(grid: &GridUiState, painter: &egui::Painter, projector: &Projector) {
    let Some(plan) = &grid.plan else { return };
    let show_mask = grid.mode_kind == GridModeKind::ClickMask;
    for r in 0..plan.rows {
        for c in 0..plan.cols {
            let id = TileId { row: r, col: c };
            let excluded = show_mask && grid.excluded.contains(&id);
            let bbox = BBoxLatLon {
                north: plan.lat_edges[r as usize],
                south: plan.lat_edges[r as usize + 1],
                west: plan.lon_edges[c as usize],
                east: plan.lon_edges[c as usize + 1],
            };
            let (fill, stroke) = if excluded {
                (
                    Color32::from_rgba_unmultiplied(0x10, 0x10, 0x10, 0x60),
                    Color32::from_rgba_unmultiplied(0x80, 0x80, 0x80, 0x80),
                )
            } else {
                (BBOX_FILL, BBOX_STROKE)
            };
            paint_tile_rect(painter, projector, bbox, fill, stroke);
        }
    }
}

/// Project one tile bbox to a screen rect and paint it (the `paint_bbox` pattern,
/// map_tab.rs). `project()` returns absolute screen coords — no `rect.min` offset.
fn paint_tile_rect(
    painter: &egui::Painter,
    projector: &Projector,
    bbox: BBoxLatLon,
    fill: Color32,
    stroke: Color32,
) {
    let nw = lat_lon(bbox.north, bbox.west);
    let se = lat_lon(bbox.south, bbox.east);
    let nw_px = projector.project(nw).to_pos2();
    let se_px = projector.project(se).to_pos2();
    let rect = Rect::from_two_pos(nw_px, se_px);
    painter.rect_filled(rect, 0.0, fill);
    painter.rect_stroke(rect, 0.0, Stroke::new(1.0, stroke), StrokeKind::Middle);
}

/// Toggle tile membership by hit-testing the pointer against projected cell rects
/// on the SINGLE map `Response` (spec §8 Mode 2). The caller (`map_tab`) invokes
/// this ONLY when `grid.enabled && ClickMask && !draw_mode`, so box-draw and
/// tile-pick never fight for the pointer — NO second interact widget is added
/// here (it reuses the map's own `Response`, honoring map_tab's warning).
pub(crate) fn update_grid_pick(
    grid: &mut GridUiState,
    resp: &egui::Response,
    projector: &Projector,
) {
    if grid.mode_kind != GridModeKind::ClickMask {
        return;
    }
    let Some(plan) = &grid.plan else { return };
    // A click selects; `clicked()` fires once per release so a single tap toggles
    // exactly one tile. The pointer pos is in absolute screen coords (the same
    // frame `project()` outputs), so no rect.min offset.
    if !resp.clicked() {
        return;
    }
    let Some(pos) = resp.interact_pointer_pos() else {
        return;
    };
    for r in 0..plan.rows {
        for c in 0..plan.cols {
            let nw = projector.project(lat_lon(plan.lat_edges[r as usize], plan.lon_edges[c as usize]));
            let se = projector.project(lat_lon(
                plan.lat_edges[r as usize + 1],
                plan.lon_edges[c as usize + 1],
            ));
            let rect = Rect::from_two_pos(nw.to_pos2(), se.to_pos2());
            if rect.contains(pos) {
                let id = TileId { row: r, col: c };
                if !grid.excluded.remove(&id) {
                    grid.excluded.insert(id);
                }
                return;
            }
        }
    }
}

/// True when the grid overlay should claim the map pointer for tile picking:
/// grid enabled, ClickMask mode, and NOT in box-draw mode (spec §8). Used by
/// `map_tab` to decide whether to call [`update_grid_pick`].
pub(crate) fn wants_pick(grid: &GridUiState, draw_mode: bool) -> bool {
    grid.enabled && grid.mode_kind == GridModeKind::ClickMask && !draw_mode
}

/// Human-readable byte size (GiB / MiB) for the estimate window.
fn fmt_bytes(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GiB", b / GIB)
    } else {
        format!("{:.0} MiB", b / MIB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_bytes_thresholds() {
        assert_eq!(fmt_bytes(0), "0 MiB");
        assert_eq!(fmt_bytes(512 * 1024 * 1024), "512 MiB");
        assert_eq!(fmt_bytes(2 * 1024 * 1024 * 1024), "2.0 GiB");
    }

    #[test]
    fn source_max_zoom_opentopography_forces_z15() {
        // OpenTopography has no tile pyramid — the forced grid zoom is z15
        // (and grid mode warns against it). Mercator tile sources expose a cap.
        assert_eq!(source_max_zoom_for(DemSource::OpenTopography, None), 15);
        let aws = source_max_zoom_for(DemSource::AwsTerrarium, None);
        assert!(aws >= 1, "a tile source must expose a positive max zoom, got {aws}");
    }

    #[test]
    fn grid_state_defaults_to_off_and_legacy_output() {
        let g = GridUiState::new();
        assert!(!g.enabled, "grid must default OFF so the single-box flow is unchanged");
        assert_eq!(g.output, OutputOptions::default());
        assert_eq!(g.mode_kind, GridModeKind::AutoSubdivide);
        assert!(!g.is_running());
    }

    #[test]
    fn wants_pick_only_in_clickmask_overlay_without_draw_mode() {
        let mut g = GridUiState::new();
        g.enabled = true;
        g.mode_kind = GridModeKind::ClickMask;
        assert!(wants_pick(&g, false), "clickmask + overlay + not drawing must pick");
        assert!(!wants_pick(&g, true), "draw mode must suppress pick (single Response)");
        g.mode_kind = GridModeKind::AutoSubdivide;
        assert!(!wants_pick(&g, false), "non-clickmask modes never pick");
        g.mode_kind = GridModeKind::ClickMask;
        g.enabled = false;
        assert!(!wants_pick(&g, false), "disabled grid never picks");
    }

    #[test]
    fn mode_reassembly_tracks_active_tag() {
        let mut g = GridUiState::new();
        g.tile_m = 750.0;
        match g.mode() {
            GridMode::AutoSubdivide { tile_m } => assert_eq!(tile_m, 750.0),
            other => panic!("expected AutoSubdivide, got {other:?}"),
        }
        g.mode_kind = GridModeKind::Explicit;
        g.explicit_cols = 4;
        g.explicit_rows = 3;
        g.explicit_anchor = AnchorKind::Center;
        match g.mode() {
            GridMode::Explicit { tile_m, cols, rows, anchor } => {
                assert_eq!((tile_m, cols, rows), (750.0, 4, 3));
                assert_eq!(anchor, AnchorKind::Center);
            }
            other => panic!("expected Explicit, got {other:?}"),
        }
    }
}
