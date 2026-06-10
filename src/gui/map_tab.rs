//! Map tab UI: OSM-backed slippy map + bbox selection + source pickers +
//! fetch/build kickoff. The map preview is always OSM regardless of the
//! selected imagery source — imagery only affects the generated bricks.

use egui::{Align, Color32, Layout, Pos2, Rect, Sense, Stroke, StrokeKind, TextEdit, Ui, Vec2};
use walkers::{HttpTiles, Map, MapMemory, Position, Projector, lat_lon, sources::OpenStreetMap};

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use poll_promise::Promise;

use super::build::{
    self, BlockType, BuildError, BuildOutcome, BuildRequest, BuildStage, DEFAULT_VERTICAL_SCALE,
};
use super::config::Config;
use super::coords::{format_lat_lon, parse_lat_lon};
use super::geocode::{self, GeocodeError, GeocodeHit};
use super::dem_sources::{Coverage, DemSource, RequiredKey};
use super::imagery_sources::ImagerySource;
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
    horizontal_scale: u16,
    vertical_scale: f32,
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
            horizontal_scale: 1,
            vertical_scale: DEFAULT_VERTICAL_SCALE,
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
    }

    fn ensure_tiles(&mut self, ctx: &egui::Context) {
        if self.tiles.is_none() {
            self.tiles = Some(HttpTiles::new(OpenStreetMap, ctx.clone()));
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
    if state.search_promise.is_some() {
        ctx.request_repaint_after(std::time::Duration::from_millis(120));
    }
    if state.is_fetching() {
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
    ui.small("Preview is OpenStreetMap only — it does not reflect the imagery source. Brick colors use the selected imagery at build time.");
    ui.add_space(8.0);
    draw_brick_options(state, ui);
    ui.add_space(8.0);
    draw_output_section(state, ui);
    ui.add_space(8.0);
    draw_fetch_button(state, ui);
    ui.add_space(6.0);
    draw_last_result(state, ui);
    ui.add_space(12.0);
    if ui.button("Settings…").clicked() {
        state.settings_open = true;
    }
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
        let label = if state.draw_mode { "Drawing… (click & drag)" } else { "Draw Box" };
        if ui
            .selectable_label(state.draw_mode, label)
            .on_hover_text("Toggle draw mode, then click-drag on the map to select an area")
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
                let usable = key_ok_for(&state.config, src.required_key());
                let mut item_label = src.display_label().to_owned();
                push_coverage_badge(&mut item_label, src.coverage());
                let resp = ui
                    .add_enabled(
                        usable,
                        egui::Button::selectable(state.dem_source == *src, item_label),
                    )
                    .on_hover_text(src.tooltip());
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
                let usable = key_ok_for(&state.config, src.required_key());
                let mut item_label = src.display_label().to_owned();
                push_coverage_badge(&mut item_label, src.coverage());
                let resp = ui
                    .add_enabled(
                        usable,
                        egui::Button::selectable(state.imagery_source == *src, item_label),
                    )
                    .on_hover_text(src.tooltip());
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
    ui.label("Horizontal scale (studs per cell)");
    ui.add(
        egui::DragValue::new(&mut state.horizontal_scale)
            .range(1..=16)
            .speed(0.1),
    )
    .on_hover_text(
        "Widens each terrain cell to N studs — same brick count, bigger map. \
         The cure for tiny output: SRTMGL1's ~30 m cells at 1 stud/cell make a \
         1 km box only ~33 studs wide. Raise vertical exaggeration to match or \
         the terrain will look flattened.",
    );

    ui.add_space(4.0);
    ui.label("Vertical exaggeration");
    ui.add(
        egui::Slider::new(&mut state.vertical_scale, 0.1..=20.0)
            .logarithmic(true)
            .text("×"),
    )
    .on_hover_text("Height multiplier. Higher relief = more vertical bricks (toward the cap).");

    ui.add_space(4.0);
    ui.checkbox(&mut state.no_collision, "No collision (decorative)");
    ui.checkbox(&mut state.glow, "Glowing terrain (emissive)");
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
        vertical_scale: state.vertical_scale,
        density_factor: state.density_factor,
        horizontal_scale: state.horizontal_scale,
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
    if matches!(state.dem_source, DemSource::Usgs3Dep) {
        reasons.push("USGS 3DEP is not wired yet; pick AWS Terrarium, Mapbox, or OpenTopography");
    }
    if matches!(state.imagery_source, ImagerySource::UsgsOrthoimagery) {
        reasons.push("USGS orthoimagery is not wired yet; pick ESRI, Mapbox, or None");
    }
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
        let msg = format!("DEM area {area:.0} km² (OpenTopography cap {cap:.0} km²)");
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
        emit_zoom_line(ui, "DEM", &bbox, src.max_zoom());
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
    /// SRTM 1 arc-second nominal ground resolution.
    const SRTMGL1_CELL_M: f64 = 30.0;
    let bbox = BBoxLatLon { north: b.north, south: b.south, east: b.east, west: b.west };
    let cell_m = match state.dem_source {
        DemSource::OpenTopography => Some(SRTMGL1_CELL_M),
        src => {
            let token = state.config.mapbox_token.as_deref();
            super::dem_sources::tile_source_for(src, token).map(|s| {
                let z = super::tiles::pick_zoom(bbox, s.max_zoom());
                ground_resolution_m(b.centroid_lat(), z)
            })
        }
    };
    let Some(cell_m) = cell_m else { return };
    let density = f64::from(state.density_factor.max(1));
    let units_per_cell =
        f64::from(state.horizontal_scale.max(1)) * if state.block_type.micro() { 1.0 } else { 5.0 };
    let studs_per_km = 1000.0 / cell_m / density * units_per_cell / 5.0;
    let (w, h) = (b.width_km() * studs_per_km, b.height_km() * studs_per_km);
    if !(w.is_finite() && h.is_finite()) || w < 1.0 || h < 1.0 {
        return;
    }
    let m_per_stud = b.width_km() * 1000.0 / w;
    ui.small(format!(
        "Predicted output ≈ {w:.0}×{h:.0} studs ({m_per_stud:.1} m per stud) — raise Horizontal \
         scale for a bigger map at no brick cost"
    ));
}

/// Web Mercator ground resolution (meters per 256px-tile pixel) at a latitude.
fn ground_resolution_m(lat_deg: f64, zoom: u32) -> f64 {
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
    let Some(tiles) = state.tiles.as_mut() else {
        ui.colored_label(
            Color32::from_rgb(220, 100, 100),
            "internal error: map tiles failed to initialize — try restarting the app",
        );
        return;
    };
    let response = ui.add(Map::new(Some(tiles), &mut state.map_memory, center));
    let rect = response.rect;
    debug_assert!(
        rect.width() > 0.0 && rect.height() > 0.0,
        "draw_map_area: map widget rect must have positive size, got {rect:?}",
    );
    let projector = Projector::new(rect, &state.map_memory, center);

    if state.draw_mode {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        let map_sense = Sense::click_and_drag();
        let drag_resp = ui.interact(rect, ui.id().with("bbox_drag"), map_sense);
        update_bbox_drag(state, &drag_resp, &projector);
    } else {
        state.drag = None;
    }

    draw_bbox_overlay(state, ui, &projector, rect);
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
        if resp.drag_started() {
            state.drag = Some(DragState { start: pos, current: pos });
            state.bbox_error = None;
        } else if resp.dragged()
            && let Some(d) = state.drag.as_mut()
        {
            d.current = pos;
        }
    }
    if resp.drag_stopped()
        && let Some(d) = state.drag.take()
    {
        match BBox::from_corners(d.start, d.current) {
            Ok(bbox) => {
                state.bbox = Some(bbox);
                state.bbox_error = None;
                // Only a successful box exits draw mode.
                state.draw_mode = false;
            }
            Err(rejection) => {
                state.bbox = None;
                state.bbox_error = Some(rejection.message().to_owned());
                // Stay armed so a too-small drag can be retried immediately
                // without re-toggling Draw Box.
            }
        }
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
