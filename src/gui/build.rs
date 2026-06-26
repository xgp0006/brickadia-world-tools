//! Build orchestrator: bbox → DEM fetch → decode → bricks → .brdb → install.
//!
//! DEM paths: AWS Terrarium and Mapbox Terrain-RGB via the shared XYZ tile
//! stitcher; OpenTopography SRTMGL1 via its single-shot bbox GeoTIFF REST API.
//! Imagery (ESRI World Imagery, Mapbox Satellite) supplies the colormap.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tiff::decoder::{Decoder, DecodingResult};

use image::RgbaImage;
use log::info;

use crate::map::{Colormap, Heightmap};
use crate::opt::gen_greedy_heightmap;
use crate::util::{GenOptions, bricks_to_save, to_linear_rgb};

use super::dem_sources::{
    DemSource, RequiredKey, decode_pixel_for, tile_source_for as dem_tile_source_for,
};
use super::imagery_sources::{ImagerySource, tile_source_for as imagery_tile_source_for};
use super::tiles::{BBoxLatLon, TileFetchError, fetch_bbox};

/// Brickadia Proton-prefix App ID for the Worlds/ install path lookup.
const BRICKADIA_APP_ID: u32 = 2199420;
/// Default brick color used by the flat colormap when no imagery source is
/// selected. A warm slate-green that reads well against the OSM basemap.
const DEFAULT_BRICK_COLOR: [u8; 4] = [0x9A, 0xA3, 0x7E, 0xFF];
/// Upper bound on generated brick count. A 64-tile high-relief fetch at a
/// large vertical scale can produce millions of bricks (heavy save + memory);
/// past this we return a checked error guiding the user to shrink the area or
/// lower the vertical exaggeration, rather than writing a multi-million-brick
/// save. The validated Horsetooth builds were ~70k, so this is a generous
/// backstop. NOTE: under the greedy mesher (the GUI path) brick *count* tracks
/// the DEM grid and vertical span. To reduce the count, lower the vertical
/// exaggeration or raise the density factor (which downsamples the DEM grid).
pub(crate) const MAX_BRICKS: usize = 2_000_000;
/// Aggregate cap on the COMBINED grid accumulator (`run_grid_build`). The
/// per-tile `MAX_BRICKS` cap must NOT gate the stitched world — exceeding it is
/// the whole point of tiling — so a separate, much larger ceiling guards the
/// combined `Vec<Brick>` against an unbounded stitch (and the single-threaded
/// all-in-RAM `.brdb` write that follows). Each tile still respects
/// `enforce_cell_budget` so no single tile OOMs the mesher.
pub(crate) const MAX_GRID_BRICKS: usize = 50_000_000;
/// Upper bound on `vertical_scale`. The Map tab DERIVES vertical_scale via
/// `derive_scale`, whose structural maximum is `studs_per_meter_max(32) * 5 *
/// exaggeration_max(8) = 1280` (the `2*hscale*upf/cell_m` form collapses to
/// `studs_per_meter*5*exaggeration` once hscale tracks the scale). This cap sits
/// ABOVE that (2000) so a faithful 1:1 build or any in-range exaggeration is
/// NEVER silently compressed — it only guards an out-of-contract BuildRequest
/// from overflowing the i32 brick-height math. The practical ceiling on a tall
/// build is MAX_BRICKS, not this. (Was 64, which silently capped the 1:1 feature.)
pub(crate) const MAX_VERTICAL_SCALE: f32 = 2000.0;

/// Brick render style for the generated terrain. Selects the brick `asset`
/// plus the `stud`/`micro` geometry flags in lockstep — these are not purely
/// cosmetic: they change `base_height()` (stud→5, micro→1, else→2) and the
/// vertical step of the mesh, so they must stay internally consistent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockType {
    SmoothTile,
    Tile,
    Brick,
    Studded,
    Micro,
}

impl BlockType {
    pub(crate) const ALL: [BlockType; 5] =
        [Self::SmoothTile, Self::Tile, Self::Brick, Self::Studded, Self::Micro];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::SmoothTile => "Smooth tile (flat top)",
            Self::Tile => "Tile",
            Self::Brick => "Brick",
            Self::Studded => "Studded",
            Self::Micro => "Micro brick (fine detail)",
        }
    }

    /// brdb brick asset for this style. Returns a fresh owned `BString`
    /// (`BString` is `Clone`, not `Copy`), so callers move it into `GenOptions`.
    fn asset(self) -> brdb::BString {
        use brdb::assets::bricks::{
            PB_DEFAULT_BRICK, PB_DEFAULT_MICRO_BRICK, PB_DEFAULT_SMOOTH_TILE, PB_DEFAULT_STUDDED,
            PB_DEFAULT_TILE,
        };
        match self {
            Self::SmoothTile => PB_DEFAULT_SMOOTH_TILE,
            Self::Tile => PB_DEFAULT_TILE,
            Self::Brick => PB_DEFAULT_BRICK,
            Self::Studded => PB_DEFAULT_STUDDED,
            Self::Micro => PB_DEFAULT_MICRO_BRICK,
        }
    }

    const fn stud(self) -> bool {
        matches!(self, Self::Studded)
    }

    pub(crate) const fn micro(self) -> bool {
        matches!(self, Self::Micro)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BuildRequest {
    pub(crate) bbox: BBoxLatLon,
    pub(crate) name: String,
    pub(crate) dem_source: DemSource,
    pub(crate) imagery_source: ImagerySource,
    pub(crate) mapbox_token: Option<String>,
    pub(crate) opentopo_key: Option<String>,
    pub(crate) vertical_scale: f32,
    /// Integer DEM-grid downsample factor (1 = full detail). Higher = fewer,
    /// coarser bricks (~1/factor² count). Applied in `downsample` before the
    /// heightmap is built; the greedy mesher itself always runs at `size = 1`.
    pub(crate) density_factor: u16,
    /// Studs of brick width per DEM cell (1 = legacy 1 stud/cell). Multiplies
    /// brick XY footprint and position, NOT brick count — the way to make a
    /// small real-world area walkable-scale. SRTMGL1's ~30 m cells at 1
    /// stud/cell make a 1 km box ~33 studs wide; raise this (and vertical
    /// exaggeration to match) for playable proportions.
    pub(crate) horizontal_scale: u16,
    pub(crate) block_type: BlockType,
    pub(crate) glow: bool,
    pub(crate) no_collision: bool,
    pub(crate) install_to_brickadia: bool,
    /// Overwrite `<name>.brdb` in Worlds/ instead of suffixing `-2`, `-3`, … .
    /// Lets a re-run of the same output name update the world already open
    /// in-game; default off preserves the never-clobber guarantee.
    pub(crate) overwrite_world: bool,
    /// Omit-below level in METRES above the area's lowest point — a column whose
    /// (min-relative) source height is at or below it emits no bricks, so the
    /// native Brickadia floor shows through. The "don't import the water level"
    /// knob; mirrors the sculpt convert's `omit_below_m`. Default `0.0` drops
    /// only true-floor columns (byte-identical to the prior map build).
    pub(crate) omit_below_m: f32,
    /// Floor level in METRES above the lowest point that every emitted column
    /// fills DOWN to (a raised base plane shortens fills). Mirrors the sculpt
    /// convert's `floor_level_m`. Default `0.0` fills to the area minimum
    /// (byte-identical to the prior map build).
    pub(crate) floor_level_m: f32,
}

impl BuildRequest {
    fn dem_token(&self) -> Option<&str> {
        token_for(
            self.dem_source.required_key(),
            self.mapbox_token.as_deref(),
            self.opentopo_key.as_deref(),
        )
    }
    fn imagery_token(&self) -> Option<&str> {
        token_for(
            self.imagery_source.required_key(),
            self.mapbox_token.as_deref(),
            self.opentopo_key.as_deref(),
        )
    }
}

fn token_for<'a>(
    required: Option<RequiredKey>,
    mapbox: Option<&'a str>,
    opentopo: Option<&'a str>,
) -> Option<&'a str> {
    match required {
        Some(RequiredKey::MapboxToken) => mapbox,
        Some(RequiredKey::OpenTopoApiKey) => opentopo,
        None => None,
    }
}

#[derive(Debug)]
pub(crate) struct BuildOutcome {
    pub(crate) brdb_path: PathBuf,
    pub(crate) installed_path: Option<PathBuf>,
    /// Non-fatal note when the `.brdb` was written but auto-install was skipped
    /// (e.g. the Brickadia Proton prefix does not exist yet). The save still
    /// exists at `brdb_path` for manual import.
    pub(crate) install_warning: Option<String>,
    pub(crate) brick_count: usize,
    pub(crate) dem_width: u32,
    pub(crate) dem_height: u32,
    pub(crate) elevation_min_m: f32,
    pub(crate) elevation_max_m: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuildStage {
    FetchingTiles,
    DecodingDem,
    FetchingImagery,
    ResamplingImagery,
    GeneratingBricks,
    WritingSave,
    Installing,
}

impl BuildStage {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::FetchingTiles => "Fetching elevation tiles…",
            Self::DecodingDem => "Decoding heights…",
            Self::FetchingImagery => "Fetching satellite imagery…",
            Self::ResamplingImagery => "Aligning imagery to terrain…",
            Self::GeneratingBricks => "Generating bricks…",
            Self::WritingSave => "Writing save…",
            Self::Installing => "Installing into Brickadia…",
        }
    }
}

#[derive(Debug)]
pub(crate) enum BuildError {
    UnsupportedSource(DemSource),
    UnsupportedImagery(ImagerySource),
    TokenMissing { source_label: &'static str, key_name: &'static str },
    Fetch(TileFetchError),
    EmptyDem,
    TooManyBricks { count: usize, max: usize },
    /// Cropped DEM grid exceeds the cell budget that bounds greedy-mesh memory
    /// (`tiles::MAX_DEM_CELLS`). Reached in practice only via OpenTopography —
    /// the tile path is already capped by `pick_zoom` — so the remedy is more
    /// Density or a smaller box.
    GridTooLarge { cells: u64, max: u64 },
    WorldNameExhausted { stem: String },
    BrickGen(String),
    Io { stage: BuildStage, source: std::io::Error },
    BrdbWrite(String),
    NoBrickadiaPrefix(PathBuf),
    /// OpenTopography (or another single-shot REST DEM) failed to fetch or
    /// decode. Holds a user-facing message; the API key is never included.
    DemApi(String),
    Cancelled,
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedSource(s) => write!(
                f,
                "DEM source {s:?} is not yet wired for in-app fetch — pick AWS Terrarium or Mapbox Terrain-RGB"
            ),
            Self::UnsupportedImagery(s) => write!(
                f,
                "imagery source {s:?} is not yet wired — pick ESRI, Mapbox Satellite, or None"
            ),
            Self::TokenMissing { source_label, key_name } => write!(
                f,
                "{source_label} requires a {key_name} — set it under Settings → API keys"
            ),
            Self::EmptyDem => write!(
                f,
                "elevation data was empty after cropping — the bounding box is too small; draw a larger area"
            ),
            Self::TooManyBricks { count, max } => write!(
                f,
                "terrain produced {count} bricks (limit {max}) — raise Density, lower the vertical exaggeration, or shrink the bounding box"
            ),
            Self::GridTooLarge { cells, max } => write!(
                f,
                "elevation grid is {cells} cells (limit {max}) — raise Density or draw a smaller box (this bounds memory for the colored mesh)"
            ),
            Self::WorldNameExhausted { stem } => write!(
                f,
                "too many worlds named '{stem}' already installed (tried -2…-1000) — choose a different output name"
            ),
            Self::Fetch(e) => write!(f, "{e}"),
            Self::BrickGen(msg) => write!(f, "brick generation failed: {msg}"),
            Self::Io { stage, source } => write!(f, "{} ({})", stage.label(), source),
            Self::BrdbWrite(msg) => write!(f, "failed to write .brdb: {msg}"),
            Self::NoBrickadiaPrefix(p) => write!(
                f,
                "Brickadia Proton prefix not found at {} — launch Brickadia at least once",
                p.display()
            ),
            Self::DemApi(msg) => write!(f, "OpenTopography DEM fetch failed: {msg}"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for BuildError {}

pub(crate) type ProgressFn =
    Arc<dyn Fn(BuildStage, f32) + Send + Sync + 'static>;

/// Reject a cropped DEM grid whose cell count would blow the greedy-mesh memory
/// budget BEFORE the mesher allocates its per-(height,color) planes (which can
/// reach GBs with per-pixel-unique imagery colors). The tile path is already
/// bounded by `pick_zoom`; this backstops OpenTopography's single-shot GeoTIFF
/// and is defense-in-depth for everything else.
pub(crate) fn enforce_cell_budget(width: u32, height: u32) -> Result<(), BuildError> {
    let cells = u64::from(width) * u64::from(height);
    if cells > super::tiles::MAX_DEM_CELLS {
        return Err(BuildError::GridTooLarge { cells, max: super::tiles::MAX_DEM_CELLS });
    }
    Ok(())
}

/// World-unit footprint of one DEM cell on one axis. Mirrors the `size` that
/// `generate_bricks` feeds `GenOptions` (`horizontal_scale * (1 micro | 5)`),
/// kept in lockstep so the centered single-box offset and the grid world
/// offsets are computed against the SAME pitch the mesher emits.
pub(crate) fn cell_size_units(style_horizontal_scale: u16, micro: bool) -> u16 {
    style_horizontal_scale.max(1) * if micro { 1 } else { 5 }
}

pub(crate) fn run_build(
    request: BuildRequest,
    progress: ProgressFn,
    cancel: Arc<AtomicBool>,
) -> Result<BuildOutcome, BuildError> {
    let raster = fetch_and_decode_dem(&request, Arc::clone(&progress), Arc::clone(&cancel))?;
    // Density = integer DEM-grid downsample, applied before everything that
    // reads the raster (heightmap, imagery target dims, reported dims/elevation)
    // so every downstream consumer sees the reduced grid consistently.
    let raster = downsample(&raster, u32::from(request.density_factor.max(1)));
    // Bound mesh memory before building planes/heightmap (OpenTopography can
    // return a grid larger than pick_zoom would ever choose).
    enforce_cell_budget(raster.width, raster.height)?;

    // Single-box placement: center the build on the origin exactly as the
    // pre-grid `quads_to_bricks` did — `offset = -(width*size), -(height*size)`.
    // The grid orchestrator computes a per-tile world offset instead and calls
    // `build_one_tile` directly. global_min = raster.min_m and base_override =
    // None make this path byte-identical to the pre-grid behavior.
    let size = i32::from(cell_size_units(request.horizontal_scale, request.block_type.micro()));
    let offset = (-(raster.width as i32 * size), -(raster.height as i32 * size));
    let dem_width = raster.width;
    let dem_height = raster.height;
    let elevation_min_m = raster.min_m;
    let elevation_max_m = raster.max_m;
    let global_min_m = raster.min_m;

    let bricks = build_one_tile(
        &request,
        raster,
        global_min_m,
        offset,
        None,
        Arc::clone(&progress),
        Arc::clone(&cancel),
    )?;

    if bricks.len() > MAX_BRICKS {
        return Err(BuildError::TooManyBricks { count: bricks.len(), max: MAX_BRICKS });
    }
    // Capture the count before moving `bricks` into write_brdb — avoids the
    // full deep-clone of the Vec<Brick> just to read its length afterward.
    let brick_count = bricks.len();

    progress(BuildStage::WritingSave, 0.0);
    let brdb_path = write_brdb(&request.name, bricks)?;
    progress(BuildStage::WritingSave, 1.0);

    let mut install_warning = None;
    let installed_path = if request.install_to_brickadia {
        progress(BuildStage::Installing, 0.0);
        // Any install failure is non-fatal: the .brdb is already on disk, so
        // degrade to "wrote but did not install" + a warning instead of
        // throwing away a successful build (the user can import it manually).
        match install_save(&brdb_path, "brdb", request.overwrite_world) {
            Ok(dest) => Some(dest),
            Err(BuildError::NoBrickadiaPrefix(prefix)) => {
                install_warning = Some(format!(
                    "Brickadia prefix not found at {} — launch Brickadia once, then import {} manually",
                    prefix.display(),
                    brdb_path.display()
                ));
                None
            }
            Err(e) => {
                install_warning = Some(format!(
                    "install into Brickadia failed ({e}) — the save was still written to {}; import it manually",
                    brdb_path.display()
                ));
                None
            }
        }
    } else {
        None
    };

    Ok(BuildOutcome {
        brdb_path,
        installed_path,
        install_warning,
        brick_count,
        dem_width,
        dem_height,
        elevation_min_m,
        elevation_max_m,
    })
}

/// Mesh ONE already-decoded (downsampled, budget-checked) tile into bricks.
/// Factored out of [`run_build`] so the grid orchestrator can mesh each tile
/// through the SAME code path with a per-tile world `offset`, a shared
/// `global_min_m` datum, and `base_override = Some(0)` for a global fill floor.
/// `run_build` is the single-tile special case (global_min = raster.min_m,
/// centered offset, base_override = None). Consumes `raster` — the caller has
/// already captured any dims/elevation it needs to report.
pub(crate) fn build_one_tile(
    request: &BuildRequest,
    raster: DemRaster,
    global_min_m: f32,
    offset: (i32, i32),
    base_override: Option<u32>,
    progress: ProgressFn,
    cancel: Arc<AtomicBool>,
) -> Result<Vec<brdb::Brick>, BuildError> {
    // Clamp both ends (Rule 2/3): the GUI slider already caps at 20, but a
    // BuildRequest can be constructed elsewhere — clamp only as an out-of-contract
    // i32-overflow guard. The ceiling (MAX_VERTICAL_SCALE = 2000) sits ABOVE
    // derive_scale's structural max (1280), so a faithful 1:1 build and any
    // in-range exaggeration pass through UNCLAMPED (the old 64 silently broke 1:1).
    let vscale = request.vertical_scale.clamp(0.01, MAX_VERTICAL_SCALE);
    let heightmap = build_heightmap(&raster, vscale, global_min_m);
    let imagery = fetch_imagery_if_requested(
        request,
        (raster.width, raster.height),
        Arc::clone(&progress),
        Arc::clone(&cancel),
    )?;

    // Omit-water + floor-level parity with the sculpt convert: convert the
    // metres-above-minimum knobs to brick-Z against the SAME vertical scale
    // build_heightmap used, so the decisions are made in source-metre space.
    // `omit_below_m > 0` turns on skip_floor (columns at/below the level drop,
    // revealing the native floor); `omit_below_m == 0` keeps the prior build
    // byte-identical (skip_floor off, omit threshold 0). skip_floor requires an
    // explicit base plane — the single-box caller passes `None`, which maps to
    // brick-Z 0 here since build_heightmap already floors at the area minimum.
    let omit_below_h = (request.omit_below_m.max(0.0) * vscale).round() as u32;
    let floor_h = (request.floor_level_m.max(0.0) * vscale).round() as u32;
    let skip_floor = request.omit_below_m > 0.0;
    let base = match base_override {
        Some(b) => Some(b.saturating_add(floor_h)),
        None if skip_floor || floor_h > 0 => Some(floor_h),
        None => None,
    };

    progress(BuildStage::GeneratingBricks, 0.0);
    let style = BrickStyle::from_request(request);
    let bricks = match &imagery {
        Some(im) => generate_bricks_skip_floor(
            &heightmap, im, style, base, offset, skip_floor, omit_below_h,
            Arc::clone(&progress), Arc::clone(&cancel), None,
        )?,
        None => {
            let flat = FlatColormap {
                width: raster.width,
                height: raster.height,
                color: DEFAULT_BRICK_COLOR,
            };
            generate_bricks_skip_floor(
                &heightmap, &flat, style, base, offset, skip_floor, omit_below_h,
                Arc::clone(&progress), Arc::clone(&cancel), None,
            )?
        }
    };
    Ok(bricks)
}

pub(crate) fn fetch_and_decode_dem(
    request: &BuildRequest,
    progress: ProgressFn,
    cancel: Arc<AtomicBool>,
) -> Result<DemRaster, BuildError> {
    let token = request.dem_token();
    require_token_if_needed(
        request.dem_source.required_key(),
        token,
        dem_source_label(request.dem_source),
    )?;
    // OpenTopography is a single-shot bbox GeoTIFF REST API, not an XYZ tile
    // source, so it bypasses the slippy-tile fetch/stitch path entirely.
    if request.dem_source == DemSource::OpenTopography {
        let key = token.ok_or(BuildError::TokenMissing {
            source_label: dem_source_label(request.dem_source),
            key_name: "OpenTopography API key",
        })?;
        return fetch_opentopo_dem(request.bbox, key, &progress, &cancel);
    }
    let source = dem_tile_source_for(request.dem_source, token)
        .ok_or(BuildError::UnsupportedSource(request.dem_source))?;
    progress(BuildStage::FetchingTiles, 0.0);
    let p_for_fetch = Arc::clone(&progress);
    let stitched = fetch_bbox(
        request.bbox,
        source.as_ref(),
        None,
        &move |f| p_for_fetch(BuildStage::FetchingTiles, f),
        &cancel,
    )
    .map_err(BuildError::Fetch)?;
    info!(
        "DEM tiles: zoom={} nw=({},{}) canvas={}×{}",
        stitched.zoom,
        stitched.nw_tile.x,
        stitched.nw_tile.y,
        stitched.image.width(),
        stitched.image.height(),
    );
    progress(BuildStage::DecodingDem, 0.0);
    let raster = decode_to_raster(&stitched.cropped(), request.dem_source)?;
    progress(BuildStage::DecodingDem, 1.0);
    Ok(raster)
}

/// OpenTopography global DEM type. SRTMGL1 is 1 arc-second (~30 m) global SRTM,
/// the best free global resolution OpenTopography serves without an academic
/// account.
const OPENTOPO_DEMTYPE: &str = "SRTMGL1";
/// OpenTopography per-request area cap for SRTMGL1. Their API docs
/// (portal.opentopography.org/apidocs) state: "Requests are limited to …
/// 450,000 km² for all other data" — SRTMGL1 falls in "all other data".
/// Exceeding it returns an HTTP 400; we reject earlier with a clear message.
pub(crate) const OPENTOPO_MAX_AREA_KM2: f64 = 450_000.0;
const OPENTOPO_TIMEOUT: Duration = Duration::from_secs(60);
/// Hard cap on the OpenTopography response body. A 450,000 km² SRTMGL1
/// GeoTIFF is well under this; anything bigger is rejected explicitly rather
/// than silently truncated into a corrupt-looking decode error.
const OPENTOPO_MAX_BODY_BYTES: u64 = 64 * 1024 * 1024;
/// SRTM void/nodata sentinel. Cells at or below this are missing data.
const SRTM_NODATA: f32 = -32768.0;
/// Per-side cap on GeoTIFF dimensions. `OPENTOPO_MAX_BODY_BYTES` bounds the
/// *compressed* payload, but the header dims are server-controlled and drive
/// the decoded allocation directly; a corrupt/hostile header could claim huge
/// dims inside a tiny body. 50,000 px per side comfortably exceeds any SRTMGL1
/// response under the 450,000 km² area cap (~3,600 px/degree → well under
/// 25,000 px per side).
const MAX_DEM_DIM: u32 = 50_000;
/// Cap on total decoded pixels: 100 M × 4-byte f32 ≈ 400 MB worst-case
/// allocation. Rejected before `read_image` allocates anything.
const MAX_DEM_PIXELS: u64 = 100_000_000;

/// Rule-3 bound on untrusted GeoTIFF header dimensions, checked BEFORE the
/// decode allocates `w×h` samples. Zero-sized rasters are also rejected here
/// (they would otherwise fall through to a confusing `EmptyDem`).
fn validate_dem_dims(w: u32, h: u32) -> Result<(), BuildError> {
    if w == 0 || h == 0 {
        return Err(BuildError::DemApi(format!(
            "GeoTIFF reports empty dimensions {w}×{h}"
        )));
    }
    if w > MAX_DEM_DIM || h > MAX_DEM_DIM {
        return Err(BuildError::DemApi(format!(
            "GeoTIFF dimensions {w}×{h} exceed the {MAX_DEM_DIM} px per-side limit — draw a smaller box"
        )));
    }
    if u64::from(w) * u64::from(h) > MAX_DEM_PIXELS {
        return Err(BuildError::DemApi(format!(
            "GeoTIFF {w}×{h} exceeds the {MAX_DEM_PIXELS}-pixel decode limit — draw a smaller box"
        )));
    }
    Ok(())
}

/// Fetch a single-shot bbox GeoTIFF from the OpenTopography globaldem REST API
/// and decode it to a [`DemRaster`]. Unlike the XYZ tile sources, this is one
/// HTTP request for the whole area; the API key travels as a query param (via
/// ureq's percent-encoding) and is never interpolated into an error/log string.
fn fetch_opentopo_dem(
    bbox: BBoxLatLon,
    key: &str,
    progress: &ProgressFn,
    cancel: &AtomicBool,
) -> Result<DemRaster, BuildError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(BuildError::Cancelled);
    }
    // Reject an over-cap bbox before the network call rather than letting
    // OpenTopography 400 on it — the limit is a real, documented API constraint.
    let area = bbox_area_km2(&bbox);
    if area > OPENTOPO_MAX_AREA_KM2 {
        return Err(BuildError::DemApi(format!(
            "selected area is {area:.0} km², over OpenTopography's {OPENTOPO_MAX_AREA_KM2:.0} km² per-request limit for SRTMGL1 — draw a smaller box or use AWS Terrarium / Mapbox"
        )));
    }
    progress(BuildStage::FetchingTiles, 0.0);
    let agent = ureq::AgentBuilder::new()
        .timeout(OPENTOPO_TIMEOUT)
        .user_agent(super::USER_AGENT)
        .build();
    let resp = agent
        .get("https://portal.opentopography.org/API/globaldem")
        .query("demtype", OPENTOPO_DEMTYPE)
        .query("south", &bbox.south.to_string())
        .query("north", &bbox.north.to_string())
        .query("west", &bbox.west.to_string())
        .query("east", &bbox.east.to_string())
        .query("outputFormat", "GTiff")
        .query("API_Key", key)
        .call();
    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            // OpenTopo returns a plaintext reason for 4xx (bad/too-large bbox,
            // bad key). Surface it, never the key-bearing request URL.
            let body = r.into_string().unwrap_or_default();
            let body = body.trim();
            let detail = if body.is_empty() {
                String::new()
            } else {
                format!(": {body}")
            };
            return Err(BuildError::DemApi(format!("HTTP {code}{detail}")));
        }
        // ureq's Transport Display embeds the key-bearing request URL — redact.
        Err(e) => {
            return Err(BuildError::DemApi(format!(
                "network error: {}",
                super::tiles::redact_secrets(&e.to_string())
            )));
        }
    };
    // Residual guard for a non-200 success status (3xx past the redirect cap).
    if resp.status() != 200 {
        return Err(BuildError::DemApi(format!("HTTP {}", resp.status())));
    }
    progress(BuildStage::DecodingDem, 0.0);
    let mut bytes = Vec::new();
    // Read one byte past the cap so an oversized body is a loud error instead
    // of a silent truncation that surfaces as a baffling GeoTIFF decode error.
    resp.into_reader()
        .take(OPENTOPO_MAX_BODY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| BuildError::DemApi(format!("reading response: {e}")))?;
    if bytes.len() as u64 > OPENTOPO_MAX_BODY_BYTES {
        return Err(BuildError::DemApi(format!(
            "response exceeds {} MiB — draw a smaller box",
            OPENTOPO_MAX_BODY_BYTES / (1024 * 1024)
        )));
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(BuildError::Cancelled);
    }
    let raster = decode_geotiff_dem(&bytes)?;
    progress(BuildStage::DecodingDem, 1.0);
    Ok(raster)
}

/// Approximate area of a lat/lon bbox in km², via the equirectangular metric:
/// latitude degrees are ~110.574 km, longitude degrees shrink by cos(midlat).
/// Good to a few percent at DEM scales — fine for an API quota guard.
pub(crate) fn bbox_area_km2(bbox: &BBoxLatLon) -> f64 {
    const KM_PER_DEG_LAT: f64 = 110.574;
    const KM_PER_DEG_LON_EQUATOR: f64 = 111.320;
    let mid_lat_rad = ((bbox.north + bbox.south) * 0.5).to_radians();
    let height_km = (bbox.north - bbox.south) * KM_PER_DEG_LAT;
    let width_km = (bbox.east - bbox.west) * KM_PER_DEG_LON_EQUATOR * mid_lat_rad.cos();
    (height_km * width_km).abs()
}

/// Decode a single-band elevation GeoTIFF (any int/float storage) to a
/// [`DemRaster`] in meters. SRTM void cells (`-32768`) are floored to the valid
/// minimum so the downstream zero-base does not sink the whole terrain.
fn decode_geotiff_dem(bytes: &[u8]) -> Result<DemRaster, BuildError> {
    let mut dec = Decoder::new(Cursor::new(bytes))
        .map_err(|e| BuildError::DemApi(format!("not a readable GeoTIFF: {e:?}")))?;
    let (w, h) = dec
        .dimensions()
        .map_err(|e| BuildError::DemApi(format!("GeoTIFF dimensions: {e:?}")))?;
    validate_dem_dims(w, h)?;
    let img = dec
        .read_image()
        .map_err(|e| BuildError::DemApi(format!("decoding GeoTIFF raster: {e:?}")))?;
    let heights_raw: Vec<f32> = match img {
        DecodingResult::I16(v) => v.into_iter().map(|x| x as f32).collect(),
        DecodingResult::U16(v) => v.into_iter().map(|x| x as f32).collect(),
        DecodingResult::I32(v) => v.into_iter().map(|x| x as f32).collect(),
        DecodingResult::F32(v) => v,
        DecodingResult::F64(v) => v.into_iter().map(|x| x as f32).collect(),
        _ => {
            return Err(BuildError::DemApi(
                "unsupported GeoTIFF sample format (expected 16/32-bit int or float)".to_owned(),
            ));
        }
    };
    let expected = (w as usize) * (h as usize);
    if heights_raw.len() != expected {
        return Err(BuildError::DemApi(format!(
            "GeoTIFF pixel count {} does not match {w}×{h}",
            heights_raw.len()
        )));
    }
    // Nodata test must catch non-finite samples too: F32 GeoTIFFs commonly use
    // NaN as nodata, and `NaN <= x` is false — without the finiteness check NaN
    // would slip through here and silently punch height-0 holes in the terrain.
    let is_nodata = |m: f32| !m.is_finite() || m <= SRTM_NODATA + 1.0;
    let mut min_m = f32::INFINITY;
    let mut max_m = f32::NEG_INFINITY;
    for &m in &heights_raw {
        if is_nodata(m) {
            continue;
        }
        min_m = min_m.min(m);
        max_m = max_m.max(m);
    }
    // All-nodata (e.g. an all-ocean SRTM void bbox) is unbuildable.
    if !min_m.is_finite() || !max_m.is_finite() {
        return Err(BuildError::EmptyDem);
    }
    let heights_m: Vec<f32> =
        heights_raw.into_iter().map(|m| if is_nodata(m) { min_m } else { m }).collect();
    Ok(DemRaster { width: w, height: h, heights_m, min_m, max_m })
}

pub(crate) fn fetch_imagery_if_requested(
    request: &BuildRequest,
    target_dims: (u32, u32),
    progress: ProgressFn,
    cancel: Arc<AtomicBool>,
) -> Result<Option<ImageColormap>, BuildError> {
    if request.imagery_source == ImagerySource::None {
        return Ok(None);
    }
    let token = request.imagery_token();
    require_token_if_needed(
        request.imagery_source.required_key(),
        token,
        imagery_source_label(request.imagery_source),
    )?;
    let source = imagery_tile_source_for(request.imagery_source, token)
        .ok_or(BuildError::UnsupportedImagery(request.imagery_source))?;
    progress(BuildStage::FetchingImagery, 0.0);
    let p_for_fetch = Arc::clone(&progress);
    let stitched = fetch_bbox(
        request.bbox,
        source.as_ref(),
        None,
        &move |f| p_for_fetch(BuildStage::FetchingImagery, f),
        &cancel,
    )
    .map_err(BuildError::Fetch)?;
    info!(
        "imagery tiles: zoom={} canvas={}×{}",
        stitched.zoom,
        stitched.image.width(),
        stitched.image.height(),
    );
    progress(BuildStage::ResamplingImagery, 0.0);
    let cropped = stitched.cropped();
    let resampled = image::imageops::resize(
        &cropped,
        target_dims.0,
        target_dims.1,
        image::imageops::FilterType::Lanczos3,
    );
    progress(BuildStage::ResamplingImagery, 1.0);
    Ok(Some(ImageColormap::from_rgba(resampled)))
}

#[derive(Clone, Debug)]
pub(crate) struct DemRaster {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) heights_m: Vec<f32>,
    pub(crate) min_m: f32,
    pub(crate) max_m: f32,
}

/// Box-mean downsample of the elevation grid by an integer `factor`. This is
/// the real "density" control: it reduces the cell count to
/// `ceil(w/factor) × ceil(h/factor)`, which cuts the generated brick count
/// ~1/factor² while preserving terrain proportions (both XY extent and per-cell
/// height stay in fixed `size=1` brick units). `factor == 1` is the identity
/// (byte-identical to the input — the validated full-detail path is untouched).
///
/// `min_m`/`max_m` are recomputed from the REDUCED cells: `build_heightmap`
/// subtracts `min_m` for its zero-floor, so the extremes must match the cells
/// actually emitted. Averaging smooths the extremes slightly, so the reported
/// elevation range narrows a little at higher factors — correct, not a bug.
pub(crate) fn downsample(raster: &DemRaster, factor: u32) -> DemRaster {
    debug_assert!(factor >= 1, "downsample factor must be >= 1");
    if factor <= 1 {
        return raster.clone();
    }
    let f = factor as usize;
    let (w, h) = (raster.width as usize, raster.height as usize);
    let out_w = w.div_ceil(f);
    let out_h = h.div_ceil(f);
    let mut heights = Vec::with_capacity(out_w * out_h);
    let mut min_m = f32::INFINITY;
    let mut max_m = f32::NEG_INFINITY;
    for by in 0..out_h {
        for bx in 0..out_w {
            let mut sum = 0.0f32;
            let mut cnt = 0u32;
            for dy in 0..f {
                let y = by * f + dy;
                if y >= h {
                    break;
                }
                for dx in 0..f {
                    let x = bx * f + dx;
                    if x >= w {
                        break;
                    }
                    sum += raster.heights_m[y * w + x];
                    cnt += 1;
                }
            }
            // cnt >= 1 always: (bx,by) only iterate over blocks that contain at
            // least the (bx*f, by*f) cell, which is in-bounds by construction.
            let m = sum / cnt as f32;
            min_m = min_m.min(m);
            max_m = max_m.max(m);
            heights.push(m);
        }
    }
    DemRaster {
        width: out_w as u32,
        height: out_h as u32,
        heights_m: heights,
        min_m,
        max_m,
    }
}

fn decode_to_raster(image: &RgbaImage, source: DemSource) -> Result<DemRaster, BuildError> {
    let (w, h) = (image.width(), image.height());
    let mut heights = Vec::with_capacity((w * h) as usize);
    let mut min_m = f32::INFINITY;
    let mut max_m = f32::NEG_INFINITY;
    for y in 0..h {
        for x in 0..w {
            let px = image.get_pixel(x, y).0;
            let m = decode_pixel_for(source, px).ok_or(BuildError::UnsupportedSource(source))?;
            heights.push(m);
            if m < min_m { min_m = m; }
            if m > max_m { max_m = m; }
        }
    }
    // Real release guard (not debug_assert): a zero-pixel image leaves the
    // extremes at +/-INFINITY and heights empty. Reject as a checked error
    // rather than emitting a zero-brick, INFINITY-elevation save. crop_window
    // now prevents a zero-size crop upstream; this is defense in depth.
    if heights.is_empty() || !min_m.is_finite() || !max_m.is_finite() {
        return Err(BuildError::EmptyDem);
    }
    debug_assert!(max_m >= min_m, "decoded max must be >= min");
    Ok(DemRaster { width: w, height: h, heights_m: heights, min_m, max_m })
}

/// Normalize a raster's meter heights to u32 brick heights against a reference
/// minimum. The single-box caller passes `global_min_m = raster.min_m` (the
/// per-raster minimum → byte-identical to the pre-grid behavior). Grid mode
/// passes the GLOBAL minimum across all tiles so every tile shares one datum:
/// two tiles at the same real elevation map to the same brick-Z (pillar B).
pub(crate) fn build_heightmap(
    raster: &DemRaster,
    vertical_scale: f32,
    global_min_m: f32,
) -> DemHeightmap {
    debug_assert!(vertical_scale > 0.0, "vertical_scale must be positive");
    let normalized: Vec<u32> = raster
        .heights_m
        .iter()
        .map(|m| (((m - global_min_m) * vertical_scale).max(0.0).round() as i64).max(0) as u32)
        .collect();
    DemHeightmap {
        width: raster.width,
        height: raster.height,
        values: normalized,
    }
}

/// Brick-shaping knobs for `generate_bricks`, lifted off `BuildRequest` so
/// tests can drive generation without constructing a full network request.
#[derive(Clone, Copy)]
pub(crate) struct BrickStyle {
    block_type: BlockType,
    horizontal_scale: u16,
    glow: bool,
    nocollide: bool,
}

impl BrickStyle {
    pub(crate) fn from_request(request: &BuildRequest) -> Self {
        Self {
            block_type: request.block_type,
            horizontal_scale: request.horizontal_scale,
            glow: request.glow,
            nocollide: request.no_collision,
        }
    }

    /// Build a style directly from shaping knobs, for callers (the sculpt
    /// convert) that shape a [`HeightField`] rather than fetch a `BuildRequest`.
    pub(crate) fn new(
        block_type: BlockType,
        horizontal_scale: u16,
        glow: bool,
        nocollide: bool,
    ) -> Self {
        Self { block_type, horizontal_scale, glow, nocollide }
    }
}

/// `base_override`: `Some(b)` fills every column down to brick-Z `b` (grid mode
/// passes `Some(0)` so all tiles share the global floor); `None` keeps the
/// per-tile present minimum (single-box behavior). `offset`: world placement in
/// units — the single-box caller passes the centered `-(width*size),
/// -(height*size)`; grid mode passes a per-tile world offset so tiles abut.
///
/// Test-only convenience wrapper (skip_floor = false): the map/grid build paths
/// call [`generate_bricks_skip_floor`] directly so they can thread the
/// omit-water / floor-level knobs, leaving this as the no-skip shorthand tests use.
#[cfg(test)]
pub(crate) fn generate_bricks(
    heightmap: &DemHeightmap,
    colormap: &dyn Colormap,
    style: BrickStyle,
    base_override: Option<u32>,
    offset: (i32, i32),
    progress: ProgressFn,
    cancel: Arc<AtomicBool>,
) -> Result<Vec<brdb::Brick>, BuildError> {
    // Single-box and grid map builds keep their watertight base plate: the floor
    // column is real ground there, so `skip_floor = false` keeps output
    // byte-identical (guarded by the identity tests). The sculpt convert calls
    // `generate_bricks_skip_floor(true)` instead so a blank canvas reveals the
    // native Brickadia floor.
    generate_bricks_skip_floor(
        heightmap, colormap, style, base_override, offset, false, 0, progress, cancel, None,
    )
}

/// As [`generate_bricks`], but with the additive `skip_floor` seam exposed: when
/// `true`, a column whose brick-Z height is at or below `omit_below_h` emits no
/// bricks (the native floor stands in). The sculpt/blank-canvas convert sets it
/// `true`; every map-build caller goes through `generate_bricks` (`false`), so
/// the map output is unaffected.
///
/// `omit_below_h` is the brick-Z omit threshold (the meter-space `omit_below_m`
/// pre-converted to brick-Z by the caller as `round(omit_below_m *
/// vertical_scale)`). With the sculpt default `0` and `base_override = Some(0)`
/// (so the base plane is brick-Z 0), only true-floor (`h == 0`) columns drop —
/// byte-identical to the prior `(h - min_height) == 0` skip. Has no effect when
/// `skip_floor` is `false`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn generate_bricks_skip_floor(
    heightmap: &DemHeightmap,
    colormap: &dyn Colormap,
    style: BrickStyle,
    base_override: Option<u32>,
    offset: (i32, i32),
    skip_floor: bool,
    omit_below_h: u32,
    progress: ProgressFn,
    cancel: Arc<AtomicBool>,
    keep_mask: Option<&[bool]>,
) -> Result<Vec<brdb::Brick>, BuildError> {
    let BrickStyle { block_type, horizontal_scale, glow, nocollide } = style;
    // `size` = stud→unit conversion (1 stud = 5 units; micro ×1) times the
    // user's studs-per-cell multiplier. It scales brick XY footprint AND
    // position, never count; the mesher's max_quad clamp (1000/size) shrinks
    // inversely so world-unit brick caps hold at any multiplier. Density is
    // handled upstream by `downsample`-ing the DEM grid. `scale: 1` because
    // vertical exaggeration is applied upstream in
    // `build_heightmap(vertical_scale)`; `cull/img/lrgb/snap/hdmap` change
    // pixel/height semantics that would break a terrain save if exposed — they
    // are not user knobs here.
    let options = GenOptions {
        size: horizontal_scale.max(1) * if block_type.micro() { 1 } else { 5 },
        scale: 1,
        cull: false,
        asset: block_type.asset(),
        micro: block_type.micro(),
        stud: block_type.stud(),
        snap: false,
        img: false,
        glow,
        hdmap: true,
        lrgb: false,
        nocollide,
        quadtree: false,
        greedy: true,
        // Map terrain is normalized to ~0 and capped by MAX_BRICKS, so fill each
        // column to the base plane for solid, walkable ground.
        fill_to_base: true,
        // Map/grid terrain keeps its watertight base plate (the floor column is
        // real ground here). Sculpt convert passes `true` so a blank canvas
        // reveals the native floor; default-off (every `generate_bricks` caller)
        // keeps single-box and grid output byte-identical.
        skip_floor,
        // Brick-Z omit threshold, derived meter-space by the caller. Default `0`
        // (the sculpt `omit_below_m = 0`) drops only true-floor columns.
        omit_below_h,
    };
    let cancel_check = move |f: f32| -> bool {
        progress(BuildStage::GeneratingBricks, f);
        !cancel.load(std::sync::atomic::Ordering::Relaxed)
    };
    // `options.greedy` is hardcoded true here, so call the greedy mesher
    // directly — it's the only path that takes a freedraw keep-mask, and going
    // through `gen_opt_heightmap` (which would always dispatch to greedy anyway)
    // would force every other `gen_opt_heightmap` caller to thread a `None`.
    gen_greedy_heightmap(
        heightmap,
        colormap,
        options,
        base_override,
        Some(offset),
        cancel_check,
        keep_mask,
    )
    .map_err(|e| {
        if e == crate::opt::CANCELLED_MSG {
            BuildError::Cancelled
        } else {
            BuildError::BrickGen(e)
        }
    })
}

fn write_brdb(name: &str, bricks: Vec<brdb::Brick>) -> Result<PathBuf, BuildError> {
    let builds_dir = builds_dir()?;
    std::fs::create_dir_all(&builds_dir).map_err(|e| BuildError::Io {
        stage: BuildStage::WritingSave,
        source: e,
    })?;
    let safe_name = sanitize_name(name);
    let brdb_path = builds_dir.join(format!("{safe_name}.brdb"));
    // Delete a stale destination before writing: `.brdb` is open-if-exists +
    // append (`Brdb::new` → `Brdb::open`, not truncate), so repeated same-name
    // single-box builds would pile revisions in the builds_dir copy. Mirror the
    // grid path's delete-then-write (spec correction #7); non-fatal if missing.
    if brdb_path.exists() {
        std::fs::remove_file(&brdb_path).map_err(|e| BuildError::Io {
            stage: BuildStage::WritingSave,
            source: e,
        })?;
    }
    let world = bricks_to_save(bricks);
    world
        .write_brdb(&brdb_path)
        .map_err(|e| BuildError::BrdbWrite(format!("{e:?}")))?;
    info!("wrote {} ({} bytes)", brdb_path.display(),
        std::fs::metadata(&brdb_path).map(|m| m.len()).unwrap_or(0));
    Ok(brdb_path)
}

/// Copy a written save into Brickadia's Saved tree under the subdir for `ext`,
/// returning the installed path. `.brdb` → `Worlds/`, `.brz` → `Prefabs/` (both
/// paths confirmed against a live install). `overwrite`
/// writes `<stem>.<ext>` in place so a re-run updates the world already open
/// in-game; default-off suffixes `-2`, `-3`, … so a hand-authored world is
/// never clobbered. Mirrors the previous `install_to_worlds` for `.brdb`.
pub(crate) fn install_save(path: &Path, ext: &str, overwrite: bool) -> Result<PathBuf, BuildError> {
    let dir = saved_subdir(ext)?;
    std::fs::create_dir_all(&dir).map_err(|e| BuildError::Io {
        stage: BuildStage::Installing,
        source: e,
    })?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| BuildError::BrdbWrite("output path has no file name".to_owned()))?;
    let dest = if overwrite {
        dir.join(format!("{stem}.{ext}"))
    } else {
        unique_save_path(&dir, stem, ext)?
    };
    std::fs::copy(path, &dest).map_err(|e| BuildError::Io {
        stage: BuildStage::Installing,
        source: e,
    })?;
    Ok(dest)
}

/// First non-colliding `<stem>.<ext>` / `<stem>-N.<ext>` path in `dir`.
/// Bounded loop (Rule 2): up to 1000 attempts. On exhaustion returns a checked
/// error — never an existing path, or a caller's `fs::copy` would silently
/// break the "never overwrite" guarantee.
pub(crate) fn unique_save_path(dir: &Path, stem: &str, ext: &str) -> Result<PathBuf, BuildError> {
    let first = dir.join(format!("{stem}.{ext}"));
    if !first.exists() {
        return Ok(first);
    }
    for n in 2..=1000 {
        let candidate = dir.join(format!("{stem}-{n}.{ext}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(BuildError::WorldNameExhausted { stem: stem.to_owned() })
}

/// Staging directory for generated saves, in the XDG data dir
/// (`~/.local/share/heightmap2brz/builds`) — not a hardcoded project path.
/// The GUI auto-installs `.brdb` into Brickadia's Worlds/, so this is just a
/// copy the user can re-import later (and the sole home for `.brz` prefabs).
pub(crate) fn builds_dir() -> Result<PathBuf, BuildError> {
    let base = dirs::data_dir().ok_or_else(|| BuildError::Io {
        stage: BuildStage::WritingSave,
        source: std::io::Error::other("no XDG data directory"),
    })?;
    Ok(base.join("heightmap2brz").join("builds"))
}

/// Root of Brickadia's `Saved` tree inside the Steam Proton prefix. Errors
/// `NoBrickadiaPrefix` if the prefix has never been created (game not yet run).
fn brickadia_saved_dir() -> Result<PathBuf, BuildError> {
    let home = dirs::home_dir().ok_or_else(|| BuildError::NoBrickadiaPrefix(PathBuf::new()))?;
    let prefix = home
        .join(".steam/steam/steamapps/compatdata")
        .join(BRICKADIA_APP_ID.to_string())
        .join("pfx/drive_c/users/steamuser/AppData/Local/Brickadia/Saved");
    if !prefix.exists() {
        return Err(BuildError::NoBrickadiaPrefix(prefix));
    }
    Ok(prefix)
}

/// Per-extension install subdir under `Saved`. `.brdb` → `Worlds/` (loaded as a
/// world); `.brz` → `Prefabs/` (loaded as a build/prefab). Both paths confirmed
/// against a live install: `Saved/Prefabs/*.brz` is exactly where the in-game
/// prefab browser reads from (resolves spec correction #8's open question).
fn saved_subdir(ext: &str) -> Result<PathBuf, BuildError> {
    match ext {
        "brdb" => Ok(brickadia_saved_dir()?.join("Worlds")),
        "brz" => Ok(brickadia_saved_dir()?.join("Prefabs")),
        other => Err(BuildError::BrdbWrite(format!(
            "no Brickadia install path for .{other} saves (only .brdb→Worlds/, .brz→Prefabs/)"
        ))),
    }
}

fn require_token_if_needed(
    required: Option<RequiredKey>,
    token: Option<&str>,
    source_label: &'static str,
) -> Result<(), BuildError> {
    let Some(key) = required else { return Ok(()); };
    let key_name = match key {
        RequiredKey::MapboxToken => "Mapbox access token",
        RequiredKey::OpenTopoApiKey => "OpenTopography API key",
    };
    match token {
        Some(t) if !t.trim().is_empty() => Ok(()),
        _ => Err(BuildError::TokenMissing { source_label, key_name }),
    }
}

fn dem_source_label(source: DemSource) -> &'static str {
    match source {
        DemSource::AwsTerrarium => "AWS Terrarium DEM",
        DemSource::MapboxTerrainRgb => "Mapbox Terrain-RGB DEM",
        DemSource::OpenTopography => "OpenTopography DEM",
        DemSource::Usgs3Dep => "USGS 3DEP DEM",
    }
}

fn imagery_source_label(source: ImagerySource) -> &'static str {
    match source {
        ImagerySource::None => "(no imagery)",
        ImagerySource::EsriWorldImagery => "ESRI World Imagery",
        ImagerySource::MapboxSatellite => "Mapbox Satellite imagery",
        ImagerySource::UsgsOrthoimagery => "USGS orthoimagery",
    }
}

pub(crate) fn sanitize_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return String::from("untitled-build");
    }
    trimmed
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect::<String>()
}

/// In-memory `Heightmap` impl: each pixel is the pre-scaled u32 brick height.
pub(crate) struct DemHeightmap {
    width: u32,
    height: u32,
    values: Vec<u32>,
}

impl Heightmap for DemHeightmap {
    fn at(&self, x: u32, y: u32) -> u32 {
        debug_assert!(x < self.width && y < self.height, "DemHeightmap out-of-bounds {x},{y}");
        self.values[(y * self.width + x) as usize]
    }
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// In-memory `Colormap` returning a single color across the whole grid.
/// Used when the user picks `ImagerySource::None`.
pub(crate) struct FlatColormap {
    width: u32,
    height: u32,
    color: [u8; 4],
}

impl FlatColormap {
    /// Test-only constructor: the sculpt passthrough identity test needs a flat
    /// colormap matching a raster's dims but cannot reach the private fields.
    /// Uses the same `DEFAULT_BRICK_COLOR` the production None-imagery path does
    /// so the meshed output is comparable to a real build.
    #[cfg(test)]
    pub(crate) fn for_test(width: u32, height: u32) -> Self {
        Self { width, height, color: DEFAULT_BRICK_COLOR }
    }

    /// The sculpt convert has no imagery layer (color painting is a later MVP),
    /// so it meshes terrain against the same `DEFAULT_BRICK_COLOR` flat colormap
    /// the Map tab's None-imagery path uses. Production constructor (the test one
    /// above is gated out of release builds).
    pub(crate) fn sculpt_default(width: u32, height: u32) -> Self {
        Self { width, height, color: DEFAULT_BRICK_COLOR }
    }
}

impl Colormap for FlatColormap {
    fn at(&self, _x: u32, _y: u32) -> [u8; 4] {
        self.color
    }
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// In-memory `Colormap` backed by an RGBA image (typically a satellite
/// tile composite resampled to the DEM's pixel grid). Applies sRGB → linear
/// gamma conversion on read to match the existing `ColormapPNG` semantics.
pub(crate) struct ImageColormap {
    image: image::RgbaImage,
}

impl ImageColormap {
    fn from_rgba(image: image::RgbaImage) -> Self {
        debug_assert!(
            image.width() > 0 && image.height() > 0,
            "ImageColormap precondition: image must have positive dimensions",
        );
        Self { image }
    }
}

impl Colormap for ImageColormap {
    fn at(&self, x: u32, y: u32) -> [u8; 4] {
        debug_assert!(
            x < self.image.width() && y < self.image.height(),
            "ImageColormap out-of-bounds: ({x},{y}) vs {}×{}",
            self.image.width(),
            self.image.height(),
        );
        to_linear_rgb(self.image.get_pixel(x, y).0)
    }
    fn size(&self) -> (u32, u32) {
        (self.image.width(), self.image.height())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_name_strips_path_separators() {
        assert_eq!(sanitize_name("hello world"), "hello-world");
        assert_eq!(sanitize_name("../etc/passwd"), "---etc-passwd");
        assert_eq!(sanitize_name("ok_name-99"), "ok_name-99");
        assert_eq!(sanitize_name("  "), "untitled-build");
    }

    #[test]
    fn unique_save_path_never_collides_with_existing() {
        let dir = std::env::temp_dir().join(format!("h2brz-worlds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");

        // (a) no collision -> bare name
        let p0 = unique_save_path(&dir, "mtfuji", "brdb").expect("no collision");
        assert_eq!(p0, dir.join("mtfuji.brdb"));

        // (b) bare exists -> -2
        std::fs::write(dir.join("mtfuji.brdb"), b"x").unwrap();
        let p1 = unique_save_path(&dir, "mtfuji", "brdb").expect("-2");
        assert_eq!(p1, dir.join("mtfuji-2.brdb"));

        // (c) bare and -2 exist -> -3 (never returns an existing path)
        std::fs::write(dir.join("mtfuji-2.brdb"), b"x").unwrap();
        let p2 = unique_save_path(&dir, "mtfuji", "brdb").expect("-3");
        assert_eq!(p2, dir.join("mtfuji-3.brdb"));
        assert!(!p2.exists(), "returned path must not already exist");

        // (d) the ext is honored: a .brz with the same stem is independent.
        let pbrz = unique_save_path(&dir, "mtfuji", "brz").expect("brz bare");
        assert_eq!(pbrz, dir.join("mtfuji.brz"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_heightmap_normalizes_to_zero_min() {
        let raster = DemRaster {
            width: 2,
            height: 1,
            heights_m: vec![100.0, 105.0],
            min_m: 100.0,
            max_m: 105.0,
        };
        let hm = build_heightmap(&raster, 5.0, raster.min_m);
        assert_eq!(hm.at(0, 0), 0, "min height pixel must normalize to 0");
        assert_eq!(hm.at(1, 0), 25, "max pixel = (105-100)*5 = 25 brick units");
    }

    /// Grid-foundation identity guard: passing `global_min_m = raster.min_m`
    /// (the single-box caller's value) must reproduce the legacy per-raster
    /// normalization byte-for-byte — every pixel equals
    /// `(((m - raster.min_m) * v).max(0).round() as i64).max(0) as u32`. This is
    /// the build.rs half of the single-box byte-identity contract: the new
    /// `global_min_m` parameter is a no-op on the single-box path.
    #[test]
    fn build_heightmap_single_box_identity() {
        let raster = DemRaster {
            width: 3,
            height: 2,
            heights_m: vec![100.0, 105.0, 100.5, 130.0, 100.0, 162.7],
            min_m: 100.0,
            max_m: 162.7,
        };
        let vertical = 5.0_f32;
        // Independent re-derivation of the pre-grid per-raster normalization.
        let reference: Vec<u32> = raster
            .heights_m
            .iter()
            .map(|m| (((m - raster.min_m) * vertical).max(0.0).round() as i64).max(0) as u32)
            .collect();
        let hm = build_heightmap(&raster, vertical, raster.min_m);
        assert_eq!(
            hm.values, reference,
            "global_min_m = raster.min_m must be byte-identical to the legacy per-raster normalization",
        );
        assert_eq!((hm.width, hm.height), (raster.width, raster.height));
        assert_eq!(hm.at(0, 0), 0, "the present minimum still maps to brick-Z 0");
    }

    /// Encode a single-strip grayscale I16 GeoTIFF in memory — same storage
    /// class OpenTopography uses for SRTMGL1 responses.
    fn encode_geotiff_i16(w: u32, h: u32, data: &[i16]) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        let mut enc = tiff::encoder::TiffEncoder::new(&mut cursor).expect("tiff encoder");
        enc.write_image::<tiff::encoder::colortype::GrayI16>(w, h, data)
            .expect("encode I16 image");
        cursor.into_inner()
    }

    /// Encode a single-strip grayscale F32 GeoTIFF in memory (float DEMs
    /// commonly use NaN as nodata).
    fn encode_geotiff_f32(w: u32, h: u32, data: &[f32]) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        let mut enc = tiff::encoder::TiffEncoder::new(&mut cursor).expect("tiff encoder");
        enc.write_image::<tiff::encoder::colortype::Gray32Float>(w, h, data)
            .expect("encode F32 image");
        cursor.into_inner()
    }

    #[test]
    fn geotiff_i16_nodata_floored_to_valid_min() {
        let bytes = encode_geotiff_i16(2, 2, &[-32768, 100, 200, 150]);
        let raster = decode_geotiff_dem(&bytes).expect("decode I16 GeoTIFF");
        assert_eq!((raster.width, raster.height), (2, 2));
        assert_eq!(raster.min_m, 100.0, "min must come from valid cells only");
        assert_eq!(raster.max_m, 200.0, "max must come from valid cells only");
        assert_eq!(
            raster.heights_m,
            vec![100.0, 100.0, 200.0, 150.0],
            "the -32768 void cell must be floored to the valid minimum",
        );
    }

    #[test]
    fn geotiff_f32_nan_treated_as_nodata() {
        let bytes = encode_geotiff_f32(2, 2, &[f32::NAN, 5.0, 10.0, 7.5]);
        let raster = decode_geotiff_dem(&bytes).expect("decode F32 GeoTIFF");
        assert_eq!(raster.min_m, 5.0, "min must ignore the NaN cell");
        assert_eq!(raster.max_m, 10.0, "max must ignore the NaN cell");
        assert!(
            raster.heights_m.iter().all(|m| m.is_finite()),
            "no NaN may survive into the raster",
        );
        assert_eq!(
            raster.heights_m[0], 5.0,
            "NaN cell must be floored to min, not left as a height-0 hole",
        );
        // Regression for the NaN fix: a surviving NaN normalizes to height 0
        // in build_heightmap, punching a hole at the cell.
        let hm = build_heightmap(&raster, 1.0, raster.min_m);
        assert_eq!(hm.at(0, 0), 0, "floored cell sits at the terrain floor");
        assert_eq!(hm.at(0, 1), 5, "valid 10 m cell = (10-5)*1 brick units");
    }

    #[test]
    fn geotiff_all_nodata_is_empty_dem() {
        let bytes = encode_geotiff_i16(2, 2, &[-32768; 4]);
        let err = decode_geotiff_dem(&bytes).expect_err("all-void DEM is unbuildable");
        assert!(matches!(err, BuildError::EmptyDem), "expected EmptyDem, got {err:?}");
    }

    #[test]
    fn geotiff_garbage_bytes_yield_readable_error() {
        let err = decode_geotiff_dem(b"definitely not a GeoTIFF")
            .expect_err("garbage must not decode");
        match err {
            BuildError::DemApi(msg) => {
                assert!(!msg.trim().is_empty(), "error message must be non-empty");
            }
            other => panic!("expected DemApi for garbage input, got {other:?}"),
        }
    }

    #[test]
    fn validate_dem_dims_bounds_untrusted_header() {
        assert!(validate_dem_dims(1, 1).is_ok());
        // Per-side boundary: 50_000 passes, 50_001 fails.
        assert!(validate_dem_dims(MAX_DEM_DIM, 1).is_ok());
        assert!(validate_dem_dims(1, MAX_DEM_DIM).is_ok());
        assert!(matches!(
            validate_dem_dims(MAX_DEM_DIM + 1, 1),
            Err(BuildError::DemApi(_))
        ));
        assert!(matches!(
            validate_dem_dims(1, MAX_DEM_DIM + 1),
            Err(BuildError::DemApi(_))
        ));
        // Zero-sized rasters rejected outright.
        assert!(matches!(validate_dem_dims(0, 100), Err(BuildError::DemApi(_))));
        assert!(matches!(validate_dem_dims(100, 0), Err(BuildError::DemApi(_))));
        // Total-pixel boundary: exactly 100 M passes, one row more fails even
        // though both sides are under the per-side cap.
        assert!(validate_dem_dims(10_000, 10_000).is_ok());
        assert!(matches!(
            validate_dem_dims(10_000, 10_001),
            Err(BuildError::DemApi(_))
        ));
        // Error text must be the user-facing "draw a smaller box" guidance.
        match validate_dem_dims(40_000, 40_000) {
            Err(BuildError::DemApi(msg)) => {
                assert!(msg.contains("draw a smaller box"), "got: {msg}");
            }
            other => panic!("expected DemApi, got {other:?}"),
        }
    }

    /// Regression for the shipped 1/5-scale bug: dropping the stud→unit ×5 in
    /// `GenOptions::size` built terrain at 1/5 horizontal scale. Non-micro
    /// bricks must come out with XY sizes that are multiples of 5 units;
    /// micro bricks stay in ×1 units (bounded by the 4-cell grid).
    #[test]
    fn stud_scale_applies_5x_xy_for_non_micro_and_1x_for_micro() {
        let raster = DemRaster {
            width: 4,
            height: 4,
            heights_m: (0..16).map(|v| (v % 3) as f32).collect(),
            min_m: 0.0,
            max_m: 2.0,
        };
        let heightmap = build_heightmap(&raster, 1.0, raster.min_m);
        let cm = FlatColormap { width: 4, height: 4, color: DEFAULT_BRICK_COLOR };
        let progress: ProgressFn = Arc::new(|_, _| {});
        let cancel = Arc::new(AtomicBool::new(false));

        let xy_sizes = |bricks: &[brdb::Brick]| -> Vec<(u16, u16)> {
            bricks
                .iter()
                .map(|b| match &b.asset {
                    brdb::BrickType::Procedural { size, .. } => (size.x, size.y),
                    other => panic!("terrain bricks must be procedural, got {other:?}"),
                })
                .collect()
        };

        let studded = generate_bricks(
            &heightmap,
            &cm,
            BrickStyle { block_type: BlockType::SmoothTile, horizontal_scale: 1, glow: false, nocollide: false },
            None,
            (0, 0),
            Arc::clone(&progress),
            Arc::clone(&cancel),
        )
        .expect("non-micro gen");
        assert!(!studded.is_empty(), "non-micro build must emit bricks");
        for (x, y) in xy_sizes(&studded) {
            assert!(
                x >= 5 && x % 5 == 0 && y >= 5 && y % 5 == 0,
                "non-micro brick XY must be stud-scaled multiples of 5, got {x}×{y}",
            );
        }

        let micro = generate_bricks(
            &heightmap,
            &cm,
            BrickStyle { block_type: BlockType::Micro, horizontal_scale: 1, glow: false, nocollide: false },
            None,
            (0, 0),
            Arc::clone(&progress),
            Arc::clone(&cancel),
        )
        .expect("micro gen");
        assert!(!micro.is_empty(), "micro build must emit bricks");
        for (x, y) in xy_sizes(&micro) {
            assert!(
                (1..=4).contains(&x) && (1..=4).contains(&y),
                "micro brick XY must stay in ×1 units bounded by the 4-cell grid, got {x}×{y}",
            );
        }

        // horizontal_scale multiplies XY footprint (×3 → multiples of 15)
        // without changing the brick count vs the ×1 build.
        let scaled = generate_bricks(
            &heightmap,
            &cm,
            BrickStyle { block_type: BlockType::SmoothTile, horizontal_scale: 3, glow: false, nocollide: false },
            None,
            (0, 0),
            progress,
            cancel,
        )
        .expect("scaled gen");
        assert_eq!(
            scaled.len(),
            studded.len(),
            "horizontal_scale must widen bricks, never change their count",
        );
        for (x, y) in xy_sizes(&scaled) {
            assert!(
                x >= 15 && x % 15 == 0 && y >= 15 && y % 15 == 0,
                "scale-3 brick XY must be multiples of 15 units, got {x}×{y}",
            );
        }
    }

    /// Regression for the "hollow floating shell" population bug. The greedy GUI
    /// path emitted a constant 2-unit column per cell, so a cell 50 m above its
    /// neighbor became a 2-unit tile FLOATING ~50 units up with nothing beneath
    /// it — the player falls through gaps and tall features look like floating
    /// platforms. Terrain must instead fill each column from its height down to
    /// the common base plane (watertight, walkable). With the half-unit z
    /// convention (z -= brick_height*2 in emit_column_bricks) the fill height is
    /// `(h - min)/2`, so a 50-unit cell yields a ~25-unit column, not a 2-unit
    /// tile.
    #[test]
    fn greedy_terrain_fills_columns_to_base_not_floating_shell() {
        let raster = DemRaster {
            width: 2,
            height: 1,
            heights_m: vec![0.0, 50.0],
            min_m: 0.0,
            max_m: 50.0,
        };
        let heightmap = build_heightmap(&raster, 1.0, raster.min_m); // normalized values [0, 50]
        let cm = FlatColormap { width: 2, height: 1, color: DEFAULT_BRICK_COLOR };
        let progress: ProgressFn = Arc::new(|_, _| {});
        let cancel = Arc::new(AtomicBool::new(false));
        let bricks = generate_bricks(
            &heightmap,
            &cm,
            BrickStyle { block_type: BlockType::SmoothTile, horizontal_scale: 1, glow: false, nocollide: false },
            None,
            (0, 0),
            progress,
            cancel,
        )
        .expect("gen");

        let zsize = |b: &brdb::Brick| match &b.asset {
            brdb::BrickType::Procedural { size, .. } => size.z,
            other => panic!("expected procedural terrain brick, got {other:?}"),
        };
        // The 50-unit cell must produce a TALL column filled to the base, not a
        // 2-unit floating tile. The old constant-2 behavior caps every brick at
        // 2 units; the fill makes the tall cell ~25 units.
        let tallest = bricks.iter().map(zsize).max().expect("some bricks emitted");
        assert!(
            tallest >= 20,
            "tallest terrain brick is {tallest} units — expected a ~25-unit column \
             filling the 50 m cell down to the base. A <=2-unit max means the \
             floating-shell bug is back (no solid ground under the terrain).",
        );
    }

    /// R1 regression guard: the Convert/CLI path (fill_to_base=false) must NOT
    /// fill columns to the base — it keeps the legacy flat 2-unit surface tile,
    /// so a tall cell cannot emit an unbounded brick stack from un-normalized
    /// heights × a vertical_scale up to 100 (those paths have no MAX_BRICKS cap).
    /// Only the Map path opts in (fill_to_base=true).
    #[test]
    fn fill_to_base_false_keeps_flat_surface_tiles() {
        use brdb::assets::bricks::PB_DEFAULT_SMOOTH_TILE;
        let raster = DemRaster {
            width: 2,
            height: 1,
            heights_m: vec![0.0, 50.0],
            min_m: 0.0,
            max_m: 50.0,
        };
        let heightmap = build_heightmap(&raster, 1.0, raster.min_m);
        let cm = FlatColormap { width: 2, height: 1, color: DEFAULT_BRICK_COLOR };
        let options = GenOptions {
            size: 5,
            scale: 1,
            asset: PB_DEFAULT_SMOOTH_TILE,
            cull: false,
            micro: false,
            stud: false,
            snap: false,
            img: false,
            glow: false,
            hdmap: false,
            lrgb: false,
            nocollide: false,
            quadtree: false,
            greedy: true,
            fill_to_base: false,
            skip_floor: false,
            omit_below_h: 0,
        };
        let bricks =
            crate::opt::gen_opt_heightmap(&heightmap, &cm, options, None, None, |_| true).expect("gen");
        let tallest = bricks
            .iter()
            .map(|b| match &b.asset {
                brdb::BrickType::Procedural { size, .. } => size.z,
                other => panic!("expected procedural terrain brick, got {other:?}"),
            })
            .max()
            .expect("some bricks emitted");
        assert!(
            tallest <= 2,
            "fill_to_base=false must keep flat 2-unit tiles (legacy Convert/CLI \
             behavior), got tallest {tallest}",
        );
    }

    #[test]
    fn cell_budget_rejects_oversized_grid_but_accepts_normal() {
        use crate::gui::tiles::MAX_DEM_CELLS;
        // A typical ~1 km grid is well within budget.
        assert!(enforce_cell_budget(275, 275).is_ok(), "a normal grid must pass");
        // A grid past the budget is rejected, surfacing its oversized cell count
        // so the user can raise Density / shrink the box rather than OOM the mesh.
        let side = (MAX_DEM_CELLS as f64).sqrt() as u32 + 100;
        match enforce_cell_budget(side, side) {
            Err(BuildError::GridTooLarge { cells, max }) => {
                assert_eq!(max, MAX_DEM_CELLS);
                assert!(cells > MAX_DEM_CELLS, "must report the oversized count, got {cells}");
            }
            other => panic!("expected GridTooLarge, got {other:?}"),
        }
    }

    /// Adjacent terrain cells are spaced `2 * options.size` world units apart —
    /// BrickSize is a half-extent, so a cell's footprint equals its pitch
    /// (contiguous tiling). The Map-tab "Predicted output ≈ W×H studs" readout
    /// relies on this 2× pitch (studs/cell = size*2/5); pin it so the estimate
    /// can never silently drift from the geometry again.
    #[test]
    fn greedy_brick_pitch_is_twice_size() {
        let raster = DemRaster {
            width: 2,
            height: 1,
            heights_m: vec![0.0, 50.0],
            min_m: 0.0,
            max_m: 50.0,
        };
        let heightmap = build_heightmap(&raster, 1.0, raster.min_m);
        let cm = FlatColormap { width: 2, height: 1, color: DEFAULT_BRICK_COLOR };
        let progress: ProgressFn = Arc::new(|_, _| {});
        let cancel = Arc::new(AtomicBool::new(false));
        let hscale: u16 = 1;
        let bricks = generate_bricks(
            &heightmap,
            &cm,
            BrickStyle { block_type: BlockType::SmoothTile, horizontal_scale: hscale, glow: false, nocollide: false },
            None,
            (0, 0),
            progress,
            cancel,
        )
        .expect("gen");
        let size = i32::from(hscale) * 5; // GenOptions.size for non-micro
        let mut xs: Vec<i32> = bricks.iter().map(|b| b.position.x).collect();
        xs.sort_unstable();
        xs.dedup();
        assert_eq!(xs.len(), 2, "two distinct-height cells must yield two columns, got {xs:?}");
        assert_eq!(
            xs[1] - xs[0],
            2 * size,
            "adjacent-cell pitch must be 2*size = {}; the studs readout assumes it",
            2 * size,
        );
    }

    /// Micro bricks must work end-to-end as terrain: still fill each column to
    /// the base plane (solid, 1:1) and carry the micro brick asset — so a user
    /// can pick Micro "for nice resolution" and get a faithful, walkable world.
    #[test]
    fn micro_terrain_fills_to_base_and_uses_micro_asset() {
        use brdb::assets::bricks::PB_DEFAULT_MICRO_BRICK;
        let raster = DemRaster {
            width: 2,
            height: 1,
            heights_m: vec![0.0, 50.0],
            min_m: 0.0,
            max_m: 50.0,
        };
        let heightmap = build_heightmap(&raster, 1.0, raster.min_m);
        let cm = FlatColormap { width: 2, height: 1, color: DEFAULT_BRICK_COLOR };
        let progress: ProgressFn = Arc::new(|_, _| {});
        let cancel = Arc::new(AtomicBool::new(false));
        let bricks = generate_bricks(
            &heightmap,
            &cm,
            BrickStyle { block_type: BlockType::Micro, horizontal_scale: 1, glow: false, nocollide: false },
            None,
            (0, 0),
            progress,
            cancel,
        )
        .expect("micro gen");
        let tallest = bricks
            .iter()
            .map(|b| match &b.asset {
                brdb::BrickType::Procedural { size, .. } => size.z,
                other => panic!("expected procedural, got {other:?}"),
            })
            .max()
            .expect("some bricks");
        assert!(tallest >= 20, "micro terrain must fill the 50 m cell to base too, got {tallest}");
        assert!(
            bricks.iter().all(|b| matches!(
                &b.asset,
                brdb::BrickType::Procedural { asset, .. } if *asset == PB_DEFAULT_MICRO_BRICK
            )),
            "micro terrain must use the micro brick asset",
        );
    }

    #[test]
    fn generate_bricks_cancelled_before_start_returns_cancelled() {
        let raster = DemRaster {
            width: 2,
            height: 2,
            heights_m: vec![0.0, 1.0, 2.0, 3.0],
            min_m: 0.0,
            max_m: 3.0,
        };
        let heightmap = build_heightmap(&raster, 1.0, raster.min_m);
        let cm = FlatColormap { width: 2, height: 2, color: DEFAULT_BRICK_COLOR };
        let progress: ProgressFn = Arc::new(|_, _| {});
        let cancel = Arc::new(AtomicBool::new(true));
        let result = generate_bricks(
            &heightmap,
            &cm,
            BrickStyle { block_type: BlockType::SmoothTile, horizontal_scale: 1, glow: false, nocollide: false },
            None,
            (0, 0),
            progress,
            cancel,
        );
        match result {
            Err(BuildError::Cancelled) => {}
            Err(other) => panic!("expected Cancelled, got {other:?}"),
            Ok(bricks) => panic!(
                "pre-set cancel flag must abort generation; got {} bricks",
                bricks.len()
            ),
        }
    }

    #[test]
    fn decode_to_raster_empty_image_is_empty_dem() {
        let empty = RgbaImage::new(0, 0);
        let err = decode_to_raster(&empty, DemSource::AwsTerrarium)
            .expect_err("zero-pixel crop must be rejected");
        assert!(matches!(err, BuildError::EmptyDem), "expected EmptyDem, got {err:?}");
    }

    #[test]
    fn decode_to_raster_minmax_on_hand_built_terrarium_pixels() {
        // Terrarium encoding: meters = R*256 + G + B/256 − 32768.
        // [128,0,0]→0 m, [129,0,0]→256 m, [128,1,0]→1 m, [127,255,0]→−1 m.
        let mut img = RgbaImage::new(2, 2);
        img.put_pixel(0, 0, image::Rgba([128, 0, 0, 255]));
        img.put_pixel(1, 0, image::Rgba([129, 0, 0, 255]));
        img.put_pixel(0, 1, image::Rgba([128, 1, 0, 255]));
        img.put_pixel(1, 1, image::Rgba([127, 255, 0, 255]));
        let raster =
            decode_to_raster(&img, DemSource::AwsTerrarium).expect("decode terrarium raster");
        assert_eq!((raster.width, raster.height), (2, 2));
        assert_eq!(raster.heights_m, vec![0.0, 256.0, 1.0, -1.0]);
        assert_eq!(raster.min_m, -1.0, "min aggregated across all pixels");
        assert_eq!(raster.max_m, 256.0, "max aggregated across all pixels");
    }

    /// Real-data smoke for the OpenTopography GeoTIFF decoder. Point
    /// `H2BRZ_TIFF_FIXTURE` at a globaldem GTiff response and run with
    /// `cargo test --release -- --ignored opentopo_geotiff`. Verifies the
    /// signed-16-bit path decodes to a plausible DemRaster.
    #[test]
    #[ignore = "needs a local GeoTIFF fixture via H2BRZ_TIFF_FIXTURE"]
    fn opentopo_geotiff_decodes_real_fixture() {
        let path = std::env::var("H2BRZ_TIFF_FIXTURE")
            .expect("set H2BRZ_TIFF_FIXTURE to a globaldem GTiff path");
        let bytes = std::fs::read(&path).expect("read fixture");
        let raster = decode_geotiff_dem(&bytes).expect("decode GeoTIFF");
        assert!(raster.width > 0 && raster.height > 0, "non-empty raster");
        assert_eq!(
            raster.heights_m.len(),
            (raster.width * raster.height) as usize,
            "pixel count matches dims",
        );
        assert!(raster.max_m >= raster.min_m, "max >= min");
        assert!(raster.min_m > SRTM_NODATA + 1.0, "nodata floored out of min");
        eprintln!(
            "OpenTopo GeoTIFF OK: {}×{}, elev {:.0}–{:.0} m",
            raster.width, raster.height, raster.min_m, raster.max_m,
        );
    }

    #[test]
    fn bbox_area_km2_matches_known_scale() {
        // 1° × 1° at the equator ≈ 110.574 × 111.320 ≈ 12309 km².
        let equ = BBoxLatLon { north: 0.5, south: -0.5, east: 0.5, west: -0.5 };
        let a = bbox_area_km2(&equ);
        assert!((a - 12_309.0).abs() < 50.0, "equatorial 1°² ≈ 12309 km², got {a:.0}");
        // Same span at 60°N: longitude compressed by cos(60°)=0.5 → ~half.
        let high = BBoxLatLon { north: 60.5, south: 59.5, east: 0.5, west: -0.5 };
        let ah = bbox_area_km2(&high);
        assert!(ah < a * 0.6, "high-latitude box must be much smaller; {ah:.0} vs {a:.0}");
    }

    #[test]
    fn opentopo_rejects_over_cap_bbox() {
        // A ~10°×10° box near the equator is far over the 450,000 km² SRTMGL1 cap.
        let huge = BBoxLatLon { north: 5.0, south: -5.0, east: 5.0, west: -5.0 };
        assert!(
            bbox_area_km2(&huge) > OPENTOPO_MAX_AREA_KM2,
            "10°² near equator must exceed the documented cap",
        );
        let progress: ProgressFn = Arc::new(|_, _| {});
        let cancel = AtomicBool::new(false);
        let err = fetch_opentopo_dem(huge, "dummy-key", &progress, &cancel)
            .expect_err("over-cap bbox must be rejected before any network call");
        match err {
            BuildError::DemApi(msg) => assert!(
                msg.contains("per-request limit"),
                "must explain the API limit; got {msg}",
            ),
            other => panic!("expected DemApi area-cap error, got {other:?}"),
        }
    }

    #[test]
    fn downsample_factor_one_is_identity() {
        let raster = DemRaster {
            width: 3,
            height: 2,
            heights_m: vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0],
            min_m: 1.0,
            max_m: 6.0,
        };
        let out = downsample(&raster, 1);
        assert_eq!((out.width, out.height), (3, 2));
        assert_eq!(out.heights_m, raster.heights_m, "factor 1 must be byte-identical");
        assert_eq!((out.min_m, out.max_m), (1.0, 6.0));
    }

    #[test]
    fn downsample_factor_two_block_means_and_dims() {
        // 4×4 grid, rows 0..=15 left→right top→bottom. Factor 2 → 2×2 cells,
        // each the mean of a 2×2 block.
        let raster = DemRaster {
            width: 4,
            height: 4,
            heights_m: (0..16).map(|v| v as f32).collect(),
            min_m: 0.0,
            max_m: 15.0,
        };
        let out = downsample(&raster, 2);
        assert_eq!((out.width, out.height), (2, 2));
        // Block (0,0) = mean(0,1,4,5)=2.5; (1,0)=mean(2,3,6,7)=4.5;
        // (0,1)=mean(8,9,12,13)=10.5; (1,1)=mean(10,11,14,15)=12.5.
        assert_eq!(out.heights_m, vec![2.5, 4.5, 10.5, 12.5]);
        assert_eq!(out.min_m, 2.5, "min recomputed from reduced cells");
        assert_eq!(out.max_m, 12.5, "max recomputed from reduced cells");
    }

    #[test]
    fn downsample_handles_non_divisible_dims() {
        // 3×3 at factor 2 → ceil(3/2)=2 per axis; edge blocks are partial.
        let raster = DemRaster {
            width: 3,
            height: 3,
            heights_m: (0..9).map(|v| v as f32).collect(),
            min_m: 0.0,
            max_m: 8.0,
        };
        let out = downsample(&raster, 2);
        assert_eq!((out.width, out.height), (2, 2));
        // (0,0)=mean(0,1,3,4)=2; (1,0)=mean(2,5)=3.5; (0,1)=mean(6,7)=6.5; (1,1)=8.
        assert_eq!(out.heights_m, vec![2.0, 3.5, 6.5, 8.0]);
    }

    #[test]
    fn downsample_factor_two_reduces_generated_brick_count() {
        // The density payoff, proven end-to-end through the mesher: a factor-2
        // downsample must yield strictly fewer *bricks*, not just fewer cells.
        // Heights cycle mod 7 over an 8-wide grid, so no two adjacent cells
        // share a height (neighbors differ by 1 or 8 ≡ 1 mod 7) — greedy
        // merging cannot collapse the full-detail grid and mask a regression.
        let raster = DemRaster {
            width: 8,
            height: 8,
            heights_m: (0..64).map(|v| (v % 7) as f32).collect(),
            min_m: 0.0,
            max_m: 6.0,
        };
        let progress: ProgressFn = Arc::new(|_, _| {});
        let cancel = Arc::new(AtomicBool::new(false));
        let gen_for = |r: &DemRaster| {
            let hm = build_heightmap(r, 1.0, r.min_m);
            let cm = FlatColormap { width: r.width, height: r.height, color: DEFAULT_BRICK_COLOR };
            generate_bricks(
                &hm,
                &cm,
                BrickStyle { block_type: BlockType::SmoothTile, horizontal_scale: 1, glow: false, nocollide: false },
                None,
                (0, 0),
                Arc::clone(&progress),
                Arc::clone(&cancel),
            )
            .expect("offline brick generation must succeed")
        };
        let full = gen_for(&raster);
        let coarse = gen_for(&downsample(&raster, 2));
        assert!(!coarse.is_empty(), "downsampled terrain must still produce bricks");
        assert!(
            coarse.len() < full.len(),
            "factor 2 must reduce brick count; full={} coarse={}",
            full.len(),
            coarse.len(),
        );
    }

    #[test]
    fn image_colormap_returns_per_pixel_color() {
        let mut img = image::RgbaImage::new(2, 1);
        img.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        img.put_pixel(1, 0, image::Rgba([0, 255, 0, 255]));
        let cm = ImageColormap::from_rgba(img);
        assert_eq!(cm.size(), (2, 1));
        let left = cm.at(0, 0);
        let right = cm.at(1, 0);
        assert_ne!(
            left, right,
            "ImageColormap must return different colors at different pixels; got {left:?} and {right:?}",
        );
    }

    /// REAL colormap carry-through proof (offline): a uniform-height terrain
    /// with a TWO-colour imagery colormap must produce bricks of >=2 distinct
    /// colours, and the equivalent FLAT colormap must produce exactly 1. This
    /// proves the imagery actually reaches the brick palette — replacing the
    /// b2_2 E2E's file-size proxy, which only showed the file got bigger.
    #[test]
    fn colormap_carries_distinct_colors_into_bricks() {
        use std::collections::HashSet;
        use std::sync::atomic::AtomicBool;

        // Uniform normalized height (min_m below the pixels => non-zero, equal
        // everywhere) so greedy meshing can ONLY be split by colour, isolating
        // the colormap's effect from height.
        let raster = DemRaster {
            width: 4,
            height: 4,
            heights_m: vec![10.0; 16],
            min_m: 0.0,
            max_m: 10.0,
        };
        let heightmap = build_heightmap(&raster, 5.0, raster.min_m);

        let mut img = image::RgbaImage::new(4, 4);
        for y in 0..4 {
            for x in 0..4 {
                let c = if x < 2 { [220, 40, 40, 255] } else { [40, 80, 220, 255] };
                img.put_pixel(x, y, image::Rgba(c));
            }
        }
        let img_cm = ImageColormap::from_rgba(img);
        let progress: ProgressFn = Arc::new(|_, _| {});
        let cancel = Arc::new(AtomicBool::new(false));

        let colored = generate_bricks(
            &heightmap,
            &img_cm,
            BrickStyle { block_type: BlockType::SmoothTile, horizontal_scale: 1, glow: false, nocollide: false },
            None,
            (0, 0),
            Arc::clone(&progress),
            Arc::clone(&cancel),
        )
        .expect("colored gen");
        let colored_colors: HashSet<String> =
            colored.iter().map(|b| format!("{:?}", b.color)).collect();
        assert!(
            colored_colors.len() >= 2,
            "two-colour imagery must yield >=2 distinct brick colours; got {}",
            colored_colors.len()
        );

        let flat_cm = FlatColormap { width: 4, height: 4, color: DEFAULT_BRICK_COLOR };
        let flat = generate_bricks(
            &heightmap,
            &flat_cm,
            BrickStyle { block_type: BlockType::SmoothTile, horizontal_scale: 1, glow: false, nocollide: false },
            None,
            (0, 0),
            progress,
            cancel,
        )
        .expect("flat gen");
        let flat_colors: HashSet<String> =
            flat.iter().map(|b| format!("{:?}", b.color)).collect();
        assert_eq!(
            flat_colors.len(),
            1,
            "a single-colour colormap must yield exactly 1 brick colour; got {}",
            flat_colors.len()
        );
    }

    /// Offline omit-water parity (mirrors the sculpt convert's omit): a tile with
    /// a low strip (at the area minimum) and a high strip must, with
    /// `omit_below_m` raised above the low strip, emit NO bricks for the low
    /// columns (the native floor shows through) while the high strip survives.
    /// `omit_below_m = 0` stays byte-identical to the prior build. Uses
    /// `build_one_tile` directly with a synthetic raster (no network).
    #[test]
    fn map_omit_below_drops_low_columns() {
        use std::sync::atomic::AtomicBool;
        // 4×2 grid: bottom row 0 m (the minimum → "water"), top row 100 m.
        let raster = DemRaster {
            width: 4,
            height: 2,
            heights_m: vec![0., 0., 0., 0., 100., 100., 100., 100.],
            min_m: 0.0,
            max_m: 100.0,
        };
        fn req(omit_below_m: f32) -> BuildRequest {
            BuildRequest {
                bbox: BBoxLatLon { south: 0.0, north: 1.0, west: 0.0, east: 1.0 },
                name: "omit".to_owned(),
                dem_source: DemSource::AwsTerrarium,
                imagery_source: ImagerySource::None,
                mapbox_token: None,
                opentopo_key: None,
                vertical_scale: 1.0,
                density_factor: 1,
                horizontal_scale: 1,
                block_type: BlockType::SmoothTile,
                glow: false,
                no_collision: false,
                install_to_brickadia: false,
                overwrite_world: false,
                omit_below_m,
                floor_level_m: 0.0,
            }
        }
        let noop: ProgressFn = Arc::new(|_, _| {});
        let off = (0, 0);

        // Baseline: import everything — both strips emit.
        let base = build_one_tile(
            &req(0.0), raster.clone(), 0.0, off, Some(0), Arc::clone(&noop),
            Arc::new(AtomicBool::new(false)),
        )
        .expect("baseline build");
        // Omit below 50 m (relative): the 0 m strip drops, the 100 m strip stays.
        let omitted = build_one_tile(
            &req(50.0), raster.clone(), 0.0, off, Some(0), Arc::clone(&noop),
            Arc::new(AtomicBool::new(false)),
        )
        .expect("omit build");
        assert!(!omitted.is_empty(), "high terrain must still emit");
        assert!(
            omitted.len() < base.len(),
            "omitting the water strip must drop bricks: base={} omitted={}",
            base.len(),
            omitted.len(),
        );
        // Omit above the peak: all terrain drops.
        let none = build_one_tile(
            &req(200.0), raster, 0.0, off, Some(0), noop,
            Arc::new(AtomicBool::new(false)),
        )
        .expect("omit-all build");
        assert!(none.is_empty(), "omit above the peak drops all terrain, got {}", none.len());
    }

    /// Live end-to-end test against AWS Terrarium for the Horsetooth bbox.
    /// Network-dependent; `#[ignore]` keeps it out of routine CI. Opt in with
    /// `cargo test -- --ignored b2_1_horsetooth_e2e`.
    ///
    /// Asserts the externally-observable effect: a `.brdb` file is written to
    /// disk, contains > 0 bricks, and the decoded elevation range covers
    /// Horsetooth's ~485 m of relief (1727–2212 m).
    #[test]
    #[ignore = "live network — opt in with --ignored"]
    fn b2_1_horsetooth_e2e() {
        use std::sync::atomic::AtomicBool;

        let bbox = BBoxLatLon {
            north: 40.560,
            south: 40.523,
            east: -105.131,
            west: -105.183,
        };
        let request = BuildRequest {
            bbox,
            name: "b2_1_e2e_horsetooth".to_owned(),
            dem_source: DemSource::AwsTerrarium,
            imagery_source: ImagerySource::None,
            mapbox_token: None,
            opentopo_key: None,
            vertical_scale: 5.0,
            density_factor: 2,
            horizontal_scale: 1,
            block_type: BlockType::SmoothTile,
            glow: false,
            no_collision: false,
            install_to_brickadia: false,
            overwrite_world: false,
            omit_below_m: 0.0,
            floor_level_m: 0.0,
        };
        let progress: ProgressFn = Arc::new(|stage, f| {
            eprintln!("stage={stage:?} progress={f:.2}");
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let outcome = run_build(request, progress, cancel).expect("build should succeed");

        assert!(
            outcome.brdb_path.exists(),
            ".brdb must exist at {:?}",
            outcome.brdb_path
        );
        let size = std::fs::metadata(&outcome.brdb_path).unwrap().len();
        assert!(size > 1024, ".brdb size {size} too small to be a real save");
        assert!(outcome.brick_count > 100, "expected many bricks, got {}", outcome.brick_count);
        assert!(
            outcome.elevation_max_m - outcome.elevation_min_m > 100.0,
            "Horsetooth relief should exceed 100m; got {}m",
            outcome.elevation_max_m - outcome.elevation_min_m
        );
        eprintln!(
            "B-2.1 E2E OK: {} bricks, DEM {}×{} px, elev {:.0}-{:.0} m, brdb={}",
            outcome.brick_count,
            outcome.dem_width,
            outcome.dem_height,
            outcome.elevation_min_m,
            outcome.elevation_max_m,
            outcome.brdb_path.display(),
        );
    }

    #[test]
    fn require_token_returns_error_when_missing() {
        let result = require_token_if_needed(
            Some(RequiredKey::MapboxToken),
            None,
            "Mapbox Terrain-RGB DEM",
        );
        assert!(matches!(result, Err(BuildError::TokenMissing { .. })));
        let result_blank = require_token_if_needed(
            Some(RequiredKey::MapboxToken),
            Some("   "),
            "Mapbox Terrain-RGB DEM",
        );
        assert!(matches!(result_blank, Err(BuildError::TokenMissing { .. })));
    }

    #[test]
    fn require_token_passes_through_when_present() {
        let result = require_token_if_needed(
            Some(RequiredKey::MapboxToken),
            Some("pk.realish"),
            "Mapbox Terrain-RGB DEM",
        );
        assert!(result.is_ok());
    }

    #[test]
    fn require_token_noop_when_source_needs_none() {
        let result = require_token_if_needed(None, None, "AWS Terrarium DEM");
        assert!(result.is_ok());
    }

    #[test]
    fn build_request_routes_tokens_by_required_key() {
        let req_mapbox = BuildRequest {
            bbox: BBoxLatLon { north: 1.0, south: 0.0, east: 1.0, west: 0.0 },
            name: "x".into(),
            dem_source: DemSource::MapboxTerrainRgb,
            imagery_source: ImagerySource::MapboxSatellite,
            mapbox_token: Some("pk.tok".into()),
            opentopo_key: Some("otkey".into()),
            vertical_scale: 1.0,
            density_factor: 1,
            horizontal_scale: 1,
            block_type: BlockType::SmoothTile,
            glow: false,
            no_collision: false,
            install_to_brickadia: false,
            overwrite_world: false,
            omit_below_m: 0.0,
            floor_level_m: 0.0,
        };
        assert_eq!(req_mapbox.dem_token(), Some("pk.tok"));
        assert_eq!(req_mapbox.imagery_token(), Some("pk.tok"));

        let req_no_keys = BuildRequest {
            bbox: BBoxLatLon { north: 1.0, south: 0.0, east: 1.0, west: 0.0 },
            name: "x".into(),
            dem_source: DemSource::AwsTerrarium,
            imagery_source: ImagerySource::EsriWorldImagery,
            mapbox_token: Some("pk.tok".into()),
            opentopo_key: None,
            vertical_scale: 1.0,
            density_factor: 1,
            horizontal_scale: 1,
            block_type: BlockType::SmoothTile,
            glow: false,
            no_collision: false,
            install_to_brickadia: false,
            overwrite_world: false,
            omit_below_m: 0.0,
            floor_level_m: 0.0,
        };
        assert_eq!(req_no_keys.dem_token(), None);
        assert_eq!(req_no_keys.imagery_token(), None);
    }

    /// Live E2E for B-2.2: AWS Terrarium DEM + ESRI World Imagery colormap.
    /// Network-dependent; opt in with
    /// `cargo test -- --ignored b2_2_horsetooth_with_colormap_e2e`.
    ///
    /// Live smoke that the AWS Terrarium DEM + ESRI imagery path completes and
    /// writes a non-trivial `.brdb`. NOTE: colormap *carry-through* is proven
    /// by the offline `colormap_carries_distinct_colors_into_bricks` test —
    /// the file-size comparison below is only a weak corroborating signal
    /// (a larger file does not by itself prove colour reached the bricks).
    #[test]
    #[ignore = "live network — opt in with --ignored"]
    fn b2_2_horsetooth_with_colormap_e2e() {
        use std::sync::atomic::AtomicBool;

        let bbox = BBoxLatLon {
            north: 40.560,
            south: 40.523,
            east: -105.131,
            west: -105.183,
        };
        let request = BuildRequest {
            bbox,
            name: "b2_2_e2e_horsetooth_colored".to_owned(),
            dem_source: DemSource::AwsTerrarium,
            imagery_source: ImagerySource::EsriWorldImagery,
            mapbox_token: None,
            opentopo_key: None,
            vertical_scale: 5.0,
            density_factor: 2,
            horizontal_scale: 1,
            block_type: BlockType::SmoothTile,
            glow: false,
            no_collision: false,
            install_to_brickadia: false,
            overwrite_world: false,
            omit_below_m: 0.0,
            floor_level_m: 0.0,
        };
        let progress: ProgressFn = Arc::new(|stage, f| {
            eprintln!("stage={stage:?} progress={f:.2}");
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let outcome = run_build(request, progress, cancel).expect("build should succeed");

        assert!(outcome.brdb_path.exists(), ".brdb must exist");
        let size_with_imagery = std::fs::metadata(&outcome.brdb_path).unwrap().len();
        let baseline_path = builds_dir().unwrap().join("b2_1_e2e_horsetooth.brdb");
        if baseline_path.exists() {
            let baseline_size = std::fs::metadata(&baseline_path).unwrap().len();
            assert!(
                size_with_imagery > baseline_size,
                "colored brdb {} should be larger than flat baseline {}",
                size_with_imagery, baseline_size,
            );
        }
        assert!(outcome.brick_count > 100, "expected many bricks, got {}", outcome.brick_count);
        eprintln!(
            "B-2.2 E2E OK: {} bricks colored, DEM {}×{}, brdb size={} bytes",
            outcome.brick_count, outcome.dem_width, outcome.dem_height, size_with_imagery,
        );
    }

    /// Diagnostic probe: real OpenTopography DEM + ESRI imagery build. Reports
    /// brick count, DEM dims, elevation range, and DISTINCT brick colors (to
    /// tell apart "no terrain bricks" from "terrain has no color variation").
    /// Needs the opentopo key in config. Opt in:
    /// `cargo test --release -- --ignored --nocapture opentopo_probe`.
    #[test]
    #[ignore = "live network + OpenTopography key required"]
    fn opentopo_probe() {
        use std::sync::atomic::AtomicBool;

        let config = super::super::config::Config::load().expect("load config");
        let key = config.opentopo_api_key.clone().expect("opentopo key in config");

        let bbox = BBoxLatLon { north: 40.560, south: 40.523, east: -105.131, west: -105.183 };
        let request = BuildRequest {
            bbox,
            name: "opentopo_probe".to_owned(),
            dem_source: DemSource::OpenTopography,
            imagery_source: ImagerySource::EsriWorldImagery,
            mapbox_token: None,
            opentopo_key: Some(key),
            vertical_scale: 5.0,
            density_factor: 1,
            horizontal_scale: 1,
            block_type: BlockType::SmoothTile,
            glow: false,
            no_collision: false,
            install_to_brickadia: false,
            overwrite_world: false,
            omit_below_m: 0.0,
            floor_level_m: 0.0,
        };
        let progress: ProgressFn = Arc::new(|_, _| {});
        let cancel = Arc::new(AtomicBool::new(false));
        let outcome = run_build(request, progress, cancel).expect("OpenTopo build should succeed");

        // Re-read the world's bricks via the colormap path is not exposed; instead
        // assert the outcome and surface counts. Distinct color check happens in
        // the colormap unit test; here we prove terrain bricks exist.
        eprintln!(
            "OPENTOPO PROBE: {} bricks, DEM {}×{}, elev {:.0}-{:.0} m, brdb={}",
            outcome.brick_count,
            outcome.dem_width,
            outcome.dem_height,
            outcome.elevation_min_m,
            outcome.elevation_max_m,
            outcome.brdb_path.display(),
        );
        assert!(
            outcome.brick_count > 100,
            "OpenTopo produced only {} bricks — terrain missing",
            outcome.brick_count,
        );
    }

    /// Live E2E for B-2.3: Mapbox Terrain-RGB DEM + Mapbox Satellite imagery.
    /// Requires a real Mapbox token in `~/.config/heightmap2brz/config.toml`;
    /// otherwise the test prints a notice and returns Ok without exercising
    /// the network. Opt in with
    /// `cargo test -- --ignored b2_3_horsetooth_mapbox_e2e`.
    #[test]
    #[ignore = "live network + Mapbox token required"]
    fn b2_3_horsetooth_mapbox_e2e() {
        use std::sync::atomic::AtomicBool;

        let config = match super::super::config::Config::load() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("skip: could not load config: {e}");
                return;
            }
        };
        let Some(token) = config.mapbox_token.as_ref().filter(|t| !t.trim().is_empty()) else {
            eprintln!(
                "skip: no Mapbox token configured. Set one via the GUI Settings to run this test."
            );
            return;
        };

        let bbox = BBoxLatLon {
            north: 40.560,
            south: 40.523,
            east: -105.131,
            west: -105.183,
        };
        let request = BuildRequest {
            bbox,
            name: "b2_3_e2e_horsetooth_mapbox".to_owned(),
            dem_source: DemSource::MapboxTerrainRgb,
            imagery_source: ImagerySource::MapboxSatellite,
            mapbox_token: Some(token.clone()),
            opentopo_key: None,
            vertical_scale: 5.0,
            density_factor: 2,
            horizontal_scale: 1,
            block_type: BlockType::SmoothTile,
            glow: false,
            no_collision: false,
            install_to_brickadia: false,
            overwrite_world: false,
            omit_below_m: 0.0,
            floor_level_m: 0.0,
        };
        let progress: ProgressFn = Arc::new(|stage, f| {
            eprintln!("stage={stage:?} progress={f:.2}");
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let outcome = run_build(request, progress, cancel).expect("Mapbox build should succeed");

        assert!(outcome.brdb_path.exists(), ".brdb must exist");
        let size = std::fs::metadata(&outcome.brdb_path).unwrap().len();
        assert!(size > 1024, ".brdb size {size} too small to be a real save");
        assert!(outcome.brick_count > 100);
        assert!(
            outcome.elevation_max_m - outcome.elevation_min_m > 100.0,
            "Horsetooth relief via Mapbox should exceed 100m; got {}m",
            outcome.elevation_max_m - outcome.elevation_min_m
        );
        eprintln!(
            "B-2.3 E2E OK: {} bricks (Mapbox DEM+Sat), DEM {}×{}, elev {:.0}-{:.0} m, brdb size={}",
            outcome.brick_count,
            outcome.dem_width,
            outcome.dem_height,
            outcome.elevation_min_m,
            outcome.elevation_max_m,
            size,
        );
    }

    #[test]
    fn build_aborts_when_mapbox_dem_picked_with_no_token() {
        let bbox = BBoxLatLon {
            north: 40.560,
            south: 40.523,
            east: -105.131,
            west: -105.183,
        };
        let request = BuildRequest {
            bbox,
            name: "b2_3_token_missing".into(),
            dem_source: DemSource::MapboxTerrainRgb,
            imagery_source: ImagerySource::None,
            mapbox_token: None,
            opentopo_key: None,
            vertical_scale: 5.0,
            density_factor: 1,
            horizontal_scale: 1,
            block_type: BlockType::SmoothTile,
            glow: false,
            no_collision: false,
            install_to_brickadia: false,
            overwrite_world: false,
            omit_below_m: 0.0,
            floor_level_m: 0.0,
        };
        let progress: ProgressFn = Arc::new(|_, _| {});
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let result = run_build(request, progress, cancel);
        assert!(
            matches!(result, Err(BuildError::TokenMissing { .. })),
            "expected TokenMissing, got {result:?}",
        );
    }
}
