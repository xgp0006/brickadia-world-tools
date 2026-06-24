//! Map tab UI: OSM-backed slippy map + bbox selection + source pickers +
//! fetch/build kickoff. The map preview is always OSM regardless of the
//! selected imagery source — imagery only affects the generated bricks.

use egui::{Align, Color32, Layout, Pos2, Rect, Stroke, StrokeKind, TextEdit, Ui, Vec2};
use walkers::{HttpTiles, Map, MapMemory, Position, Projector, lat_lon};

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use poll_promise::Promise;

use super::build::{
    self, BlockType, BuildError, BuildOutcome, BuildRequest, BuildStage,
};
use super::config::Config;
use super::grid_ui::{self, GridInputs, GridUiState};
use super::coords::{format_lat_lon, parse_lat_lon};
use super::geocode::{self, GeocodeError, GeocodeHit};
use super::dem_sources::{Coverage, DemSource, RequiredKey};
use super::imagery_sources::ImagerySource;
use super::preview_source::PreviewBasemap;
use super::theme::{BBOX_FILL, BBOX_STROKE, STATUS_ERROR_FG, STATUS_WARN_FG};
use super::tiles::{BBoxLatLon, MERCATOR_LAT_LIMIT};

const HORSETOOTH_LAT: f64 = 40.5417;
const HORSETOOTH_LON: f64 = -105.1556;
const EARTH_RADIUS_KM: f64 = 6371.0088;
const KEY_INPUT_MAX_LEN: usize = 256;
/// Minimum bbox span per axis, in degrees (~11 m N/S). Boxes thinner than this
/// in either axis are rejected at the UI: a hairline drag yields a sub-pixel
/// crop that, while now safe (crop_window forces >=1px), produces a
/// pointless 1-pixel terrain. Symmetric gate for both latitude and longitude.
const MIN_SPAN_DEG: f64 = 1e-4;

/// Default + bounds for the true-scale "studs per real meter" control. 4 studs/m
/// makes a ~1 km AWS-Terrarium box (~3.6 m/cell at z15) a ~3,850-stud world at
/// faithful 1:1 relief — a big, walkable map out of the box. Bigger = bigger
/// world at NO brick cost; the bounds keep the derived integer brick scale in a
/// safe, useful range.
const DEFAULT_STUDS_PER_METER: f32 = 4.0;
const STUDS_PER_METER_MIN: f32 = 0.5;
const STUDS_PER_METER_MAX: f32 = 32.0;
/// Hard cap on the derived integer horizontal brick scale. The widest single
/// brick is `max_quad * size = 500 * hscale` units in BOTH brick classes (normal:
/// 100 * hscale*5; micro: 500 * hscale*1), which must stay under the u16
/// `BrickSize` ceiling (65535) → hscale ≤ 131. 128 (= 64000, safe) lets MICRO —
/// which needs ~5× the integer scale of normal bricks to hit the same physical
/// scale — reach a fine, faithful 1:1 model instead of clamping early; normal
/// bricks rarely want more than ~32 (above that each cell is a very wide plate).
const MAX_HORIZONTAL_SCALE: u16 = 128;

#[derive(Debug, Clone, Copy)]
pub(crate) struct BBox {
    pub(crate) north: f64,
    pub(crate) south: f64,
    pub(crate) east: f64,
    pub(crate) west: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BBoxRejection {
    Antimeridian,
    Degenerate,
}

impl BBoxRejection {
    pub(crate) const fn message(self) -> &'static str {
        match self {
            Self::Antimeridian => {
                "Bounding box crosses the 180° antimeridian — split your selection."
            }
            Self::Degenerate => {
                "Bounding box is too small (near-zero width or height) — \
                 draw a larger area within ±85° latitude."
            }
        }
    }
}

impl BBox {
    /// Build a bbox from two opposite corners.
    ///
    /// # Preconditions
    /// None — non-finite corners are rejected as `Degenerate` at runtime.
    ///
    /// # Postconditions on `Ok(bbox)`
    /// - `bbox.north > bbox.south` (positive height)
    /// - both latitudes within `±MERCATOR_LAT_LIMIT` (Web Mercator safe range)
    /// - `bbox.east >= bbox.west` (the antimeridian-crossing case rejects)
    ///
    /// # Errors
    /// - `Antimeridian` if the drag spans 180°/-180° (split bboxes unsupported)
    /// - `Degenerate` if the polar latitude clamp collapses height to zero
    fn from_corners(a: Position, b: Position) -> Result<Self, BBoxRejection> {
        // Real release check, not debug_assert: corners derive from projector
        // math on user drag input, and a NaN passes every comparison below
        // (NaN.max/min propagate) — it must be rejected here or it reaches the
        // fetch pipeline. debug_assert! is compiled out of release builds.
        if !(a.x().is_finite() && a.y().is_finite() && b.x().is_finite() && b.y().is_finite()) {
            return Err(BBoxRejection::Degenerate);
        }
        let raw_north = a.y().max(b.y());
        let raw_south = a.y().min(b.y());
        let raw_east = a.x().max(b.x());
        let raw_west = a.x().min(b.x());

        let dlon = raw_east - raw_west;
        if dlon > 180.0 {
            return Err(BBoxRejection::Antimeridian);
        }
        // Symmetric width gate: reject a near-zero longitude span (vertical-line
        // drag) as well as latitude. The lat collapse is re-checked after the
        // polar clamp below, which can also collapse height near the poles.
        if dlon < MIN_SPAN_DEG {
            return Err(BBoxRejection::Degenerate);
        }

        let north = raw_north.clamp(-MERCATOR_LAT_LIMIT, MERCATOR_LAT_LIMIT);
        let south = raw_south.clamp(-MERCATOR_LAT_LIMIT, MERCATOR_LAT_LIMIT);
        if north - south < MIN_SPAN_DEG {
            return Err(BBoxRejection::Degenerate);
        }

        let bbox = Self { north, south, east: raw_east, west: raw_west };
        debug_assert!(
            bbox.north > bbox.south
                && bbox.north <= MERCATOR_LAT_LIMIT
                && bbox.south >= -MERCATOR_LAT_LIMIT
                && bbox.east >= bbox.west,
            "BBox::from_corners postcondition violated: {bbox:?}",
        );
        Ok(bbox)
    }

    fn centroid_lat(&self) -> f64 {
        0.5 * (self.north + self.south)
    }

    fn height_km(&self) -> f64 {
        haversine_km(self.south, self.west, self.north, self.west)
    }

    fn width_km(&self) -> f64 {
        haversine_km(self.centroid_lat(), self.west, self.centroid_lat(), self.east)
    }

    fn area_km2(&self) -> f64 {
        self.width_km() * self.height_km()
    }
}

fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let to_rad = std::f64::consts::PI / 180.0;
    let phi1 = lat1 * to_rad;
    let phi2 = lat2 * to_rad;
    let dphi = (lat2 - lat1) * to_rad;
    let dlam = (lon2 - lon1) * to_rad;
    let a = (dphi * 0.5).sin().powi(2) + phi1.cos() * phi2.cos() * (dlam * 0.5).sin().powi(2);
    let c = 2.0 * a.sqrt().asin();
    EARTH_RADIUS_KM * c
}

#[derive(Debug, Clone, Copy)]
struct DragState {
    start: Position,
    current: Position,
}

pub(crate) struct MapTabState {
    tiles: Option<HttpTiles>,
    /// Which basemap `tiles` currently holds, so `ensure_tiles` rebuilds the
    /// `HttpTiles` only when the selected imagery (or its token) actually changes.
    tiles_basemap: Option<PreviewBasemap>,
    map_memory: MapMemory,
    config: Config,
    config_error: Option<String>,
    coord_input: String,
    coord_error: Option<String>,
    search_input: String,
    search_promise: Option<Promise<Result<GeocodeHit, GeocodeError>>>,
    search_status: Option<String>,
    bbox: Option<BBox>,
    bbox_error: Option<String>,
    drag: Option<DragState>,
    draw_mode: bool,
    dem_source: DemSource,
    imagery_source: ImagerySource,
    block_type: BlockType,
    density_factor: u16,
    /// True-scale controls. `studs_per_meter` sets horizontal world size (more =
    /// bigger, free); `vertical_exaggeration` scales relief (1.0 = faithful
    /// 1:1). The integer brick `horizontal_scale` + `vertical_scale` fed to the
    /// build are DERIVED from these + the DEM resolution at fetch time
    /// (`derive_scale`), so the world is true-to-life at the chosen scale.
    studs_per_meter: f32,
    vertical_exaggeration: f32,
    glow: bool,
    no_collision: bool,
    overwrite_world: bool,
    output_name: String,
    settings_open: bool,
    key_input_opentopo: String,
    key_input_mapbox: String,
    fetch_promise: Option<Promise<Result<BuildOutcome, BuildError>>>,
    fetch_progress: Arc<Mutex<(BuildStage, f32)>>,
    fetch_cancel: Arc<AtomicBool>,
    last_outcome: Option<BuildOutcome>,
    last_error: Option<String>,
    /// All grid-build UI + worker state (spec §8). Defaults OFF so the single-box
    /// flow is visually unchanged; lives behind the "Grid build" collapsing header.
    grid: GridUiState,
}

impl MapTabState {
    pub(crate) fn new() -> Self {
        let (config, config_error) = match Config::load() {
            Ok(c) => (c, None),
            Err(err) => {
                log::error!("config load failed, using defaults: {err}");
                (Config::default(), Some(err.to_string()))
            }
        };
        let key_input_opentopo = config.opentopo_api_key.clone().unwrap_or_default();
        let key_input_mapbox = config.mapbox_token.clone().unwrap_or_default();
        Self {
            tiles: None,
            tiles_basemap: None,
            map_memory: MapMemory::default(),
            config,
            config_error,
            coord_input: format_lat_lon(HORSETOOTH_LAT, HORSETOOTH_LON),
            coord_error: None,
            search_input: String::new(),
            search_promise: None,
            search_status: None,
            bbox: None,
            bbox_error: None,
            drag: None,
            draw_mode: false,
            dem_source: DemSource::AwsTerrarium,
            imagery_source: ImagerySource::EsriWorldImagery,
            block_type: BlockType::SmoothTile,
            density_factor: 1,
            studs_per_meter: DEFAULT_STUDS_PER_METER,
            vertical_exaggeration: 1.0,
            glow: false,
            no_collision: false,
            overwrite_world: false,
            output_name: String::from("my-area"),
            settings_open: false,
            key_input_opentopo,
            key_input_mapbox,
            fetch_promise: None,
            fetch_progress: Arc::new(Mutex::new((BuildStage::FetchingTiles, 0.0))),
            fetch_cancel: Arc::new(AtomicBool::new(false)),
            last_outcome: None,
            last_error: None,
            grid: GridUiState::new(),
        }
    }

    fn is_fetching(&self) -> bool {
        self.fetch_promise.is_some()
    }

    /// Signal an in-flight build worker to stop. Called on app shutdown so the
    /// detached fetch thread observes cancellation promptly instead of running
    /// the full network + brick generation after the window is gone.
    pub(crate) fn cancel_fetch(&self) {
        self.fetch_cancel.store(true, Ordering::Relaxed);
        // The grid worker shares the same poll-promise + cancel-flag contract; on
        // shutdown it must observe cancellation too so its detached two-phase
        // build (fetch + mesh + write) stops instead of running on after the
        // window is gone.
        self.grid.cancel.store(true, Ordering::Relaxed);
    }

    fn ensure_tiles(&mut self, ctx: &egui::Context) {
        // The preview basemap mirrors the selected imagery source, so changing
        // the Imagery picker visibly swaps the map and the user watches the new
        // tiles stream in (walkers fetches + repaints per tile on its own). Pure
        // derive-from-state: rebuild HttpTiles only when the resolved basemap key
        // (including the Mapbox token) differs from what is currently loaded.
        let want = PreviewBasemap::resolve(self.imagery_source, self.config.mapbox_token.as_deref());
        if self.tiles.is_none() || self.tiles_basemap.as_ref() != Some(&want) {
            self.tiles = Some(want.build_tiles(ctx));
            self.tiles_basemap = Some(want);
        }
    }

    fn default_center(&self) -> Position {
        lat_lon(HORSETOOTH_LAT, HORSETOOTH_LON)
    }
}

pub(crate) fn draw(state: &mut MapTabState, ctx: &egui::Context, ui: &mut Ui) {
    state.ensure_tiles(ctx);
    poll_fetch_promise(state);
    poll_search_promise(state);
    grid_ui::poll_grid_promise(&mut state.grid);
    if state.search_promise.is_some() {
        ctx.request_repaint_after(std::time::Duration::from_millis(120));
    }
    if state.is_fetching() {
        ctx.request_repaint_after(std::time::Duration::from_millis(120));
    }
    if grid_ui::is_grid_running(&state.grid) {
        ctx.request_repaint_after(std::time::Duration::from_millis(120));
    }
    egui::SidePanel::right("map_controls")
        .resizable(true)
        .default_width(280.0)
        .show_inside(ui, |ui| draw_controls(state, ui));
    egui::TopBottomPanel::bottom("map_status")
        .resizable(false)
        .min_height(48.0)
        .show_inside(ui, |ui| draw_status(state, ui));
    egui::CentralPanel::default().show_inside(ui, |ui| draw_map_area(state, ui));
    draw_settings_window(state, ctx);
}

fn draw_controls(state: &mut MapTabState, ui: &mut Ui) {
    ui.heading("Map");
    ui.separator();
    draw_coord_entry(state, ui);
    ui.add_space(8.0);
    draw_search_field(state, ui);
    ui.add_space(8.0);
    draw_box_controls(state, ui);
    ui.add_space(8.0);
    draw_source_pickers(state, ui);
    let basemap = state.tiles_basemap.as_ref().map_or("OpenStreetMap", PreviewBasemap::label);
    ui.small(format!(
        "Preview basemap: {basemap}. Brick colors use the selected imagery at build \
         time (often a higher zoom than the preview shows)."
    ));
    if state.imagery_source == ImagerySource::MapboxSatellite && !state.config.mapbox_token_set() {
        ui.colored_label(
            STATUS_WARN_FG,
            "No Mapbox token — preview falls back to OpenStreetMap (set one in Settings).",
        );
    }
    ui.add_space(8.0);
    draw_brick_options(state, ui);
    ui.add_space(8.0);
    draw_output_section(state, ui);
    ui.add_space(8.0);
    draw_fetch_button(state, ui);
    ui.add_space(8.0);
    draw_grid_section(state, ui);
    ui.add_space(6.0);
    draw_last_result(state, ui);
    ui.add_space(12.0);
    if ui.button("Settings…").clicked() {
        state.settings_open = true;
    }
}

/// Render the grid-build section (spec §8). Constructs [`GridInputs`] from the
/// single-box shaping fields (immutable borrows of disjoint `MapTabState` fields)
/// and passes `&mut state.grid` — Rust splits the borrow because each side
/// touches a different field. The section is gated internally by `grid.enabled`,
/// so the single-box flow is visually unchanged when grid mode is off.
fn draw_grid_section(state: &mut MapTabState, ui: &mut Ui) {
    let bbox = state.bbox.map(|b| BBoxLatLon {
        north: b.north,
        south: b.south,
        east: b.east,
        west: b.west,
    });
    let inputs = GridInputs {
        bbox,
        dem_source: state.dem_source,
        imagery_source: state.imagery_source,
        block_type: state.block_type,
        glow: state.glow,
        no_collision: state.no_collision,
        overwrite: state.overwrite_world,
        studs_per_meter: state.studs_per_meter,
        vertical_exaggeration: state.vertical_exaggeration,
        output_name: &state.output_name,
        config: &state.config,
    };
    grid_ui::draw_grid_section(&mut state.grid, ui, &inputs);
}

fn draw_coord_entry(state: &mut MapTabState, ui: &mut Ui) {
    ui.label("Go to coordinates (decimal lat, lon)");
    ui.horizontal(|ui| {
        let resp = ui.add(
            TextEdit::singleline(&mut state.coord_input)
                .hint_text("40.5417, -105.1556")
                .desired_width(180.0),
        );
        let submit = ui.button("Go").clicked()
            || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
        if submit {
            try_recenter(state);
        }
    });
    if let Some(err) = &state.coord_error {
        ui.colored_label(STATUS_ERROR_FG, err);
    }
}

fn try_recenter(state: &mut MapTabState) {
    match parse_lat_lon(&state.coord_input) {
        Ok((lat, lon)) => {
            state.map_memory.center_at(lat_lon(lat, lon));
            state.coord_error = None;
        }
        Err(err) => {
            state.coord_error = Some(err.to_string());
        }
    }
}

fn draw_search_field(state: &mut MapTabState, ui: &mut Ui) {
    ui.label("Search place (any landmark, city, or address)");
    let token_set = state.config.mapbox_token_set();
    let searching = state.search_promise.is_some();
    let mut submit = false;
    ui.horizontal(|ui| {
        let resp = ui.add_enabled(
            !searching,
            TextEdit::singleline(&mut state.search_input)
                .hint_text("Mount Fuji")
                .desired_width(180.0),
        );
        // Drop a stale "→ place" / error line once the user edits the query.
        if resp.changed() {
            state.search_status = None;
        }
        let has_query = !state.search_input.trim().is_empty();
        let btn_label = if searching { "…" } else { "Search" };
        let btn = ui.add_enabled(
            token_set && has_query && !searching,
            egui::Button::new(btn_label),
        );
        let btn = if !token_set {
            btn.on_disabled_hover_text("Set a Mapbox token in Settings to search")
        } else {
            btn
        };
        submit = btn.clicked()
            || (token_set
                && has_query
                && !searching
                && resp.lost_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter)));
    });
    if submit {
        start_search(state);
    }
    if let Some(msg) = &state.search_status {
        ui.small(msg);
    }
}

/// Spawn a worker thread that forward-geocodes `search_input` via Mapbox and
/// hands the hit back through a `Promise`. Mirrors `start_fetch`: the promise
/// is registered only if the thread actually spawned, so `poll_search_promise`
/// never panics on a dropped sender.
fn start_search(state: &mut MapTabState) {
    let query = state.search_input.trim().to_owned();
    let Some(token) = state.config.mapbox_token.clone() else {
        state.search_status = Some(GeocodeError::MissingToken.to_string());
        return;
    };
    // Bias results toward the coordinate currently in the "go to" box, when it
    // parses — keeps same-named places near the user's focus ranked first.
    let proximity = parse_lat_lon(&state.coord_input).ok();
    state.search_status = Some(format!("Searching “{query}”…"));

    let (sender, promise) = Promise::new();
    match std::thread::Builder::new()
        .name("h2brz-geocode".into())
        .spawn(move || {
            let result = geocode::forward(&query, &token, proximity);
            sender.send(result);
        }) {
        Ok(_handle) => state.search_promise = Some(promise),
        Err(e) => {
            state.search_status = Some(format!("could not start search thread: {e}"));
        }
    }
}

fn poll_search_promise(state: &mut MapTabState) {
    let Some(promise) = state.search_promise.as_ref() else {
        return;
    };
    if promise.ready().is_none() {
        return;
    }
    let promise = state.search_promise.take().expect("just verified Some");
    match promise.try_take() {
        Ok(Ok(hit)) => {
            state.map_memory.center_at(lat_lon(hit.lat, hit.lon));
            state.coord_input = format_lat_lon(hit.lat, hit.lon);
            state.coord_error = None;
            state.search_status = Some(format!("→ {}", hit.label));
        }
        Ok(Err(err)) => {
            state.search_status = Some(err.to_string());
        }
        Err(_) => {
            state.search_promise = None;
        }
    }
}

fn draw_box_controls(state: &mut MapTabState, ui: &mut Ui) {
    ui.label("Bounding box");
    ui.horizontal(|ui| {
        let label = if state.draw_mode { "Drawing… (left-drag; right-drag pans)" } else { "Draw Box" };
        if ui
            .selectable_label(state.draw_mode, label)
            .on_hover_text(
                "Toggle draw mode, then LEFT-drag on the map to select an area. While drawing, \
                 RIGHT- or MIDDLE-drag still pans and scroll / double-click zooms.",
            )
            .clicked()
        {
            state.draw_mode = !state.draw_mode;
            if !state.draw_mode {
                state.drag = None;
            }
        }
        let clear_enabled = state.bbox.is_some();
        if ui.add_enabled(clear_enabled, egui::Button::new("Clear")).clicked() {
            state.bbox = None;
            state.drag = None;
        }
    });
}

fn draw_source_pickers(state: &mut MapTabState, ui: &mut Ui) {
    ui.label("Elevation source");
    draw_dem_picker(state, ui);
    ui.add_space(4.0);
    ui.label("Imagery source (colormap)");
    draw_imagery_picker(state, ui);
}

fn draw_dem_picker(state: &mut MapTabState, ui: &mut Ui) {
    let current_key_ok = key_ok_for(&state.config, state.dem_source.required_key());
    let label = if current_key_ok {
        state.dem_source.display_label()
    } else {
        "(needs key — set in Settings)"
    };
    egui::ComboBox::from_id_salt("dem_source_combo")
        .selected_text(label)
        .width(260.0)
        .show_ui(ui, |ui| {
            for src in DemSource::ALL {
                let wired = src.is_fetchable();
                let usable = wired && key_ok_for(&state.config, src.required_key());
                let mut item_label = src.display_label().to_owned();
                push_coverage_badge(&mut item_label, src.coverage());
                if !wired {
                    item_label.push_str("  (coming soon)");
                }
                let resp = ui
                    .add_enabled(
                        usable,
                        egui::Button::selectable(state.dem_source == *src, item_label),
                    )
                    .on_hover_text(src.tooltip());
                let resp = if wired {
                    resp
                } else {
                    resp.on_disabled_hover_text("Not wired for in-app fetch yet")
                };
                if resp.clicked() && usable {
                    state.dem_source = *src;
                }
            }
        });
}

fn draw_imagery_picker(state: &mut MapTabState, ui: &mut Ui) {
    let current_key_ok = key_ok_for(&state.config, state.imagery_source.required_key());
    let label = if current_key_ok {
        state.imagery_source.display_label()
    } else {
        "(needs key — set in Settings)"
    };
    egui::ComboBox::from_id_salt("imagery_source_combo")
        .selected_text(label)
        .width(260.0)
        .show_ui(ui, |ui| {
            for src in ImagerySource::ALL {
                let wired = src.is_fetchable();
                let usable = wired && key_ok_for(&state.config, src.required_key());
                let mut item_label = src.display_label().to_owned();
                push_coverage_badge(&mut item_label, src.coverage());
                if !wired {
                    item_label.push_str("  (coming soon)");
                }
                let resp = ui
                    .add_enabled(
                        usable,
                        egui::Button::selectable(state.imagery_source == *src, item_label),
                    )
                    .on_hover_text(src.tooltip());
                let resp = if wired {
                    resp
                } else {
                    resp.on_disabled_hover_text("Not wired for in-app fetch yet")
                };
                if resp.clicked() && usable {
                    state.imagery_source = *src;
                }
            }
        });
}

fn push_coverage_badge(label: &mut String, coverage: Coverage) {
    label.push_str("  [");
    label.push_str(coverage.short_badge());
    label.push(']');
}

fn key_ok_for(config: &Config, required: Option<RequiredKey>) -> bool {
    match required {
        None => true,
        Some(RequiredKey::OpenTopoApiKey) => config.opentopo_key_set(),
        Some(RequiredKey::MapboxToken) => config.mapbox_token_set(),
    }
}

fn draw_brick_options(state: &mut MapTabState, ui: &mut Ui) {
    ui.label("Brick type");
    egui::ComboBox::from_id_salt("block_type_combo")
        .selected_text(state.block_type.label())
        .width(260.0)
        .show_ui(ui, |ui| {
            for bt in BlockType::ALL {
                if ui
                    .add(egui::Button::selectable(state.block_type == bt, bt.label()))
                    .clicked()
                {
                    state.block_type = bt;
                }
            }
        });
    if state.block_type.micro() {
        ui.small(
            "Micro: finest bricks (1/5 size) — best for detailed / small-scale models. \
             The world stays true 1:1.",
        );
    }

    ui.add_space(4.0);
    ui.label("Density (terrain resolution)");
    ui.add(
        egui::DragValue::new(&mut state.density_factor)
            .range(1..=8)
            .speed(0.1),
    )
    .on_hover_text(
        "Downsamples the elevation grid. Larger = fewer, coarser bricks \
         (real count reduction, ~1/factor²); 1 = full detail.",
    );

    ui.add_space(4.0);
    ui.label("Scale (studs per real meter)");
    ui.add(
        egui::DragValue::new(&mut state.studs_per_meter)
            .range(STUDS_PER_METER_MIN..=STUDS_PER_METER_MAX)
            .speed(0.1),
    )
    .on_hover_text(
        "True-to-life horizontal scale: this many studs reproduce one real meter. \
         Higher = a bigger world at the SAME brick count (free). Vertical height \
         auto-matches for faithful 1:1 relief — see the predicted size below.",
    );

    ui.add_space(4.0);
    ui.label("Vertical exaggeration");
    ui.add(
        egui::Slider::new(&mut state.vertical_exaggeration, 0.25..=8.0)
            .logarithmic(true)
            .text("×"),
    )
    .on_hover_text(
        "1× = faithful 1:1 relief (height matched to the true ground at the chosen \
         scale). Raise it to exaggerate hills; lower it to flatten.",
    );

    ui.add_space(4.0);
    ui.checkbox(&mut state.no_collision, "No collision (decorative)")
        .on_hover_text(
            "Bricks have no player/weapon/interaction collision — a look-only \
             model you walk through, not on.",
        );
    ui.checkbox(&mut state.glow, "Glowing terrain (emissive)")
        .on_hover_text(
            "Terrain uses the emissive GLOW material instead of plastic, so it \
             self-illuminates in-game.",
        );
}

fn draw_output_section(state: &mut MapTabState, ui: &mut Ui) {
    ui.label("Output name");
    ui.add(
        TextEdit::singleline(&mut state.output_name)
            .hint_text("my-area")
            .desired_width(260.0),
    );
    ui.checkbox(&mut state.overwrite_world, "Overwrite existing world")
        .on_hover_text(
            "On: re-running this name replaces <name>.brdb in Brickadia (load the same world to \
             see updates). Off: never clobbers — installs as <name>-2, -3, … instead.",
        );
}

fn draw_fetch_button(state: &mut MapTabState, ui: &mut Ui) {
    if state.is_fetching() {
        draw_fetch_in_progress(state, ui);
        return;
    }
    let reasons = fetch_disabled_reasons(state);
    let enabled = reasons.is_empty();
    // Surface the specific "too small" rejection (only otherwise shown in the
    // bottom status panel) right next to the button it disabled.
    if let Some(err) = &state.bbox_error {
        ui.colored_label(STATUS_ERROR_FG, err);
    }
    // Co-locate every blocker with the dead button — some reasons (empty name,
    // missing key, unwired source) have no other on-screen cue.
    if !enabled {
        for reason in &reasons {
            ui.colored_label(STATUS_WARN_FG, format!("• {reason}"));
        }
    }
    let label = "⬇  Fetch & Build";
    let resp = ui.add_enabled(
        enabled,
        egui::Button::new(label).min_size(Vec2::new(260.0, 32.0)),
    );
    if !enabled {
        resp.on_disabled_hover_text(format!("Not ready:\n{}", reasons.join("\n")));
    } else if resp
        .on_hover_text("Fetch DEM tiles, generate bricks, install into Brickadia Worlds/")
        .clicked()
    {
        start_fetch(state);
    }
}

fn draw_fetch_in_progress(state: &mut MapTabState, ui: &mut Ui) {
    let (stage, fraction) = match state.fetch_progress.lock() {
        Ok(g) => *g,
        Err(_) => (BuildStage::FetchingTiles, 0.0),
    };
    ui.label(stage.label());
    ui.add(egui::ProgressBar::new(fraction.clamp(0.0, 1.0)).show_percentage());
    if ui.button("Cancel").clicked() {
        state.fetch_cancel.store(true, Ordering::Relaxed);
    }
}

fn start_fetch(state: &mut MapTabState) {
    let Some(bbox) = state.bbox else {
        state.last_error = Some("internal: Fetch clicked with no bbox".into());
        return;
    };
    // Derive the integer brick horizontal_scale + the 1:1-matched vertical_scale
    // from the user's true-scale controls and the DEM resolution this box will
    // fetch at (the SAME prediction the size readout shows). Fall back to the
    // default cell size if the resolution can't be predicted (shouldn't happen
    // once Fetch is enabled, which requires a usable source).
    let cell_m_eff = predicted_cell_m(state, bbox).unwrap_or(30.0)
        * f64::from(state.density_factor.max(1));
    let (horizontal_scale, vertical_scale) = derive_scale(
        cell_m_eff,
        state.studs_per_meter,
        state.vertical_exaggeration,
        state.block_type.micro(),
    );
    let request = BuildRequest {
        bbox: BBoxLatLon {
            north: bbox.north,
            south: bbox.south,
            east: bbox.east,
            west: bbox.west,
        },
        name: state.output_name.clone(),
        dem_source: state.dem_source,
        imagery_source: state.imagery_source,
        mapbox_token: state.config.mapbox_token.clone(),
        opentopo_key: state.config.opentopo_api_key.clone(),
        vertical_scale,
        density_factor: state.density_factor,
        horizontal_scale,
        block_type: state.block_type,
        glow: state.glow,
        no_collision: state.no_collision,
        install_to_brickadia: true,
        overwrite_world: state.overwrite_world,
    };
    state.last_outcome = None;
    state.last_error = None;
    state.fetch_cancel.store(false, Ordering::Relaxed);
    if let Ok(mut g) = state.fetch_progress.lock() {
        *g = (BuildStage::FetchingTiles, 0.0);
    }
    let progress_arc = Arc::clone(&state.fetch_progress);
    let cancel_arc = Arc::clone(&state.fetch_cancel);
    let progress_fn: build::ProgressFn = Arc::new(move |stage, f| {
        if let Ok(mut g) = progress_arc.lock() {
            *g = (stage, f);
        }
    });
    let (sender, promise) = Promise::new();
    // Register the promise ONLY if the worker actually started. If spawn fails
    // (e.g. EAGAIN under resource pressure) the closure — and its `sender` —
    // are dropped without sending; registering the promise anyway would make
    // the next poll_fetch_promise panic ("The Promise Sender was dropped").
    match std::thread::Builder::new()
        .name("h2brz-build".into())
        .spawn(move || {
            let result = build::run_build(request, progress_fn, cancel_arc);
            // Receiver may be gone if the user closed the tab; send() no-ops.
            sender.send(result);
        }) {
        Ok(_handle) => state.fetch_promise = Some(promise),
        Err(e) => {
            // promise + its sender drop together here, never polled.
            state.last_error = Some(format!("could not start build thread: {e}"));
        }
    }
}

fn poll_fetch_promise(state: &mut MapTabState) {
    let Some(promise) = state.fetch_promise.as_ref() else {
        return;
    };
    if promise.ready().is_none() {
        return;
    }
    let promise = state.fetch_promise.take().expect("just verified Some");
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
            state.fetch_promise = None;
        }
    }
}

fn draw_last_result(state: &MapTabState, ui: &mut Ui) {
    if let Some(outcome) = &state.last_outcome {
        ui.colored_label(
            STATUS_WARN_FG,
            format!(
                "✔ {} bricks · {}×{} px · {:.0}–{:.0} m",
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

fn fetch_disabled_reasons(state: &MapTabState) -> Vec<&'static str> {
    let mut reasons: Vec<&'static str> = Vec::with_capacity(4);
    if state.bbox.is_none() {
        reasons.push("Select a bounding box first");
    }
    if state.output_name.trim().is_empty() {
        reasons.push("Output name is empty");
    }
    if !key_ok_for(&state.config, state.dem_source.required_key()) {
        reasons.push("Selected elevation source needs an API key");
    }
    // USGS 3DEP / USGS orthoimagery are shown DISABLED ("coming soon") in the
    // pickers and can never be SELECTED (the picker gates the click on
    // is_fetchable), so state.dem_source / state.imagery_source is always a
    // fetch-wired source — no dead-source Fetch blocker is needed here.
    if !key_ok_for(&state.config, state.imagery_source.required_key()) {
        reasons.push("Selected imagery source needs an API key");
    }
    reasons
}

fn draw_status(state: &mut MapTabState, ui: &mut Ui) {
    ui.horizontal(|ui| {
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| match state.bbox {
            Some(b) => draw_bbox_readout(b, ui),
            None => {
                ui.label("No bounding box selected — click Draw Box and drag on the map");
            }
        });
    });
    if let Some(b) = state.bbox {
        draw_zoom_readout(state, b, ui);
        draw_output_estimate(state, b, ui);
    }
    if let Some(err) = &state.bbox_error {
        ui.colored_label(STATUS_ERROR_FG, err);
    }
    if let Some(err) = &state.config_error {
        ui.colored_label(STATUS_WARN_FG, format!("(config warning: {err})"));
    }
}

/// Show the effective fetch zoom for the selected bbox and each provider's
/// resolution cap. Reads the SAME `TileSource::max_zoom()` the fetch clamps to
/// (via the real source constructor) so the displayed cap can never lie. DEM
/// and imagery have different caps and are fetched separately — show both.
/// Best-effort: a source needing an unset key yields no line rather than guess.
fn draw_zoom_readout(state: &MapTabState, b: BBox, ui: &mut Ui) {
    let bbox = BBoxLatLon { north: b.north, south: b.south, east: b.east, west: b.west };
    let token = state.config.mapbox_token.as_deref();
    // OpenTopography is a single-shot bbox API with a per-request AREA cap, not a
    // zoom cap — show the real km² limit instead (the documented 450,000 km²).
    if state.dem_source == DemSource::OpenTopography {
        let area = build::bbox_area_km2(&bbox);
        let cap = build::OPENTOPO_MAX_AREA_KM2;
        let msg = format!(
            "DEM: {} · area {area:.0} km² (cap {cap:.0} km²)",
            state.dem_source.display_label(),
        );
        if area > cap {
            ui.colored_label(STATUS_ERROR_FG, format!("{msg} — over limit, shrink the box"));
        } else {
            ui.small(msg);
        }
        if area < 100.0 {
            ui.colored_label(
                STATUS_WARN_FG,
                "SRTMGL1 is ~30 m per cell — for areas this small, AWS Terrarium (~5 m/cell at \
                 z15) gives far finer terrain",
            );
        }
    } else if let Some(src) = super::dem_sources::tile_source_for(state.dem_source, token) {
        // Name the DEM source + its per-cell resolution so switching the DEM
        // picker visibly changes this line (zero extra fetch — pure prediction).
        let cap = src.max_zoom();
        let z = super::tiles::pick_zoom(bbox, cap);
        let m_per_cell = ground_resolution_m(b.centroid_lat(), z);
        let line = format!(
            "DEM: {} · zoom {z} (cap z{cap}) · ~{m_per_cell:.0} m/cell",
            state.dem_source.display_label(),
        );
        if z >= cap {
            ui.colored_label(STATUS_WARN_FG, line);
        } else {
            ui.small(line);
        }
    }
    if let Some(src) = super::imagery_sources::tile_source_for(state.imagery_source, token) {
        emit_zoom_line(ui, "Imagery", &bbox, src.max_zoom());
    }
}

fn emit_zoom_line(ui: &mut Ui, label: &str, bbox: &BBoxLatLon, cap: u32) {
    let z = super::tiles::pick_zoom(*bbox, cap);
    if z >= cap {
        ui.colored_label(STATUS_WARN_FG, format!("{label} zoom {z} (provider cap z{cap})"));
    } else {
        ui.small(format!("{label} zoom {z} (cap z{cap})"));
    }
}

/// Predicted footprint of the generated map for the current bbox + settings,
/// shown BEFORE fetching so "why is my map tiny" is answerable from the status
/// bar. DEM cell size: SRTMGL1 ≈ 30 m; XYZ tile sources use the Web Mercator
/// ground resolution at the zoom `pick_zoom` will choose. Haversine-side
/// estimate, not a pixel-exact crop preview.
fn draw_output_estimate(state: &MapTabState, b: BBox, ui: &mut Ui) {
    let Some(cell_m) = predicted_cell_m(state, b) else { return };
    let cell_m_eff = cell_m * f64::from(state.density_factor.max(1));
    let (hscale, _vertical) = derive_scale(
        cell_m_eff,
        state.studs_per_meter,
        state.vertical_exaggeration,
        state.block_type.micro(),
    );
    // studs/cell = 2*size/5 = 2*hscale*upf/5 (BrickSize is a half-extent, so a
    // cell's footprint equals its 2*size pitch — verified by
    // greedy_brick_pitch_is_twice_size). World studs/side = cells/side *
    // studs/cell. The integer hscale is derived from the Scale control, so the
    // realized size may differ slightly from the exact target (shown live).
    let upf = if state.block_type.micro() { 1.0 } else { 5.0 };
    let studs_per_cell = 2.0 * f64::from(hscale) * upf / 5.0;
    let cells_w = b.width_km() * 1000.0 / cell_m_eff;
    let cells_h = b.height_km() * 1000.0 / cell_m_eff;
    let (w, h) = (cells_w * studs_per_cell, cells_h * studs_per_cell);
    if !(w.is_finite() && h.is_finite()) || w < 1.0 || h < 1.0 {
        return;
    }
    let achieved = w / (b.width_km() * 1000.0); // studs per real meter
    let relief = if (state.vertical_exaggeration - 1.0).abs() < 0.05 {
        "1:1".to_owned()
    } else {
        format!("×{:.1}", state.vertical_exaggeration)
    };
    ui.small(format!(
        "World ≈ {w:.0}×{h:.0} studs · ~{achieved:.1} studs/m · relief {relief} (auto) · \
         ~{cell_m_eff:.0} m/cell"
    ));
    // The requested scale needs a per-cell brick the mesher can't reach (the u16
    // BrickSize ceiling caps the integer scale at MAX_HORIZONTAL_SCALE), so the
    // achieved studs/m is BELOW what was set and each coarse cell renders as a
    // large brick — the "why are my micro bricks huge?" case. Tell the user
    // loudly and point at the real fix (a smaller box → finer cells), since
    // raising Scale further does nothing here.
    let ideal_hscale = f64::from(state.studs_per_meter) * 5.0 * cell_m_eff / (2.0 * upf);
    if ideal_hscale > f64::from(MAX_HORIZONTAL_SCALE) + 0.5 {
        ui.colored_label(
            STATUS_WARN_FG,
            format!(
                "Scale capped: you set {:.1} studs/m, but this area's ~{cell_m_eff:.0} m cells max \
                 out at ~{achieved:.1} studs/m — so each cell is a large brick (micro or not). Draw \
                 a SMALLER box for finer cells + your full scale; a big area can't also be fine.",
                state.studs_per_meter,
            ),
        );
    } else {
        ui.small("Bigger = raise Scale (free). Micro = finer brick TYPE, not smaller bricks.");
    }
}

/// DEM ground resolution (meters per cell) predicted for this bbox + DEM source,
/// BEFORE density downsampling — mirrors what `fetch_and_decode_dem` resolves so
/// the size readout and `start_fetch` derive the SAME scale. `None` when the
/// source has no usable fetch path (e.g. a token-gated source with no token).
fn predicted_cell_m(state: &MapTabState, b: BBox) -> Option<f64> {
    /// SRTMGL1 N-S cell size: 1 arc-second of LATITUDE ≈ 30.92 m, ~constant.
    const SRTMGL1_NS_M: f64 = 30.92;
    let bbox = BBoxLatLon { north: b.north, south: b.south, east: b.east, west: b.west };
    match state.dem_source {
        DemSource::OpenTopography => {
            // OpenTopography SRTMGL1 is a GEOGRAPHIC (EPSG:4326) grid, NOT Web
            // Mercator: a cell is 1 arc-second square in degrees, so its E-W
            // ground size shrinks with cos(lat) (≈30.92*cos) while N-S stays
            // ~30.92. The mesher renders SQUARE bricks, so no single cell size
            // captures both axes (geographic sources are inherently aspect-
            // distorted away from the equator — use AWS Terrarium / Mapbox, which
            // are Mercator and isotropic, for a faithful 1:1 SHAPE). Use the
            // geometric mean of the two axes so the relief error is symmetric
            // (±sqrt over each axis) rather than one axis exact and the other off.
            let cos_lat = b.centroid_lat().to_radians().cos().max(0.01);
            Some(SRTMGL1_NS_M * cos_lat.sqrt())
        }
        src => {
            let token = state.config.mapbox_token.as_deref();
            super::dem_sources::tile_source_for(src, token).map(|s| {
                let z = super::tiles::pick_zoom(bbox, s.max_zoom());
                ground_resolution_m(b.centroid_lat(), z)
            })
        }
    }
}

/// Derive the integer horizontal brick scale and the 1:1-matched vertical scale
/// from the user's target studs-per-meter + exaggeration, the effective
/// (post-density) DEM cell size in meters, and whether micro bricks are used.
///
/// True 1:1: a cell spans `2*size = 2*hscale*upf` world units (`upf` = the
/// GenOptions size multiplier — 5 for normal bricks, 1 for micro; 1 stud = 5
/// units) over `cell_m_eff` meters → `2*hscale*upf/cell_m_eff` units/m
/// horizontally. Surface-Z is `(m-min)*vertical` units → `vertical` units/m.
/// Setting them equal makes relief faithful; `exaggeration` scales it.
pub(crate) fn derive_scale(
    cell_m_eff: f64,
    studs_per_meter: f32,
    exaggeration: f32,
    micro: bool,
) -> (u16, f32) {
    debug_assert!(cell_m_eff > 0.0, "derive_scale: cell_m_eff must be positive, got {cell_m_eff}");
    let upf = if micro { 1.0 } else { 5.0 };
    // Solve `2*hscale*upf / cell_m_eff == studs_per_meter * 5` (5 units/stud).
    let hscale = ((f64::from(studs_per_meter) * 5.0 * cell_m_eff) / (2.0 * upf))
        .round()
        .clamp(1.0, f64::from(MAX_HORIZONTAL_SCALE)) as u16;
    // 1:1 vertical (units/m) == achieved horizontal units/m, times exaggeration.
    let vertical = ((2.0 * f64::from(hscale) * upf / cell_m_eff) * f64::from(exaggeration)) as f32;
    (hscale, vertical)
}

/// Web Mercator ground resolution (meters per 256px-tile pixel) at a latitude.
pub(crate) fn ground_resolution_m(lat_deg: f64, zoom: u32) -> f64 {
    const EQUATOR_M_PER_PX_Z0: f64 = 40_075_016.686 / 256.0;
    EQUATOR_M_PER_PX_Z0 * lat_deg.to_radians().cos() / f64::from(2_u32.pow(zoom.min(30)))
}

fn draw_bbox_readout(b: BBox, ui: &mut Ui) {
    let w_km = b.width_km();
    let h_km = b.height_km();
    let area = b.area_km2();
    ui.monospace(format!(
        "N {:.5}  S {:.5}  W {:.5}  E {:.5}",
        b.north, b.south, b.west, b.east
    ));
    ui.separator();
    ui.label(format!("{w_km:.2} × {h_km:.2} km  ({area:.1} km²)"));
}

fn draw_map_area(state: &mut MapTabState, ui: &mut Ui) {
    let center = state.default_center();
    let draw_mode = state.draw_mode;
    let Some(tiles) = state.tiles.as_mut() else {
        ui.colored_label(
            Color32::from_rgb(220, 100, 100),
            "internal error: map tiles failed to initialize — try restarting the app",
        );
        return;
    };
    // Double-click zooms in — an obvious framing affordance (Ctrl+scroll is not
    // discoverable). While armed for drawing, move the map's drag-to-pan OFF the
    // primary button onto secondary/middle. walkers' Map allocates
    // `click_and_drag` over the whole rect and pans on PRIMARY by default, which
    // would fight the box draw; the old overlaid SECOND interact couldn't fix
    // that — two widgets over one rect both claim the pointer drag (egui tracks a
    // drag per widget Id, button-agnostically), so it was deleted. With ONE
    // widget the response can still be queried PER BUTTON via `*_by`, so we
    // re-key the map's pan to secondary/middle and key the box to the primary
    // button (update_bbox_drag) — left-drag draws while the user can STILL
    // right/middle-drag to pan and reposition the map, no armed navigation trap.
    // Outside draw mode the map keeps default PRIMARY drag-pan.
    let mut map = Map::new(Some(tiles), &mut state.map_memory, center).double_click_to_zoom(true);
    if draw_mode {
        map = map.drag_pan_buttons(egui::DragPanButtons::SECONDARY | egui::DragPanButtons::MIDDLE);
    }
    let response = ui.add(map);
    let rect = response.rect;
    debug_assert!(
        rect.width() > 0.0 && rect.height() > 0.0,
        "draw_map_area: map widget rect must have positive size, got {rect:?}",
    );
    let projector = Projector::new(rect, &state.map_memory, center);

    if draw_mode {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        // Drive the bbox drag from the map widget's OWN response (no overlaid
        // second interact, which would steal the drag). The box keys off the
        // PRIMARY button; secondary/middle drags pan the map underneath.
        update_bbox_drag(state, &response, &projector);
    } else {
        state.drag = None;
        // Grid Click-mask tile pick reuses the SAME map `Response` (NO second
        // interact widget — spec §8). It runs ONLY when grid is enabled, the mode
        // is ClickMask, and Draw Box is OFF, so box-draw and tile-pick never fight
        // for the pointer.
        if grid_ui::wants_pick(&state.grid, draw_mode) {
            grid_ui::update_grid_pick(&mut state.grid, &response, &projector);
        }
    }

    draw_bbox_overlay(state, ui, &projector, rect);

    // "loading N tiles" cue so a source/basemap swap visibly shows progress.
    // walkers repaints per tile arrival, so this counter ticks down on its own.
    // (The &mut tiles borrow ended when `ui.add(map)` consumed the Map above.)
    let loading = state.tiles.as_ref().map_or(0, |t| t.stats().in_progress);
    if loading > 0 {
        ui.painter_at(rect).text(
            rect.left_top() + Vec2::new(8.0, 8.0),
            egui::Align2::LEFT_TOP,
            format!("loading {loading} tile{}…", if loading == 1 { "" } else { "s" }),
            egui::FontId::proportional(12.0),
            Color32::from_rgba_unmultiplied(0xE4, 0xE0, 0xD2, 0xC0),
        );
    }
}

fn update_bbox_drag(state: &mut MapTabState, resp: &egui::Response, projector: &Projector) {
    debug_assert!(
        resp.rect.width() > 0.0 && resp.rect.height() > 0.0,
        "update_bbox_drag precondition: response rect must have positive area, got {:?}",
        resp.rect,
    );
    if let Some(pointer_pos) = resp.interact_pointer_pos() {
        // Projector works in ABSOLUTE screen coordinates (project(map_center)
        // == rect.center() on screen), so the pointer position is fed to
        // unproject directly — NO rect.min subtraction. Verified by
        // `projector_returns_absolute_screen_coords`.
        let pos = projector.unproject(pointer_pos.to_vec2());
        // Gate strictly on the PRIMARY button: the map pans on secondary/middle
        // while armed, and egui's drag_started()/dragged()/drag_stopped() flags
        // are button-AGNOSTIC, so a right/middle-drag pan would otherwise draw a
        // box too. `*_by(Primary)` keeps the box on the left button only.
        if resp.drag_started_by(egui::PointerButton::Primary) {
            state.drag = Some(DragState { start: pos, current: pos });
            state.bbox_error = None;
        } else if resp.dragged_by(egui::PointerButton::Primary)
            && let Some(d) = state.drag.as_mut()
        {
            d.current = pos;
        }
    }
    if resp.drag_stopped_by(egui::PointerButton::Primary) {
        if let Some(d) = state.drag.take() {
            match BBox::from_corners(d.start, d.current) {
                Ok(bbox) => {
                    state.bbox = Some(bbox);
                    state.bbox_error = None;
                    // Stay armed (do NOT auto-exit draw mode): the next drag
                    // simply replaces the box, so a slightly-off selection is
                    // re-drawn in one gesture instead of re-toggling Draw Box.
                    // Toggle Draw Box off (or Clear) to leave draw mode and pan.
                }
                Err(rejection) => {
                    // A stray too-small drag must NOT wipe a good existing box —
                    // only report the error. Stay armed to retry immediately.
                    state.bbox_error = Some(rejection.message().to_owned());
                }
            }
        }
    } else if state.drag.is_some()
        && !resp.dragged_by(egui::PointerButton::Primary)
        && !resp.drag_started_by(egui::PointerButton::Primary)
    {
        // The primary draw drag ended WITHOUT a drag_stopped event (Escape, or a
        // multi-button gesture that released Primary early). Discard the
        // in-progress box so no ghost rectangle lingers until the next drag.
        state.drag = None;
    }
}

fn draw_bbox_overlay(state: &MapTabState, ui: &Ui, projector: &Projector, rect: Rect) {
    let painter = ui.painter_at(rect);
    if let Some(d) = state.drag
        && let Ok(bbox) = BBox::from_corners(d.start, d.current)
    {
        paint_bbox(&painter, projector, bbox, BBOX_FILL, BBOX_STROKE);
    }
    if let Some(bbox) = state.bbox {
        paint_bbox(&painter, projector, bbox, BBOX_FILL, BBOX_STROKE);
    }
    // Additive grid-lattice paint: only when grid mode is enabled, so the
    // single-box overlay is visually unchanged when grid is off (spec §8).
    if state.grid.enabled {
        grid_ui::draw_grid_overlay(&state.grid, &painter, projector);
    }
    if state.draw_mode && state.drag.is_none() {
        // Armed but not yet dragging — this is exactly when the user needs the
        // prompt, so paint it at full opacity.
        paint_map_hint(&painter, rect, "Click and drag to draw the box", 0xFF);
    } else if state.bbox.is_none() && !state.draw_mode {
        paint_map_hint(&painter, rect, "Click \"Draw Box\" then drag here", 0x80);
    }
}

fn paint_map_hint(painter: &egui::Painter, rect: Rect, text: &str, alpha: u8) {
    let hint_color = Color32::from_rgba_unmultiplied(0xE4, 0xE0, 0xD2, alpha);
    let center = rect.center();
    let pip_radius = 4.0;
    painter.circle_stroke(center, pip_radius, Stroke::new(1.5, hint_color));
    let label_rect = Rect::from_center_size(
        Pos2::new(center.x, center.y + pip_radius + 18.0),
        Vec2::new(280.0, 20.0),
    );
    painter.text(
        label_rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(13.0),
        hint_color,
    );
}

fn paint_bbox(
    painter: &egui::Painter,
    projector: &Projector,
    bbox: BBox,
    fill: Color32,
    stroke: Color32,
) {
    debug_assert!(
        bbox.north.is_finite()
            && bbox.south.is_finite()
            && bbox.east.is_finite()
            && bbox.west.is_finite(),
        "paint_bbox precondition: bbox fields must be finite, got {bbox:?}",
    );
    debug_assert!(
        bbox.north > bbox.south,
        "paint_bbox precondition: north must exceed south, got {bbox:?}",
    );
    let nw = lat_lon(bbox.north, bbox.west);
    let se = lat_lon(bbox.south, bbox.east);
    // project() returns absolute screen coords; no rect.min offset.
    let nw_px = projector.project(nw).to_pos2();
    let se_px = projector.project(se).to_pos2();
    let rect = Rect::from_two_pos(nw_px, se_px);
    painter.rect_filled(rect, 0.0, fill);
    painter.rect_stroke(rect, 0.0, Stroke::new(2.0, stroke), StrokeKind::Middle);
}

fn draw_settings_window(state: &mut MapTabState, ctx: &egui::Context) {
    let mut open = state.settings_open;
    egui::Window::new("Settings — API keys")
        .open(&mut open)
        .resizable(false)
        .default_width(420.0)
        .show(ctx, |ui| draw_settings_body(state, ui));
    state.settings_open = open;
}

fn draw_settings_body(state: &mut MapTabState, ui: &mut Ui) {
    ui.label("Keys are stored in ~/.config/heightmap2brz/config.toml (mode 600)");
    ui.add_space(6.0);
    ui.label("OpenTopography API key:");
    draw_key_row(ui, &mut state.key_input_opentopo, "paste your free key");
    ui.small("https://portal.opentopography.org/myopentopo — free signup");
    ui.add_space(8.0);
    ui.label("Mapbox access token:");
    draw_key_row(ui, &mut state.key_input_mapbox, "paste your free token");
    ui.small("https://account.mapbox.com/access-tokens/ — free tier");
    ui.add_space(12.0);
    ui.horizontal(|ui| draw_settings_buttons(state, ui));
}

fn draw_key_row(ui: &mut Ui, value: &mut String, hint: &str) {
    ui.horizontal(|ui| {
        ui.add(
            TextEdit::singleline(value)
                .password(true)
                .hint_text(hint)
                .char_limit(KEY_INPUT_MAX_LEN)
                .desired_width(310.0),
        );
        let clear_enabled = !value.is_empty();
        if ui
            .add_enabled(clear_enabled, egui::Button::new("Clear"))
            .on_hover_text("Empty the field. Click Save to remove the key from disk.")
            .clicked()
        {
            value.clear();
        }
    });
}

fn draw_settings_buttons(state: &mut MapTabState, ui: &mut Ui) {
    if ui.button("Save").clicked() {
        commit_settings(state);
    }
    if ui.button("Close").clicked() {
        state.settings_open = false;
    }
}

fn commit_settings(state: &mut MapTabState) {
    let opentopo = trimmed_or_none(&state.key_input_opentopo);
    let mapbox = trimmed_or_none(&state.key_input_mapbox);
    state.config.opentopo_api_key = opentopo;
    state.config.mapbox_token = mapbox;
    match state.config.save() {
        Ok(()) => {
            log::info!("config saved to disk");
            state.settings_open = false;
        }
        Err(err) => {
            log::error!("config save failed: {err}");
            state.config_error = Some(format!("save failed: {err}"));
        }
    }
}

fn trimmed_or_none(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_owned()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Pos2, Rect, Vec2};

    /// The whole point of the scale control: at exaggeration 1.0 the world is
    /// FAITHFUL 1:1 — vertical units per meter equals horizontal units per meter,
    /// so a real hill keeps its true aspect ratio in-game. Horizontal units/m =
    /// `2*hscale*upf/cell_m`; vertical units/m = the returned `vertical_scale`.
    #[test]
    fn derive_scale_is_true_1to1_at_exaggeration_1() {
        let cell_m = 3.63; // AWS Terrarium z15, ~1 km box at 40.5°N
        let (hs, v) = derive_scale(cell_m, 4.0, 1.0, false);
        // hscale = round(4 * 5 * 3.63 / (2*5)) = round(7.26) = 7
        assert_eq!(hs, 7, "horizontal scale derivation");
        let horiz_units_per_m = 2.0 * f64::from(hs) * 5.0 / cell_m;
        assert!(
            (f64::from(v) - horiz_units_per_m).abs() < 1e-3,
            "1:1 broken: vertical {v} units/m != horizontal {horiz_units_per_m} units/m",
        );
        // Micro keeps the SAME physical scale (upf=1 → hscale ×5 to compensate),
        // and stays 1:1.
        let (hs_m, v_m) = derive_scale(cell_m, 4.0, 1.0, true);
        let horiz_m = 2.0 * f64::from(hs_m) * 1.0 / cell_m;
        assert!((f64::from(v_m) - horiz_m).abs() < 1e-3, "micro 1:1 broken");
    }

    #[test]
    fn derive_scale_exaggeration_only_scales_vertical() {
        let (hs1, v1) = derive_scale(3.63, 4.0, 1.0, false);
        let (hs2, v2) = derive_scale(3.63, 4.0, 2.0, false);
        assert_eq!(hs1, hs2, "exaggeration must NOT change horizontal scale");
        assert!((v2 - 2.0 * v1).abs() < 1e-3, "2× exaggeration must double vertical, {v1}->{v2}");
    }

    #[test]
    fn derive_scale_micro_matches_normal_physical_scale_at_1to1() {
        // Micro bricks must produce the SAME physical world (same studs/m, same
        // 1:1 relief) as normal bricks at the same Scale — micro just needs ~5×
        // the integer brick scale because each micro brick is 1/5 the size. This
        // is what lets a user pick Micro "for nice resolution" without changing
        // the true-scale geometry.
        let cell_m = 3.63;
        let (hs_n, v_n) = derive_scale(cell_m, 4.0, 1.0, false);
        let (hs_m, v_m) = derive_scale(cell_m, 4.0, 1.0, true);
        let horiz_n = 2.0 * f64::from(hs_n) * 5.0 / cell_m; // normal units/m
        let horiz_m = 2.0 * f64::from(hs_m) * 1.0 / cell_m; // micro units/m
        assert!(
            (horiz_n - horiz_m).abs() / horiz_n < 0.05,
            "micro physical scale must match normal within integer rounding: {horiz_n} vs {horiz_m}",
        );
        assert!(
            (f64::from(v_n) - horiz_n).abs() < 1e-3 && (f64::from(v_m) - horiz_m).abs() < 1e-3,
            "both micro and normal must stay true 1:1",
        );
        // Micro reaches FINER scales than normal: at hscale 1 its cell is 1/5 the
        // footprint, so a very small / detailed model is achievable where normal
        // bricks floor out.
        let (hs_fine_n, _) = derive_scale(cell_m, 0.1, 1.0, false);
        let (hs_fine_m, _) = derive_scale(cell_m, 0.1, 1.0, true);
        let fine_n = 2.0 * f64::from(hs_fine_n) * 5.0 / cell_m / 5.0; // studs/m floor, normal
        let fine_m = 2.0 * f64::from(hs_fine_m) * 1.0 / cell_m / 5.0; // studs/m floor, micro
        assert!(fine_m < fine_n, "micro must reach a finer studs/m floor than normal ({fine_m} < {fine_n})");
    }

    #[test]
    fn derive_scale_bigger_studs_per_meter_means_bigger_world_clamped() {
        let (small, _) = derive_scale(3.63, 1.0, 1.0, false);
        let (big, _) = derive_scale(3.63, 8.0, 1.0, false);
        assert!(big > small, "more studs/m must raise horizontal scale ({small} -> {big})");
        assert_eq!(derive_scale(3.63, 1000.0, 1.0, false).0, MAX_HORIZONTAL_SCALE, "caps at MAX");
        assert_eq!(derive_scale(3.63, 0.0001, 1.0, false).0, 1, "floors at 1");
    }

    #[test]
    fn derive_scale_clamps_on_a_huge_coarse_box() {
        // The user's report: a ~28 km box forces ~58 m cells (the DEM cell
        // budget downsamples a big area), so studs_per_meter=4 wants hscale ~580
        // and clamps to the u16 brick-size ceiling — yielding LARGE bricks (micro
        // or not) at a much lower achieved scale than requested. Lock that case.
        let cell_m = 58.0; // ~z11 over a 28 km box at mid latitude
        let (hs, _) = derive_scale(cell_m, 4.0, 1.0, true);
        assert_eq!(hs, MAX_HORIZONTAL_SCALE, "coarse cells + 4 studs/m must hit the brick-scale cap");
        let achieved = 2.0 * f64::from(hs) * 1.0 / cell_m / 5.0; // studs/m
        assert!(achieved < 1.5, "capped achieved studs/m ~{achieved} is well below the set 4");
    }

    #[test]
    fn derive_scale_vertical_stays_under_the_build_clamp() {
        // TS-1 guard (the regression an earlier 64.0 clamp introduced): the
        // vertical derive_scale produces must NEVER exceed the build.rs ceiling,
        // or build_heightmap would silently compress 1:1 / the exaggeration. Sweep
        // cell_m across the realizable range (fine high-latitude tiles → coarse
        // OpenTopography) at the control maxima for BOTH brick classes.
        use crate::gui::build::MAX_VERTICAL_SCALE;
        const EXAGGERATION_MAX: f32 = 8.0; // matches the slider range below
        for &cell_m in &[0.4_f64, 3.63, 8.0, 30.0, 120.0] {
            for &micro in &[false, true] {
                let (_, v) = derive_scale(cell_m, STUDS_PER_METER_MAX, EXAGGERATION_MAX, micro);
                assert!(
                    v < MAX_VERTICAL_SCALE,
                    "derive_scale vertical {v} (cell_m={cell_m}, micro={micro}) must stay under the \
                     build clamp {MAX_VERTICAL_SCALE}, else 1:1/exaggeration is silently capped",
                );
            }
        }
    }

    /// Lock down the `walkers::Projector` semantics our bbox math depends on.
    /// `project(p)` returns ABSOLUTE screen coordinates: `project(map_center)`
    /// equals `rect.center()` on screen (NOT rect-relative). Production
    /// therefore feeds project/unproject directly — `project(corner)` for
    /// painting and `unproject(pointer_pos)` for the drag — with no `rect.min`
    /// offset. (An earlier B-1.1 version added `rect.min`, which double-offset
    /// the bbox whenever the map widget sat at a non-zero origin — i.e. always,
    /// since it lives below the tab bar. `projector_returns_absolute_screen_coords`
    /// is the regression guard, using a non-zero `rect.min` so the bug is
    /// observable; a `rect.min == ZERO` test cannot see it.)
    ///
    /// `projector_anchor_follows_map_memory` separately proves the projector
    /// anchors on `MapMemory`'s center, not the `default_pos` arg (Horsetooth
    /// vs Fuji, ~9000 km, would otherwise project far outside the viewport).
    #[test]
    fn projector_anchor_follows_map_memory() {
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));
        let mut memory = MapMemory::default();
        memory.center_at(lat_lon(35.36, 138.73));
        let default_pos = lat_lon(40.54, -105.16);
        let projector = Projector::new(rect, &memory, default_pos);

        let projected = projector.project(lat_lon(35.36, 138.73));
        let expected = rect.center().to_vec2();
        let drift = (projected - expected).length();
        assert!(
            drift < 5.0,
            "Centered position should project to rect.center() = {expected:?}; \
             got {projected:?} (drift {drift} px). If this drifts, the projector \
             may have stopped anchoring on MapMemory or its coordinate origin \
             changed across a walkers version bump.",
        );
    }

    /// Round-trip property: `unproject(project(p)) ≈ p`. Guarantees the
    /// bbox-drag (`unproject(pointer_pos)`) and bbox-paint (`project(corner)`)
    /// compose correctly in absolute screen space.
    #[test]
    fn projector_project_unproject_roundtrip() {
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));
        let mut memory = MapMemory::default();
        memory.center_at(lat_lon(35.36, 138.73));
        let default_pos = lat_lon(40.54, -105.16);
        let projector = Projector::new(rect, &memory, default_pos);

        let p = lat_lon(35.36, 138.73);
        let screen = projector.project(p);
        let back = projector.unproject(screen);
        let dlat = (back.y() - p.y()).abs();
        let dlon = (back.x() - p.x()).abs();
        assert!(
            dlat < 1e-6 && dlon < 1e-6,
            "Round-trip drift exceeds tolerance: p={p:?} → screen={screen:?} → back={back:?} \
             (dlat={dlat}, dlon={dlon})",
        );
    }

    /// Proves `project()` returns ABSOLUTE screen coords (no `rect.min` offset),
    /// using a NON-zero `rect.min` so the absolute-vs-relative distinction is
    /// observable. This is the regression guard for the B-1.1 double-offset bug:
    /// `project(map_center)` must equal the rect's on-screen center directly,
    /// and ADDING `rect.min` (the old code) must be demonstrably wrong.
    #[test]
    fn projector_returns_absolute_screen_coords() {
        let origin = Pos2::new(120.0, 80.0);
        let rect = Rect::from_min_size(origin, Vec2::new(800.0, 600.0));
        let mut memory = MapMemory::default();
        let center_pos = lat_lon(35.36, 138.73);
        memory.center_at(center_pos);
        let projector = Projector::new(rect, &memory, lat_lon(40.54, -105.16));

        // project() yields absolute screen coords: map center -> rect center.
        let projected = projector.project(center_pos);
        let visual_center = rect.center().to_vec2();
        let drift = (projected - visual_center).length();
        assert!(
            drift < 5.0,
            "project() must return absolute screen coords (map center -> rect center \
             {visual_center:?}); got {projected:?} (drift {drift}px)",
        );

        // Adding rect.min (the old B-1.1 bug) double-offsets and misregisters.
        let double_offset = origin.to_vec2() + projected;
        assert!(
            (double_offset - visual_center).length() > 100.0,
            "adding rect.min would misregister by ~rect.min; {double_offset:?} vs {visual_center:?}",
        );

        // unproject is the exact inverse in the same absolute frame: this is
        // what update_bbox_drag relies on (unproject(pointer_pos), no offset).
        let back = projector.unproject(projected);
        assert!(
            (back.x() - center_pos.x()).abs() < 1e-6 && (back.y() - center_pos.y()).abs() < 1e-6,
            "unproject(project(center)) must round-trip; got {back:?}",
        );
    }

    #[test]
    fn haversine_basic_sanity() {
        let d = haversine_km(0.0, 0.0, 0.0, 0.001);
        assert!((d - 0.1112).abs() < 0.01, "expected ~0.111 km; got {d}");
    }

    #[test]
    fn bbox_clamps_polar_latitudes() {
        let a = lat_lon(86.0, -106.0);
        let b = lat_lon(40.5, -105.0);
        let bbox = BBox::from_corners(a, b).expect("bbox should clamp, not reject");
        assert!(
            (bbox.north - MERCATOR_LAT_LIMIT).abs() < 1e-9,
            "north latitude {} should clamp to {MERCATOR_LAT_LIMIT}",
            bbox.north
        );
        assert!(
            (bbox.south - 40.5).abs() < 1e-9,
            "south latitude {} should pass through unchanged",
            bbox.south
        );
    }

    #[test]
    fn bbox_rejects_antimeridian_crossing() {
        let a = lat_lon(0.0, 170.0);
        let b = lat_lon(10.0, -170.0);
        let result = BBox::from_corners(a, b);
        assert_eq!(
            result,
            Err(BBoxRejection::Antimeridian),
            "spanning 170°E to -170°W should reject as antimeridian crossing (got dlon \
             {}°), not silently invert the bbox",
            (170.0_f64).max(-170.0) - (170.0_f64).min(-170.0)
        );
    }

    #[test]
    fn bbox_rejects_degenerate_after_clamp() {
        // Both corners above the polar clamp → both clamp to the same value
        // → north == south → degenerate bbox.
        let a = lat_lon(89.0, 0.0);
        let b = lat_lon(88.0, 10.0);
        let result = BBox::from_corners(a, b);
        assert_eq!(
            result,
            Err(BBoxRejection::Degenerate),
            "both corners above ±85° should reject as degenerate after polar clamp",
        );
    }

    #[test]
    fn bbox_accepts_normal_horsetooth() {
        let a = lat_lon(40.523, -105.183);
        let b = lat_lon(40.560, -105.131);
        let bbox = BBox::from_corners(a, b).expect("Horsetooth bbox should pass cleanly");
        assert!(bbox.area_km2() > 1.0 && bbox.area_km2() < 100.0, "area {} km²", bbox.area_km2());
    }

    impl PartialEq for BBox {
        fn eq(&self, other: &Self) -> bool {
            (self.north - other.north).abs() < 1e-12
                && (self.south - other.south).abs() < 1e-12
                && (self.east - other.east).abs() < 1e-12
                && (self.west - other.west).abs() < 1e-12
        }
    }
}
