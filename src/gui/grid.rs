//! Grid / tiled large-world planner + alignment engine (pure).
//!
//! This module owns the data model that every grid UI mode reduces to
//! ([`GridPlan`]) and the pure functions that build it from a big bounding box
//! ([`partition`]) and place each tile in the combined world ([`world_offset`]).
//!
//! The three correctness pillars (spec §1) live here:
//!
//! * **A — geographic continuity:** one shared `lon_edges`/`lat_edges` lattice
//!   built by equal-fraction lerp, so adjacent tiles share an interior edge BY
//!   f64 VALUE → `round(lat_lon_to_world_px)` is identical from either side →
//!   A's last DEM column and B's first are consecutive real pixels.
//! * **A — uniform pitch:** ONE forced zoom (a `tile_m`-square centroid probe
//!   through [`pick_zoom`]) and ONE [`derive_scale`] result stamped into every
//!   tile, so `size` is bit-identical across the grid.
//! * **C — world placement:** [`world_offset`] uses the cumulative-cells
//!   prefix-sum (realized per-column/row widths from the world-px lattice) plus
//!   the global-centering term; a single-box reduction yields exactly the legacy
//!   `-(width*size)`.
//!
//! The planner ([`partition`]/[`world_offset`]) and the pure pre-commit estimate
//! ([`estimate_grid`]) are I/O-free and unit-testable. The orchestrator
//! ([`run_grid_build`]) is the only function here that touches the network and
//! the filesystem; it runs on a single worker thread spawned by the GUI.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rayon::prelude::*;

use crate::util::{bricks_to_save, write_save_world};

use super::build::{
    BlockType, BuildError, BuildRequest, BuildStage, DemRaster, MAX_BRICKS, MAX_GRID_BRICKS,
    build_one_tile, builds_dir, cell_size_units, downsample, enforce_cell_budget,
    fetch_and_decode_dem, install_save, sanitize_name,
};
use super::dem_sources::DemSource;
use super::imagery_sources::ImagerySource;
use super::scale::{derive_scale, ground_resolution_m};
use super::tiles::{BBoxLatLon, MAX_DEM_CELLS, MERCATOR_LAT_LIMIT, approx_cell_count, lat_lon_to_world_px, pick_zoom};

/// Bounded-parallel FETCH workers (Phase A). I/O-bound, so a small fixed pool
/// saturates the network without flooding the providers; each `fetch_bbox`
/// builds its own `ureq::Agent` (independent connections). The MESH phase is
/// SEQUENTIAL on the global rayon pool (BINDING CORRECTION #1) — there is NO
/// tile-worker meshing pool.
const NET_WORKERS: usize = 8;

/// Per-tile single-box brick cap is bypassed in grid mode; `MAX_GRID_BRICKS`
/// (re-exported from `build`) caps the COMBINED accumulator instead.
///
/// The estimate's RAM model assumes only `parallel_tiles` tile meshes are
/// resident at once (the FETCH governor), NOT the sum — meshing is sequential so
/// peak mesh RAM is one tile, but the fetch phase holds all rasters and the
/// estimate's `parallel_tiles` figure models the bounded-parallel fetch RAM.
const PARALLEL_TILES: u32 = NET_WORKERS as u32;

/// Volumetric mean Earth radius (km), shared with `map_tab::haversine_km`.
/// Duplicated here (~10 lines) rather than widening a public surface — the
/// planner is a sibling module and the function is trivially pure (spec §3.1,
/// ponytail: laziest correct, no speculative shared module).
const EARTH_RADIUS_KM: f64 = 6371.0088;

/// Great-circle distance in km between two lat/lon points (haversine).
/// A value-identical copy of `map_tab::haversine_km` (spec §3.1).
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

/// SRTMGL1 N-S cell size: 1 arc-second of LATITUDE ≈ 30.92 m, ~constant.
/// Used only for the OpenTopography geometric-mean `cell_m` (mirrors
/// `map_tab::predicted_cell_m`), which grid mode warns against (non-Mercator).
const SRTMGL1_NS_M: f64 = 30.92;

/// Row 0 = NORTH, Col 0 = WEST. Matches Web-Mercator y-grows-south.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TileId {
    pub(crate) row: u32,
    pub(crate) col: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct PlannedTile {
    pub(crate) id: TileId,
    /// Sub-box; shares edges with neighbors BY VALUE (read from the lattice).
    pub(crate) bbox: BBoxLatLon,
    /// Global NW cell index (sum of column widths to the west).
    pub(crate) off_cells_x: u32,
    /// Global NW cell index (sum of row heights to the north).
    pub(crate) off_cells_y: u32,
    /// Realized cropped width (cells), from the world-px edge lattice. The
    /// orchestrator places tiles via the cumulative `off_cells_x` prefix, so this
    /// realized per-tile width is only read by the seam-contiguity regression
    /// tests (they assert `partition` stored A's width so that A's last absolute
    /// DEM column +1 == B's first — pillar A). Kept as a canonical model field.
    #[allow(dead_code)]
    pub(crate) cells_w: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct GridPlan {
    /// SINGLE locked zoom for ALL tiles (pillar A / uniform pitch).
    pub(crate) zoom: u32,
    /// Ground m/cell at the locked zoom + grid-center lat.
    pub(crate) cell_m: f64,
    /// `derive_scale()` ONCE → identical `size` for every tile.
    pub(crate) horizontal_scale: u16,
    /// `derive_scale()` ONCE → identical affine Z map.
    pub(crate) vertical_scale: f32,
    pub(crate) rows: u32,
    pub(crate) cols: u32,
    /// len = cols+1, monotonic west→east, shared by value.
    pub(crate) lon_edges: Vec<f64>,
    /// len = rows+1, monotonic north→south, shared by value.
    pub(crate) lat_edges: Vec<f64>,
    /// Included tiles only, row-major; excluded already dropped.
    pub(crate) tiles: Vec<PlannedTile>,
    /// Full mosaic extent in cells (from the world-px lattice; centering + estimate).
    pub(crate) global_cells_w: u32,
    pub(crate) global_cells_h: u32,
    pub(crate) name: String,
}

#[derive(Debug, Clone)]
pub(crate) enum GridMode {
    /// Mode 1: subdivide the drawn box into `tile_m`-ish tiles.
    AutoSubdivide { tile_m: f64 },
    /// Mode 2: same lattice as auto, minus the EXCLUDED tiles (default all-in).
    ClickMask { tile_m: f64, excluded: HashSet<TileId> },
    /// Mode 3: numeric cols×rows anchored at the box NW corner or center.
    Explicit { tile_m: f64, cols: u32, rows: u32, anchor: AnchorKind },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnchorKind {
    NwCorner,
    Center,
}

/// The non-bbox, non-name shaping fields, stamped per tile (spec §2).
#[derive(Debug, Clone)]
pub(crate) struct GridSettings {
    pub(crate) dem_source: DemSource,
    pub(crate) imagery_source: ImagerySource,
    pub(crate) mapbox_token: Option<String>,
    pub(crate) opentopo_key: Option<String>,
    pub(crate) block_type: BlockType,
    pub(crate) glow: bool,
    pub(crate) no_collision: bool,
    pub(crate) output: OutputOptions,
    pub(crate) overwrite: bool,
}

/// Output sinks: format(s) × layout(s) × install. `≥1` true in each axis is the
/// caller's contract (validated in stage 4). Defaults reproduce the legacy
/// single-box output (brdb + stitched + install).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputOptions {
    pub(crate) brdb: bool,
    pub(crate) brz: bool,
    pub(crate) stitched: bool,
    pub(crate) individual: bool,
    pub(crate) install_to_brickadia: bool,
}

impl Default for OutputOptions {
    fn default() -> Self {
        Self {
            brdb: true,
            brz: false,
            stitched: true,
            individual: false,
            install_to_brickadia: true,
        }
    }
}

/// The `tile_m` field, common to all three modes.
fn mode_tile_m(mode: &GridMode) -> f64 {
    match mode {
        GridMode::AutoSubdivide { tile_m }
        | GridMode::ClickMask { tile_m, .. }
        | GridMode::Explicit { tile_m, .. } => *tile_m,
    }
}

/// Linear interpolation between `a` and `b` at fraction `t` (= `i / n`). Built
/// from the SAME `t = i as f64 / n as f64` for every edge so interior edges of
/// adjacent tiles are bit-identical (`lon_edges[c+1]` read by BOTH neighbors).
fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

/// Build the shared edge lattice for `cols` × `rows` over `big`. Equal-fraction
/// lerp guarantees every interior edge is one f64 value (pillar A).
fn edge_lattice(big: BBoxLatLon, cols: u32, rows: u32) -> (Vec<f64>, Vec<f64>) {
    let mut lon_edges = Vec::with_capacity(cols as usize + 1);
    for i in 0..=cols {
        lon_edges.push(lerp(big.west, big.east, f64::from(i) / f64::from(cols)));
    }
    let mut lat_edges = Vec::with_capacity(rows as usize + 1);
    for j in 0..=rows {
        // north→south: j=0 is north, j=rows is south.
        lat_edges.push(lerp(big.north, big.south, f64::from(j) / f64::from(rows)));
    }
    (lon_edges, lat_edges)
}

/// Realized per-column cell widths from the world-px lattice: `cw[c] =
/// round(world_px_x(lon_edges[c+1])) - round(world_px_x(lon_edges[c]))`. Using
/// the SAME rounded absolute world pixels the fetch crops against (`crop_window`
/// rounds each edge), so a tile's east column world-x equals its neighbor's west
/// column world-x EXACTLY and the prefix sum tiles seamlessly (spec §2/§3.5).
fn col_cell_widths(lon_edges: &[f64], lat_ref: f64, zoom: u32) -> Vec<u32> {
    let px: Vec<i64> = lon_edges
        .iter()
        .map(|&lon| lat_lon_to_world_px(lat_ref, lon, zoom).0.round() as i64)
        .collect();
    px.windows(2).map(|w| (w[1] - w[0]).max(0) as u32).collect()
}

/// Realized per-row cell heights from the world-px lattice (north→south).
fn row_cell_heights(lat_edges: &[f64], lon_ref: f64, zoom: u32) -> Vec<u32> {
    let px: Vec<i64> = lat_edges
        .iter()
        .map(|&lat| lat_lon_to_world_px(lat, lon_ref, zoom).1.round() as i64)
        .collect();
    px.windows(2).map(|w| (w[1] - w[0]).max(0) as u32).collect()
}

/// Prefix sums `off[c] = Σ_{j<c} v[j]` (len = `v.len()+1`); `off[len]` is the
/// total. Used for both the per-tile NW cell offset and the mosaic extent.
fn prefix_sum(v: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(v.len() + 1);
    let mut acc = 0u32;
    out.push(0);
    for &x in v {
        acc = acc.saturating_add(x);
        out.push(acc);
    }
    out
}

/// `cell_m` (ground meters per cell) at the locked zoom and grid-center latitude.
/// Mercator sources use `ground_resolution_m`; OpenTopography (geographic, EPSG
/// :4326) uses the geometric-mean of its anisotropic axes, mirroring
/// `map_tab::predicted_cell_m` (grid mode warns against it — spec §9).
fn cell_m_at(dem_source: DemSource, center_lat: f64, zoom: u32) -> f64 {
    match dem_source {
        DemSource::OpenTopography | DemSource::OpenTopographyCop30 => {
            let cos_lat = center_lat.to_radians().cos().max(0.01);
            SRTMGL1_NS_M * cos_lat.sqrt()
        }
        _ => ground_resolution_m(center_lat, zoom),
    }
}

/// Analytically derive a `cols` × `rows` area bbox of `tile_m`-square tiles
/// anchored at `big`'s NW corner or center (Explicit mode). Each tile spans
/// `tile_m` meters on a side; `dlat = tile_m / (EARTH_RADIUS_KM*1000) * 180/π`
/// and `dlon = dlat / cos(lat)` keep the cells ~square in ground meters at the
/// anchor latitude. The latitude span is clamped to the Mercator limit.
fn explicit_bbox(
    big: BBoxLatLon,
    tile_m: f64,
    cols: u32,
    rows: u32,
    anchor: AnchorKind,
) -> BBoxLatLon {
    let deg_per_m_lat = 180.0 / (std::f64::consts::PI * EARTH_RADIUS_KM * 1000.0);
    let dlat_total = tile_m * f64::from(rows) * deg_per_m_lat;
    let anchor_lat = match anchor {
        AnchorKind::NwCorner => big.north,
        AnchorKind::Center => (big.north + big.south) * 0.5,
    };
    let anchor_lon = match anchor {
        AnchorKind::NwCorner => big.west,
        AnchorKind::Center => (big.west + big.east) * 0.5,
    };
    // cos at the mid latitude of the spanned area keeps the E-W metric honest.
    let (north, south) = match anchor {
        AnchorKind::NwCorner => (anchor_lat, anchor_lat - dlat_total),
        AnchorKind::Center => (anchor_lat + dlat_total * 0.5, anchor_lat - dlat_total * 0.5),
    };
    let mid_lat = (north + south) * 0.5;
    let cos_lat = mid_lat.to_radians().cos().max(0.01);
    let dlon_total = tile_m * f64::from(cols) * deg_per_m_lat / cos_lat;
    let (west, east) = match anchor {
        AnchorKind::NwCorner => (anchor_lon, anchor_lon + dlon_total),
        AnchorKind::Center => (anchor_lon - dlon_total * 0.5, anchor_lon + dlon_total * 0.5),
    };
    BBoxLatLon {
        north: north.clamp(-MERCATOR_LAT_LIMIT, MERCATOR_LAT_LIMIT),
        south: south.clamp(-MERCATOR_LAT_LIMIT, MERCATOR_LAT_LIMIT),
        west,
        east,
    }
}

/// Number of columns/rows for `big` at `tile_m` meters per tile, via haversine
/// (spec §3.1). The remainder tile is simply thinner; always ≥1.
fn auto_cols_rows(big: BBoxLatLon, tile_m: f64) -> (u32, u32) {
    let center_lat = (big.north + big.south) * 0.5;
    let width_m = haversine_km(center_lat, big.west, center_lat, big.east) * 1000.0;
    let height_m = haversine_km(big.north, big.west, big.south, big.west) * 1000.0;
    let cols = (width_m / tile_m).ceil().max(1.0) as u32;
    let rows = (height_m / tile_m).ceil().max(1.0) as u32;
    (cols, rows)
}

/// Build the canonical [`GridPlan`] from a big bbox and a [`GridMode`].
///
/// One shared lattice, one forced zoom (a `tile_m`-square centroid probe through
/// [`pick_zoom`]), one [`derive_scale`]; for `ClickMask` the excluded tiles are
/// dropped while every kept tile retains its full-lattice offset; for `Explicit`
/// the area bbox is derived analytically from the anchor before the SAME path.
pub(crate) fn partition(
    big: BBoxLatLon,
    mode: GridMode,
    dem_source: DemSource,
    src_max_zoom: u32,
    studs_per_meter: f32,
    exaggeration: f32,
    micro: bool,
) -> GridPlan {
    let tile_m = mode_tile_m(&mode);

    // Resolve cols/rows and the area bbox per mode. Explicit derives its own
    // area analytically from the anchor; the other two subdivide `big`.
    let (cols, rows, area) = match &mode {
        GridMode::AutoSubdivide { .. } | GridMode::ClickMask { .. } => {
            let (c, r) = auto_cols_rows(big, tile_m);
            (c, r, big)
        }
        GridMode::Explicit { cols, rows, anchor, .. } => {
            let c = (*cols).max(1);
            let r = (*rows).max(1);
            (c, r, explicit_bbox(big, tile_m, c, r, *anchor))
        }
    };

    let (lon_edges, lat_edges) = edge_lattice(area, cols, rows);

    // Forced zoom: a tile_m-square probe at the grid centroid (pillar A). The
    // probe is one representative tile, NOT a per-tile pick (spec §3.2).
    let center_lat = (area.north + area.south) * 0.5;
    let center_lon = (area.west + area.east) * 0.5;
    let deg_per_m_lat = 180.0 / (std::f64::consts::PI * EARTH_RADIUS_KM * 1000.0);
    let half_dlat = tile_m * 0.5 * deg_per_m_lat;
    let half_dlon = half_dlat / center_lat.to_radians().cos().max(0.01);
    let probe = BBoxLatLon {
        north: (center_lat + half_dlat).clamp(-MERCATOR_LAT_LIMIT, MERCATOR_LAT_LIMIT),
        south: (center_lat - half_dlat).clamp(-MERCATOR_LAT_LIMIT, MERCATOR_LAT_LIMIT),
        west: center_lon - half_dlon,
        east: center_lon + half_dlon,
    };
    let zoom = pick_zoom(probe, src_max_zoom);

    // cell_m + derive_scale ONCE for the whole grid (pillar A → uniform pitch).
    let cell_m = cell_m_at(dem_source, center_lat, zoom);
    let (horizontal_scale, vertical_scale) =
        derive_scale(cell_m, studs_per_meter, exaggeration, micro);

    // Realized per-column/row cell counts from the world-px lattice + prefix
    // sums. The lat/lon ref is constant per axis so the rounding is consistent
    // with what each tile's crop will produce along its shared edges.
    let cw = col_cell_widths(&lon_edges, center_lat, zoom);
    let ch = row_cell_heights(&lat_edges, center_lon, zoom);
    let off_x = prefix_sum(&cw);
    let off_y = prefix_sum(&ch);
    let global_cells_w = off_x[cols as usize];
    let global_cells_h = off_y[rows as usize];

    // Excluded set (ClickMask only).
    let excluded: &HashSet<TileId> = match &mode {
        GridMode::ClickMask { excluded, .. } => excluded,
        _ => {
            static EMPTY: std::sync::OnceLock<HashSet<TileId>> = std::sync::OnceLock::new();
            EMPTY.get_or_init(HashSet::new)
        }
    };

    let mut tiles = Vec::with_capacity((cols * rows) as usize);
    for r in 0..rows {
        for c in 0..cols {
            let id = TileId { row: r, col: c };
            if excluded.contains(&id) {
                continue;
            }
            tiles.push(PlannedTile {
                id,
                bbox: BBoxLatLon {
                    west: lon_edges[c as usize],
                    east: lon_edges[c as usize + 1],
                    north: lat_edges[r as usize],
                    south: lat_edges[r as usize + 1],
                },
                off_cells_x: off_x[c as usize],
                off_cells_y: off_y[r as usize],
                cells_w: cw[c as usize],
            });
        }
    }

    GridPlan {
        zoom,
        cell_m,
        horizontal_scale,
        vertical_scale,
        rows,
        cols,
        lon_edges,
        lat_edges,
        tiles,
        global_cells_w,
        global_cells_h,
        name: String::new(),
    }
}

/// Per-tile world offset in brick units for tile `(c, r)` at brick `size`
/// (spec §3.5): cumulative-cells prefix-sum plus the global-centering term.
///
/// ```text
/// world_off_x(c)  = off_cells_x(c) * 2 * size
/// GLOBAL_CENTER_X = -(global_cells_w * size)
/// offset_x(c)     = world_off_x(c) + GLOBAL_CENTER_X
/// ```
///
/// Single-box reduction (`cols=1`, `off_cells_x=0`, `global_cells_w=width`)
/// yields `-(width*size)` — exactly the legacy centering it replaces.
///
/// The per-tile NW cell offsets are read from the tile's stored
/// `off_cells_x/off_cells_y` (the prefix sums `partition` computed ONCE from the
/// shared lattice) — never recomputed, so there is no chance of float drift
/// diverging from the realized crop (spec CROSS-CUTTING RISKS #1).
pub(crate) fn world_offset(plan: &GridPlan, tile: &PlannedTile, size: u16) -> (i32, i32) {
    tile_world_offset(
        tile.off_cells_x,
        tile.off_cells_y,
        plan.global_cells_w,
        plan.global_cells_h,
        size,
    )
}

/// The pure cumulative-cells + global-centering world-offset algebra, factored
/// out of [`world_offset`] so the sculpt heightfield tiler can place its
/// sub-fields with the SAME math without constructing a geographic `GridPlan`
/// (spec §5). `off_cells_{x,y}` is the sub-tile's NW cell index (cumulative cells
/// to the west/north); `global_cells_{w,h}` is the full mosaic extent in cells.
///
/// ```text
/// world_off_x = off_cells_x * 2 * size
/// center_x    = -(global_cells_w * size)
/// offset_x    = world_off_x + center_x
/// ```
///
/// A single-tile reduction (`off_cells = 0`, `global_cells = width`) yields
/// `-(width * size)` — exactly the legacy single-box centering, so a 1×1 sculpt
/// tiling stitches byte-identically to a single mesh (modulo the one spawn).
pub(crate) fn tile_world_offset(
    off_cells_x: u32,
    off_cells_y: u32,
    global_cells_w: u32,
    global_cells_h: u32,
    size: u16,
) -> (i32, i32) {
    let size_i = i32::from(size);
    let world_off_x = (off_cells_x as i32).saturating_mul(2).saturating_mul(size_i);
    let world_off_y = (off_cells_y as i32).saturating_mul(2).saturating_mul(size_i);
    let center_x = -((global_cells_w as i32).saturating_mul(size_i));
    let center_y = -((global_cells_h as i32).saturating_mul(size_i));
    let off_x = world_off_x.saturating_add(center_x);
    let off_y = world_off_y.saturating_add(center_y);

    // World-extent guard (spec correction #4 / §3.6). `Position` is `i32`, but
    // `to_relative` maps it to an i16 `ChunkIndex` (× CHUNK_SIZE=2048) +
    // RelativePosition, so the real per-axis world cap is ±2048×32767, NOT
    // i32::MAX. The far edge of this tile reaches |off| + global_cells*size; a
    // sane 28 km mosaic is ~0.4% of the bound, so this is a far-edge debug-only
    // guard against a pathological studs_per_meter/size that silently CLAMPS via
    // the saturating math above. Debug-only: zero release behavior change.
    debug_assert!(
        offset_fits_chunk_index(off_x, global_cells_w, size)
            && offset_fits_chunk_index(off_y, global_cells_h, size),
        "grid mosaic exceeds the i16 ChunkIndex world extent (±{MAX_CHUNK_INDEX_UNITS} units/axis): \
         off=({off_x},{off_y}), global_cells=({global_cells_w},{global_cells_h}), size={size}",
    );
    (off_x, off_y)
}

/// CHUNK_SIZE for the brdb i16 `ChunkIndex` partition (spec §3.6, brdb
/// `position.rs`). Hardcoded (zero new dep surface, ponytail) — the spec pins it.
const CHUNK_SIZE_UNITS: i64 = 2048;

/// Maximum |position| per axis the i16 `ChunkIndex` can address:
/// `CHUNK_SIZE * i16::MAX` (`to_relative` splits a position into an i16 chunk
/// index × CHUNK_SIZE plus an in-chunk RelativePosition).
const MAX_CHUNK_INDEX_UNITS: i64 = CHUNK_SIZE_UNITS * (i16::MAX as i64);

/// True if the tile's most-negative-to-most-positive world span fits the i16
/// `ChunkIndex` bound: the emitted `offset` plus the full `global_cells * size`
/// extent (the far edge of the mosaic on this axis) must stay within
/// ±[`MAX_CHUNK_INDEX_UNITS`]. Computed in i64 so the bound check itself cannot
/// overflow even for an over-extent mosaic (the thing it is guarding against).
pub(crate) fn offset_fits_chunk_index(offset: i32, global_cells: u32, size: u16) -> bool {
    let extent = i64::from(global_cells) * i64::from(size) * 2;
    let far_edge = i64::from(offset) + extent;
    i64::from(offset).abs() <= MAX_CHUNK_INDEX_UNITS && far_edge.abs() <= MAX_CHUNK_INDEX_UNITS
}

#[cfg(test)]
impl GridPlan {
    /// The constant latitude used to project the lon edges to world-x (grid
    /// center lat) — kept consistent with `partition`'s `col_cell_widths`. Used
    /// only by the seam-contiguity test to project shared meridians.
    fn cell_lat_ref(&self) -> f64 {
        (self.lat_edges[0] + self.lat_edges[self.rows as usize]) * 0.5
    }
}

// ───────────────────────── Pre-commit estimate (pure, spec §6) ──────────────

/// Owned bytes per `brdb::Brick` in the accumulator. Calibration target (spec
/// MUST-ADD) — held at 144 (a `Brick` carries a `BString` asset + `Vec`
/// components + scalars); the write-peak factor below covers the sort/blob copy.
pub(crate) const BRICK_OWNED_BYTES: u64 = 144;

/// Stitched-write peak multiplier over the raw `Vec<Brick>` footprint: `to_unsaved`
/// sorts ALL bricks (itertools `sorted_by`) and `to_pending` holds all blobs, so
/// the single combined `.brdb` write transiently needs ~3× the accumulator
/// (spec §6, the calibration band is 2–4×).
pub(crate) const WRITE_PEAK_FACTOR: u64 = 3;

/// RAM reserve held back from the fits-RAM gate: OS + egui + the growing
/// accumulator + the write-time copy. The accumulator/write term is folded in
/// via `est_brick_vec_bytes` at the call site; this is the fixed OS/UI floor.
pub(crate) const RAM_RESERVE_BYTES: u64 = 12 * 1024 * 1024 * 1024;

/// Per-tile mesh peak in bytes (spec §6). Imagery makes the mesh ~`cells^1.5`
/// (per-pixel-unique color planes, tiles.rs model); a flat colormap collapses to
/// ~unique heights so it is ~`cells*64`. Branch on whether imagery is selected.
pub(crate) fn est_tile_mesh_bytes(cells: u64, imagery: bool) -> u64 {
    if imagery {
        let c = cells as f64;
        (c.powf(1.5) * 40.0) as u64
    } else {
        cells.saturating_mul(64)
    }
}

/// Read `MemAvailable` from `/proc/meminfo` and return it in BYTES. Zero-dep
/// (`std::fs` + a line scan — no `sysinfo` for one number, spec §6). Linux-only;
/// `None` on any failure so callers fall back conservatively.
pub(crate) fn available_ram_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_mem_available_kb(&text).map(|kb| kb.saturating_mul(1024))
}

/// Parse the `MemAvailable:   N kB` line out of a `/proc/meminfo` body. Split
/// out so a fixture string can drive it in tests without touching the filesystem.
fn parse_mem_available_kb(meminfo: &str) -> Option<u64> {
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            return rest.split_whitespace().next()?.parse::<u64>().ok();
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GridEstimate {
    pub(crate) tile_count: u32,
    pub(crate) total_cells: u64,
    pub(crate) est_bricks: u64,
    pub(crate) peak_mesh_bytes: u64,
    pub(crate) est_brick_vec_bytes: u64,
    /// All decoded tile rasters held resident through Phase 1 (`global_min_m`
    /// requires every tile before any mesh): `total_cells * 4B` (f32 heights).
    /// Additive to the mesh peak + accumulator per spec MUST-ADD / cross-cutting
    /// risks — the two-phase peak is mesh + write-vec + this resident raster set.
    pub(crate) est_rasters_bytes: u64,
    pub(crate) parallel_tiles: u32,
    pub(crate) est_seconds: f64,
    /// Any SINGLE tile's brick ceiling (its cell count — greedy mesh ≤ 1
    /// brick/cell) would exceed the per-tile `MAX_BRICKS` cap (spec §6). Distinct
    /// from `over_cell_budget`: that gates the mesh-input cell budget
    /// (`MAX_DEM_CELLS`), this gates the per-tile OUTPUT brick count. Because
    /// `MAX_DEM_CELLS` (400 k) < `MAX_BRICKS` (2 M) a budget-passing tile can
    /// never trip this — it stays honest to its spec meaning rather than aliasing
    /// the cell-budget predicate (spec correction #5).
    pub(crate) over_brick_cap: bool,
    /// Any tile's input cell count exceeds `MAX_DEM_CELLS` at the locked zoom —
    /// the per-tile MESH budget; rejected pre-commit so no mid-build
    /// `GridTooLarge`. Distinct from `over_brick_cap` (the output-brick cap).
    pub(crate) over_cell_budget: bool,
    /// `peak_mesh + brick_vec ≤ available - reserve` at `parallel_tiles ≥ 1`.
    pub(crate) fits_ram: bool,
}

/// Wall-clock seed constants (seconds), seeded from the ~70 k-brick Horsetooth
/// build and refined live from the first completed tile (spec §6). Labeled "~".
const K_FETCH_S_PER_TILE: f64 = 2.0;
/// Mesh seconds scale with `cells` (greedy mesh is ~linear in grid cells for the
/// fill-capped terrain path); seeded so a 75 k-cell tile is ~3 s.
const K_MESH_S_PER_CELL: f64 = 4.0e-5;

/// PURE pre-commit estimate (spec §6): no I/O except the injected `available_ram`.
/// `total_cells` sums `approx_cell_count` over the plan's tiles (the SAME path
/// the fetch walks); `est_bricks` is the conservative `total_cells` ceiling
/// (greedy meshing only reduces count). Memory: `peak_mesh` is
/// `parallel_tiles * est_tile_mesh_bytes(max-tile-cells)` (concurrency-bounded,
/// NOT the sum); `est_brick_vec_bytes` is `est_bricks * BRICK_OWNED_BYTES *
/// WRITE_PEAK_FACTOR`; `est_rasters_bytes` is `total_cells * 4` for the all-tile
/// raster set held resident across the two-phase boundary (spec MUST-ADD). The
/// gates reject a too-big single tile and an over-RAM plan.
pub(crate) fn estimate_grid(
    plan: &GridPlan,
    settings: &GridSettings,
    available_ram: u64,
) -> GridEstimate {
    let imagery = settings.imagery_source != ImagerySource::None;
    let tile_count = plan.tiles.len() as u32;

    let mut total_cells: u64 = 0;
    let mut max_tile_cells: u64 = 0;
    let mut over_budget = false;
    for tile in &plan.tiles {
        let cells = approx_cell_count(tile.bbox, plan.zoom);
        total_cells = total_cells.saturating_add(cells);
        max_tile_cells = max_tile_cells.max(cells);
        if cells > MAX_DEM_CELLS {
            over_budget = true;
        }
    }

    // over_brick_cap is the per-tile OUTPUT brick ceiling (≤ 1 brick/cell under
    // the greedy mesher), distinct from the cell-budget gate above (spec
    // correction #5). Given MAX_DEM_CELLS < MAX_BRICKS this is unreachable for a
    // budget-passing tile — that's fine: the flag stays honest to its meaning.
    let over_brick_cap = max_tile_cells > MAX_BRICKS as u64;

    let est_bricks = total_cells;
    let parallel_tiles = PARALLEL_TILES.max(1);
    let peak_mesh_bytes =
        u64::from(parallel_tiles).saturating_mul(est_tile_mesh_bytes(max_tile_cells, imagery));
    let est_brick_vec_bytes = est_bricks
        .saturating_mul(BRICK_OWNED_BYTES)
        .saturating_mul(WRITE_PEAK_FACTOR);
    // All tile rasters stay resident from Phase 1 until Phase 2 meshing begins
    // (`global_min_m` reduces over every tile first). Each cell is an f32 height
    // (4B). Spec MUST-ADD: this resident set is ADDITIVE to the mesh peak + the
    // write-vec, not masked by the (now-sequential) mesh slack.
    let est_rasters_bytes = total_cells.saturating_mul(4);

    // fits_ram: the resident working set (the all-rasters set + one+ tile mesh +
    // the write-peak brick vec) must fit under `available - reserve`. Reserve
    // folds the fixed OS/UI floor; the accumulator/write copy is in
    // est_brick_vec_bytes; the resident raster set is est_rasters_bytes.
    //
    // The write-peak brick vec ONLY exists for the STITCHED layout — the
    // individual-only path streams each tile straight to disk via `std::mem::take`
    // per tile, so `combined` stays empty and the true peak is one tile's mesh +
    // the resident rasters (spec correction #1 / §7 "the bounded-RAM path that
    // makes 28 km feasible"). Drop est_brick_vec_bytes from `need` when stitched
    // is off so a grid that fails fits_ram stitched can PASS individual-only.
    let write_peak = if settings.output.stitched { est_brick_vec_bytes } else { 0 };
    let need = peak_mesh_bytes
        .saturating_add(write_peak)
        .saturating_add(est_rasters_bytes);
    let budget = available_ram.saturating_sub(RAM_RESERVE_BYTES);
    let fits_ram = need <= budget;

    let est_seconds = {
        let fetch_batches = tile_count.div_ceil(NET_WORKERS as u32) as f64;
        let mesh = tile_count as f64 * (max_tile_cells as f64 * K_MESH_S_PER_CELL);
        fetch_batches * K_FETCH_S_PER_TILE + mesh
    };

    GridEstimate {
        tile_count,
        total_cells,
        est_bricks,
        peak_mesh_bytes,
        est_brick_vec_bytes,
        est_rasters_bytes,
        parallel_tiles,
        est_seconds,
        over_brick_cap,
        over_cell_budget: over_budget,
        fits_ram,
    }
}

// ───────────────────────── Orchestrator types (spec §4/§7) ──────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GridPhase {
    Fetching,
    Meshing,
    WritingCombined,
}

impl GridPhase {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Fetching => "Fetching tiles…",
            Self::Meshing => "Meshing tiles…",
            Self::WritingCombined => "Writing combined world…",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct GridProgress {
    pub(crate) phase: GridPhase,
    pub(crate) tiles_done: u32,
    pub(crate) tiles_total: u32,
    pub(crate) current: Option<TileId>,
    pub(crate) stage: BuildStage,
    pub(crate) stage_fraction: f32,
}

impl GridProgress {
    pub(crate) fn new(tiles_total: u32) -> Self {
        Self {
            phase: GridPhase::Fetching,
            tiles_done: 0,
            tiles_total,
            current: None,
            stage: BuildStage::FetchingTiles,
            stage_fraction: 0.0,
        }
    }
}

#[derive(Debug)]
pub(crate) enum GridError {
    /// A tile failed to fetch/decode/mesh (an all-nodata `EmptyDem` ocean tile,
    /// a network error, or a per-tile over-budget). Carries the grid position.
    Tile { row: u32, col: u32, source: BuildError },
    /// The combined accumulator exceeded `MAX_GRID_BRICKS` (spec §4.3).
    TooManyBricks { count: usize, max: usize },
    /// Output write/install failed (path/dispatch/io).
    Output(String),
    /// The validated `OutputOptions` were empty in some axis (spec §7).
    BadOutputOptions(String),
    Cancelled,
}

impl std::fmt::Display for GridError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tile { row, col, source } => {
                write!(f, "tile r{row} c{col} failed: {source}")
            }
            Self::TooManyBricks { count, max } => write!(
                f,
                "combined world reached {count} bricks (limit {max}) — fewer/finer tiles, or keep-individual-only",
            ),
            Self::Output(msg) => write!(f, "output write failed: {msg}"),
            Self::BadOutputOptions(msg) => write!(f, "invalid output options: {msg}"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for GridError {}

#[derive(Debug)]
pub(crate) struct GridOutcome {
    pub(crate) written: Vec<PathBuf>,
    pub(crate) installed: Vec<PathBuf>,
    pub(crate) warnings: Vec<String>,
    pub(crate) brick_count: usize,
    pub(crate) tiles: u32,
}

impl OutputOptions {
    /// At least one format AND at least one layout must be selected (spec §7).
    pub(crate) fn validate(&self) -> Result<(), GridError> {
        if !self.brdb && !self.brz {
            return Err(GridError::BadOutputOptions(
                "select at least one format (.brdb or .brz)".to_owned(),
            ));
        }
        if !self.stitched && !self.individual {
            return Err(GridError::BadOutputOptions(
                "select at least one layout (stitched or individual)".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Zero-padded grid suffix `_r{row}_c{col}` whose width fits the grid size, so
/// the per-tile files sort lexicographically in grid order (spec §7). The width
/// is `digits(max(rows, cols))` — e.g. a 2×10 grid pads to two digits
/// (`_r0_c00 .. _r1_c09`) so `_c09` sorts after `_c08` rather than `_c1` < `_c9`.
fn tile_suffix(rows: u32, cols: u32, id: TileId) -> String {
    let width = rows.max(cols).max(1).to_string().len();
    format!("_r{:0width$}_c{:0width$}", id.row, id.col, width = width)
}

/// A fetched, decoded, budget-checked tile raster paired with its plan slot.
/// Carries the tile `bbox` (Phase-1 build time) so Phase 2 can re-derive the
/// per-tile request WITHOUT an O(tiles) `plan.tiles.find` per tile (spec
/// correction #3 — `rasters` is already in plan order, 1:1).
struct TileRaster {
    id: TileId,
    bbox: BBoxLatLon,
    raster: DemRaster,
    offset: (i32, i32),
}

/// Build the synthetic per-tile `BuildRequest` that reuses the single-box
/// fetch and mesh stages (spec correction #6). Density is forced full-res (a
/// `density_factor` of one) because the downsample is not edge-aware
/// (correction #3); the stamped uniform `horizontal_scale`/`vertical_scale` come
/// from the plan (pillar A).
fn tile_request(plan: &GridPlan, settings: &GridSettings, bbox: BBoxLatLon) -> BuildRequest {
    BuildRequest {
        bbox,
        name: plan.name.clone(),
        dem_source: settings.dem_source,
        imagery_source: settings.imagery_source,
        mapbox_token: settings.mapbox_token.clone(),
        opentopo_key: settings.opentopo_key.clone(),
        vertical_scale: plan.vertical_scale,
        density_factor: 1,
        horizontal_scale: plan.horizontal_scale,
        block_type: settings.block_type,
        glow: settings.glow,
        no_collision: settings.no_collision,
        install_to_brickadia: settings.output.install_to_brickadia,
        overwrite_world: settings.overwrite,
        // The grid (1:1 tiled) path does not yet expose omit/floor — a per-tile
        // omit needs the shared global_min threaded into each tile_request. Off
        // here keeps the grid build byte-identical; standard-fetch omit is wired.
        omit_below_m: 0.0,
        floor_level_m: 0.0,
    }
}

/// Run the two-phase grid build (spec §4). PHASE 1 fetches + decodes EVERY tile
/// bounded-parallel on a LOCAL pool of [`NET_WORKERS`] (`pool.install(par_iter)`)
/// with `forced_zoom = plan.zoom`, records realized `cells_w/cells_h + min_m`,
/// and reduces to `global_min_m`; an all-nodata tile is a hard
/// [`GridError::Tile`]. PHASE 2 meshes each tile SEQUENTIALLY (no tile-worker
/// pool — each `build_one_tile` fans plane-mesh across the FULL global rayon pool
/// per BINDING CORRECTION #1) against `global_min_m`, `base_override = Some(0)`,
/// `offset = world_offset(...)`, then routes to the output layer.
pub(crate) fn run_grid_build(
    plan: GridPlan,
    settings: GridSettings,
    progress: Arc<std::sync::Mutex<GridProgress>>,
    cancel: Arc<AtomicBool>,
) -> Result<GridOutcome, GridError> {
    settings.output.validate()?;

    let tiles_total = plan.tiles.len() as u32;
    let size = cell_size_units(plan.horizontal_scale, settings.block_type.micro());

    // ── PHASE 1: fetch + decode ALL tiles (bounded-parallel, I/O-bound). ──────
    set_phase(&progress, GridPhase::Fetching, tiles_total);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(NET_WORKERS)
        .build()
        .map_err(|e| GridError::Output(format!("fetch pool: {e}")))?;

    // A no-op per-tile progress sink: the fetch runs concurrently so a precise
    // per-tile stage bar would race; the overall tiles_done counter advances
    // below as each tile resolves. The build stage callback is required by the
    // fetch signature but its fraction is not surfaced during parallel fetch.
    let fetch_progress: super::build::ProgressFn = Arc::new(|_, _| {});

    let done = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let results: Vec<Result<TileRaster, GridError>> = pool.install(|| {
        plan.tiles
            .par_iter()
            .map(|tile| {
                if cancel.load(Ordering::Relaxed) {
                    return Err(GridError::Cancelled);
                }
                let request = tile_request(&plan, &settings, tile.bbox);
                let raster = fetch_and_decode_dem(
                    &request,
                    Arc::clone(&fetch_progress),
                    Arc::clone(&cancel),
                )
                .map_err(|source| GridError::Tile { row: tile.id.row, col: tile.id.col, source })?;
                // Force full-res (density=1) — no downsample (correction #3) — then
                // reject any tile over the per-tile cell budget pre-commit.
                let raster = downsample(&raster, 1);
                enforce_cell_budget(raster.width, raster.height).map_err(|source| {
                    GridError::Tile { row: tile.id.row, col: tile.id.col, source }
                })?;
                let offset = world_offset(&plan, tile, size);
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                if let Ok(mut p) = progress.lock() {
                    p.tiles_done = n;
                    p.current = Some(tile.id);
                }
                Ok(TileRaster { id: tile.id, bbox: tile.bbox, raster, offset })
            })
            .collect()
    });

    let mut rasters: Vec<TileRaster> = Vec::with_capacity(results.len());
    for r in results {
        rasters.push(r?);
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(GridError::Cancelled);
    }

    // global_min_m = min over all tiles' realized per-raster minimum (pillar B).
    let global_min_m = rasters
        .iter()
        .map(|t| t.raster.min_m)
        .fold(f32::INFINITY, f32::min);

    // ── PHASE 2: mesh each tile SEQUENTIALLY + route output (spec §4.2/§7). ───
    set_phase(&progress, GridPhase::Meshing, tiles_total);
    let opts = settings.output;
    let mesh_progress: super::build::ProgressFn = {
        let progress = Arc::clone(&progress);
        Arc::new(move |stage, frac| {
            if let Ok(mut p) = progress.lock() {
                p.stage = stage;
                p.stage_fraction = frac;
            }
        })
    };

    // The combined accumulator only exists when `stitched` is selected; for the
    // individual-only path it stays empty and each tile streams to disk (the
    // bounded-RAM path, spec §7). Reserve it up front to the cell-count ceiling
    // (greedy meshing only reduces brick count) so the per-tile `extend` never
    // capacity-doubles — that doubling would transiently hold the old + new
    // contiguous Brick arrays (spec §4.3 mandates the reserve). Cap at
    // MAX_GRID_BRICKS so a conservative over-estimate can't over-reserve past the
    // hard ceiling the loop already enforces.
    let mut combined: Vec<brdb::Brick> = Vec::new();
    if opts.stitched {
        let est_bricks: u64 = rasters
            .iter()
            .map(|t| u64::from(t.raster.width).saturating_mul(u64::from(t.raster.height)))
            .fold(0u64, u64::saturating_add)
            .min(MAX_GRID_BRICKS as u64);
        combined.reserve_exact(est_bricks as usize);
    }
    let mut written: Vec<PathBuf> = Vec::new();
    let mut installed: Vec<PathBuf> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let builds = builds_dir().map_err(|e| GridError::Output(e.to_string()))?;
    std::fs::create_dir_all(&builds).map_err(|e| GridError::Output(e.to_string()))?;
    let stem = sanitize_name(&plan.name);

    for (i, tr) in rasters.into_iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(GridError::Cancelled);
        }
        if let Ok(mut p) = progress.lock() {
            p.tiles_done = i as u32;
            p.current = Some(tr.id);
        }
        // Re-derive the per-tile request from the raster's own bbox (carried at
        // Phase-1 build time). `rasters` was collected by `plan.tiles.par_iter()`,
        // so it is already in plan order, 1:1 — no O(tiles) `find` per tile
        // (spec correction #3). The bbox drives the cropped imagery fetch inside
        // build_one_tile, aligned to the raster.
        let request = tile_request(&plan, &settings, tr.bbox);
        let mut bricks = build_one_tile(
            &request,
            tr.raster,
            global_min_m,
            tr.offset,
            Some(0),
            Arc::clone(&mesh_progress),
            Arc::clone(&cancel),
        )
        .map_err(|source| GridError::Tile { row: tr.id.row, col: tr.id.col, source })?;

        // Memory routing (spec §7):
        //   individual && !stitched : STREAM each tile to disk (never accumulate
        //                             — the bounded-RAM path that makes 28 km fit)
        //   individual && stitched   : write the tile AND extend the accumulator
        //                             (one bounded clone — terrain bricks are
        //                             id-less/component-empty, spec §3.5)
        //   stitched && !individual  : accumulate only
        if opts.individual {
            let suffix = tile_suffix(plan.rows, plan.cols, tr.id);
            // Build the per-tile World once. If also stitching, clone so the same
            // bricks feed the accumulator; otherwise move them straight in (the
            // accumulator stays empty on the individual-only path).
            let tile_bricks = if opts.stitched { bricks.clone() } else { std::mem::take(&mut bricks) };
            let tile_world = bricks_to_save(tile_bricks);
            write_tile_outputs(
                &tile_world,
                &builds,
                &format!("{stem}{suffix}"),
                opts,
                &mut written,
                &mut installed,
                &mut warnings,
            )?;
        }

        // Stitched accumulation: extend ONE Vec across all tiles (spec §7). On
        // the individual-only path `bricks` was moved out above, so this is a
        // no-op extend of an empty Vec and `combined` never grows (bounded RAM).
        //
        // Check the cap BEFORE the extend (spec correction #2): an over-limit tile
        // must never be allocated into the accumulator — bail with the post-extend
        // count it WOULD reach, but without the doubling allocation that extend
        // would force on the already-full Vec.
        if opts.stitched {
            let would_be = combined.len() + bricks.len();
            if would_be > MAX_GRID_BRICKS {
                cancel.store(true, Ordering::Relaxed);
                return Err(GridError::TooManyBricks {
                    count: would_be,
                    max: MAX_GRID_BRICKS,
                });
            }
            combined.extend(bricks);
        }

        if let Ok(mut p) = progress.lock() {
            p.tiles_done = (i as u32) + 1;
        }
    }

    let brick_count = combined.len();

    // Stitched output: `bricks_to_save` ONCE over the union → one spawn (spec §7).
    if opts.stitched {
        set_phase(&progress, GridPhase::WritingCombined, tiles_total);
        let world = bricks_to_save(combined);
        write_tile_outputs(
            &world,
            &builds,
            &stem,
            opts,
            &mut written,
            &mut installed,
            &mut warnings,
        )?;
    }

    Ok(GridOutcome {
        written,
        installed,
        warnings,
        brick_count,
        tiles: tiles_total,
    })
}

/// Write a built `World` to `builds_dir` in each selected format, deleting any
/// stale `.brdb` destination first (open-if-exists + append, spec correction #7;
/// `.brz` truncates). Installs ONLY `.brdb` to Worlds/ when requested (do NOT
/// auto-install `.brz` — correction #8; it stays in `builds_dir`). Install
/// failures degrade to a warning, never fatal (the file remains on disk).
fn write_tile_outputs(
    world: &brdb::World,
    builds: &std::path::Path,
    stem: &str,
    opts: OutputOptions,
    written: &mut Vec<PathBuf>,
    installed: &mut Vec<PathBuf>,
    warnings: &mut Vec<String>,
) -> Result<(), GridError> {
    let mut exts: Vec<&str> = Vec::new();
    if opts.brdb {
        exts.push("brdb");
    }
    if opts.brz {
        exts.push("brz");
    }
    for ext in exts {
        let path = builds.join(format!("{stem}.{ext}"));
        // Delete a stale destination before writing: `.brdb` is open-if-exists +
        // append, so a leftover file would pile revisions (correction #7).
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|e| GridError::Output(format!("removing stale {}: {e}", path.display())))?;
        }
        let path_str = path
            .to_str()
            .ok_or_else(|| GridError::Output(format!("non-UTF-8 path {}", path.display())))?;
        write_save_world(world, path_str).map_err(GridError::Output)?;
        written.push(path.clone());

        // Install to Brickadia's Saved tree (.brdb→Worlds/, .brz→Prefabs/) —
        // both paths confirmed against a live install. Non-fatal on failure.
        if opts.install_to_brickadia {
            match install_save(&path, ext, false) {
                Ok(dest) => installed.push(dest),
                Err(e) => warnings.push(format!(
                    "install of {} skipped ({e}) — the save remains in {}",
                    path.display(),
                    builds.display(),
                )),
            }
        }
    }
    Ok(())
}

/// Stamp a new phase onto the shared progress (resets the per-tile counter).
fn set_phase(progress: &Arc<std::sync::Mutex<GridProgress>>, phase: GridPhase, tiles_total: u32) {
    if let Ok(mut p) = progress.lock() {
        p.phase = phase;
        p.tiles_total = tiles_total;
        if phase == GridPhase::Meshing || phase == GridPhase::Fetching {
            p.tiles_done = 0;
        }
        p.stage = match phase {
            GridPhase::Fetching => BuildStage::FetchingTiles,
            GridPhase::Meshing => BuildStage::GeneratingBricks,
            GridPhase::WritingCombined => BuildStage::WritingSave,
        };
        p.stage_fraction = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ~4 km box near Horsetooth, a faithful Mercator test fixture.
    fn big_bbox() -> BBoxLatLon {
        BBoxLatLon { north: 40.560, south: 40.523, east: -105.131, west: -105.183 }
    }

    #[test]
    fn partition_shares_exact_edges() {
        // A 3×2 lattice: every interior edge is shared BY VALUE (exact f64 ==)
        // between neighbors — the #1 seam guard (spec §10, pillar A).
        let plan = partition(
            big_bbox(),
            GridMode::AutoSubdivide { tile_m: 1500.0 },
            DemSource::AwsTerrarium,
            15,
            1.0,
            1.0,
            false,
        );
        // Force a known shape independent of haversine rounding by also probing
        // an Explicit 3×2 (deterministic cols/rows).
        let plan = if plan.cols >= 2 && plan.rows >= 2 {
            plan
        } else {
            partition(
                big_bbox(),
                GridMode::Explicit {
                    tile_m: 1000.0,
                    cols: 3,
                    rows: 2,
                    anchor: AnchorKind::NwCorner,
                },
                DemSource::AwsTerrarium,
                15,
                1.0,
                1.0,
                false,
            )
        };
        let cols = plan.cols;
        let rows = plan.rows;
        let at = |r: u32, c: u32| -> &PlannedTile {
            plan.tiles
                .iter()
                .find(|t| t.id.row == r && t.id.col == c)
                .expect("tile present (no exclusions)")
        };
        // East edge of (r,c) == west edge of (r,c+1), exact f64 ==.
        for r in 0..rows {
            for c in 0..cols - 1 {
                assert_eq!(
                    at(r, c).bbox.east,
                    at(r, c + 1).bbox.west,
                    "interior meridian must be ONE f64 value (r{r} c{c}|{})",
                    c + 1
                );
            }
        }
        // South edge of (r,c) == north edge of (r+1,c), exact f64 ==.
        for r in 0..rows - 1 {
            for c in 0..cols {
                assert_eq!(
                    at(r, c).bbox.south,
                    at(r + 1, c).bbox.north,
                    "interior parallel must be ONE f64 value (r{r}|{} c{c})",
                    r + 1
                );
            }
        }
    }

    #[test]
    fn crop_contiguity_world_px() {
        // For two horizontally adjacent tiles sharing a meridian at the locked
        // zoom: round(world_px(shared edge)) is identical from A-east and
        // B-west, AND A's last absolute column +1 == B's first absolute column
        // (spec §10, the seam regression).
        let plan = partition(
            big_bbox(),
            GridMode::Explicit {
                tile_m: 1000.0,
                cols: 3,
                rows: 1,
                anchor: AnchorKind::NwCorner,
            },
            DemSource::AwsTerrarium,
            15,
            1.0,
            1.0,
            false,
        );
        let z = plan.zoom;
        let lat = plan.cell_lat_ref();
        let a = &plan.tiles[0];
        let b = &plan.tiles[1];
        assert_eq!(a.id.col + 1, b.id.col, "fixture must be horizontally adjacent");

        // Shared edge rounds identically from either side (it IS one f64 value).
        let a_east_px = lat_lon_to_world_px(lat, a.bbox.east, z).0.round() as i64;
        let b_west_px = lat_lon_to_world_px(lat, b.bbox.west, z).0.round() as i64;
        assert_eq!(
            a_east_px, b_west_px,
            "shared meridian must project to the same absolute world pixel",
        );

        // A's first absolute column + its realized width == B's first column.
        let a_west_px = lat_lon_to_world_px(lat, a.bbox.west, z).0.round() as i64;
        let a_last_abs_col = a_west_px + i64::from(a.cells_w) - 1;
        let b_first_abs_col = b_west_px;
        assert_eq!(
            a_last_abs_col + 1,
            b_first_abs_col,
            "A's last DEM column +1 must equal B's first (consecutive real pixels)",
        );
    }

    #[test]
    fn world_offset_abutment() {
        // tile(0,1)'s min-X brick must land exactly 2*size past tile(0,0)'s
        // max-X — the shared edge world-x coincides (spec §10, pillar C).
        let plan = partition(
            big_bbox(),
            GridMode::Explicit {
                tile_m: 1000.0,
                cols: 2,
                rows: 1,
                anchor: AnchorKind::NwCorner,
            },
            DemSource::AwsTerrarium,
            15,
            1.0,
            1.0,
            false,
        );
        let size = plan.horizontal_scale * 5; // normal (non-micro) brick size.
        let size_i = i32::from(size);
        let (off0_x, _) = world_offset(&plan, &plan.tiles[0], size);
        let (off1_x, _) = world_offset(&plan, &plan.tiles[1], size);

        // tile0 occupies columns [0, cells_w0); its max cell-center x in units is
        // (cells_w0-1)*2*size + size + off0_x; the abutting boundary x is
        // cells_w0*2*size + off0_x = tile1's first cell left edge = off1_x.
        let w0 = plan.tiles[0].cells_w as i32;
        let tile0_right_boundary = w0 * 2 * size_i + off0_x;
        assert_eq!(
            tile0_right_boundary, off1_x,
            "tile1 origin must abut tile0's right boundary (no gap, no overlap)",
        );

        // tile1's first cell min-X brick center sits exactly 2*size past tile0's
        // last cell min-X brick center along the shared edge.
        let tile0_last_min_x = (w0 - 1) * 2 * size_i + off0_x;
        let tile1_first_min_x = off1_x;
        assert_eq!(
            tile1_first_min_x - tile0_last_min_x,
            2 * size_i,
            "boundary pitch must equal the interior 2*size pitch",
        );
    }

    #[test]
    fn world_offset_single_box_reduction() {
        // A 1×1 grid must reduce to the legacy centering -(width*size) on both
        // axes (spec §3.5 byte-identity proof).
        let plan = partition(
            big_bbox(),
            GridMode::Explicit {
                tile_m: 1000.0,
                cols: 1,
                rows: 1,
                anchor: AnchorKind::NwCorner,
            },
            DemSource::AwsTerrarium,
            15,
            1.0,
            1.0,
            false,
        );
        let size = plan.horizontal_scale * 5;
        let (ox, oy) = world_offset(&plan, &plan.tiles[0], size);
        assert_eq!(
            ox,
            -(plan.global_cells_w as i32 * i32::from(size)),
            "single-box offset_x must be -(width*size)",
        );
        assert_eq!(
            oy,
            -(plan.global_cells_h as i32 * i32::from(size)),
            "single-box offset_y must be -(height*size)",
        );
    }

    #[test]
    fn world_offset_chunk_index_bound() {
        // The i16 ChunkIndex world-extent bound (spec §3.6 / correction #4):
        // a sane large mosaic passes `offset_fits_chunk_index`, and the bound is
        // exactly CHUNK_SIZE(2048) * i16::MAX.
        assert_eq!(
            MAX_CHUNK_INDEX_UNITS,
            2048 * 32767,
            "the i16 ChunkIndex per-axis cap must be CHUNK_SIZE * i16::MAX",
        );
        // A 28 km mosaic (~280 k cells) at size=1 reaches ~560 k units far-edge —
        // well under the ~67 M bound, so a real grid's world_offset never trips.
        let plan = partition(
            big_bbox(),
            GridMode::Explicit { tile_m: 1000.0, cols: 4, rows: 4, anchor: AnchorKind::NwCorner },
            DemSource::AwsTerrarium,
            15,
            1.0,
            1.0,
            false,
        );
        let size = plan.horizontal_scale * 5;
        for tile in &plan.tiles {
            let (ox, oy) = world_offset(&plan, tile, size); // debug_assert must not fire.
            assert!(
                offset_fits_chunk_index(ox, plan.global_cells_w, size)
                    && offset_fits_chunk_index(oy, plan.global_cells_h, size),
                "a sane mosaic must fit the ChunkIndex bound (off {ox},{oy})",
            );
        }
        // A pathological extent that overruns the bound must be REJECTED by the
        // predicate (the debug_assert's condition), proving the guard is honest.
        let over_cells = (MAX_CHUNK_INDEX_UNITS / 2 + 1) as u32; // *size(1)*2 > bound.
        assert!(
            !offset_fits_chunk_index(0, over_cells, 1),
            "an over-extent mosaic must fail the ChunkIndex bound",
        );
    }

    #[test]
    fn uniform_scale_across_tiles() {
        // One (horizontal_scale, vertical_scale) derived once for the grid →
        // every tile's lat band must yield the SAME derive_scale, so `size` is
        // bit-identical across rows (spec §10, pillar A).
        let plan = partition(
            big_bbox(),
            GridMode::Explicit {
                tile_m: 1000.0,
                cols: 2,
                rows: 3,
                anchor: AnchorKind::NwCorner,
            },
            DemSource::AwsTerrarium,
            15,
            1.0,
            1.0,
            false,
        );
        // Re-derive at each row band's mid latitude with the SAME cell_m the
        // plan locked; all must match the plan's single (hscale, vertical).
        for r in 0..plan.rows {
            let band_mid = (plan.lat_edges[r as usize] + plan.lat_edges[r as usize + 1]) * 0.5;
            let cell_m = cell_m_at(DemSource::AwsTerrarium, band_mid, plan.zoom);
            let (hs, _v) = derive_scale(cell_m, 1.0, 1.0, false);
            assert_eq!(
                hs, plan.horizontal_scale,
                "row {r} (lat {band_mid:.4}) derived a different hscale — size would differ",
            );
        }
    }

    #[test]
    fn click_mask_drops_excluded_keeps_offsets() {
        // ClickMask excludes a middle tile; the survivors keep their full-lattice
        // offsets (excluding a tile must NOT renumber its neighbors).
        let base = partition(
            big_bbox(),
            GridMode::AutoSubdivide { tile_m: 1500.0 },
            DemSource::AwsTerrarium,
            15,
            1.0,
            1.0,
            false,
        );
        // Use Explicit for a deterministic 3×1 so an interior tile exists.
        let full = partition(
            big_bbox(),
            GridMode::Explicit {
                tile_m: 1000.0,
                cols: 3,
                rows: 1,
                anchor: AnchorKind::NwCorner,
            },
            DemSource::AwsTerrarium,
            15,
            1.0,
            1.0,
            false,
        );
        let _ = base; // exercise the auto path too.
        let mut excluded = HashSet::new();
        excluded.insert(TileId { row: 0, col: 1 });
        let masked = partition(
            big_bbox(),
            GridMode::ClickMask { tile_m: 1000.0, excluded },
            DemSource::AwsTerrarium,
            15,
            1.0,
            1.0,
            false,
        );
        // The masked path's cols/rows came from auto_cols_rows over the same box;
        // align the comparison to that lattice. The dropped tile is absent.
        assert!(
            !masked.tiles.iter().any(|t| t.id == TileId { row: 0, col: 1 }),
            "excluded tile must be dropped",
        );
        // A surviving tile keeps the offset it has in the unmasked lattice.
        let same_mode_full = partition(
            big_bbox(),
            GridMode::AutoSubdivide { tile_m: 1000.0 },
            DemSource::AwsTerrarium,
            15,
            1.0,
            1.0,
            false,
        );
        for kept in &masked.tiles {
            let twin = same_mode_full
                .tiles
                .iter()
                .find(|t| t.id == kept.id)
                .expect("kept tile exists in the unmasked lattice");
            assert_eq!(
                (kept.off_cells_x, kept.off_cells_y),
                (twin.off_cells_x, twin.off_cells_y),
                "exclusion must not shift a survivor's offset",
            );
        }
        let _ = full;
    }

    #[test]
    fn explicit_center_vs_nw_same_shape() {
        // Explicit Center and NwCorner over the same box produce the SAME cols×
        // rows and the SAME locked zoom/scale — only the geographic placement
        // shifts. Guards the analytic anchor derivation (spec §10).
        let nw = partition(
            big_bbox(),
            GridMode::Explicit {
                tile_m: 800.0,
                cols: 2,
                rows: 2,
                anchor: AnchorKind::NwCorner,
            },
            DemSource::AwsTerrarium,
            15,
            1.0,
            1.0,
            false,
        );
        let ctr = partition(
            big_bbox(),
            GridMode::Explicit {
                tile_m: 800.0,
                cols: 2,
                rows: 2,
                anchor: AnchorKind::Center,
            },
            DemSource::AwsTerrarium,
            15,
            1.0,
            1.0,
            false,
        );
        assert_eq!((nw.cols, nw.rows), (ctr.cols, ctr.rows));
        assert_eq!(nw.tiles.len(), ctr.tiles.len());
        assert_eq!(nw.zoom, ctr.zoom, "anchor mode must not change the locked zoom");
        // horizontal_scale (the integer driving `size`) must be identical — this
        // is the seam-relevant invariant. vertical_scale tracks cell_m, which
        // varies sub-0.1% with the anchor's center latitude, so allow tolerance.
        assert_eq!(
            nw.horizontal_scale, ctr.horizontal_scale,
            "anchor mode must not change the integer horizontal scale",
        );
        let vrel = (nw.vertical_scale - ctr.vertical_scale).abs() / nw.vertical_scale;
        assert!(
            vrel < 1e-2,
            "anchor mode must not materially change vertical scale ({} vs {})",
            nw.vertical_scale,
            ctr.vertical_scale,
        );
        // The mosaic extent in cells is placement-dependent (Mercator world-px
        // rounds differently at slightly different latitudes) so it may differ by
        // a cell between the two anchors — assert it stays within 1% (same area).
        let dw = (nw.global_cells_w as i64 - ctr.global_cells_w as i64).unsigned_abs();
        assert!(
            dw * 100 <= u64::from(nw.global_cells_w),
            "anchor mode must not materially change the mosaic width ({} vs {})",
            nw.global_cells_w,
            ctr.global_cells_w,
        );
        // Both span ~cols*tile_m meters of longitude; they differ only by the
        // cos(mid-lat) metric between the NW band (north-anchored) and the
        // center band (centered) — a sub-0.1% difference, NOT a shape change.
        let nw_span = nw.lon_edges[nw.cols as usize] - nw.lon_edges[0];
        let ctr_span = ctr.lon_edges[ctr.cols as usize] - ctr.lon_edges[0];
        let rel = (nw_span - ctr_span).abs() / nw_span;
        assert!(
            rel < 1e-3,
            "anchor mode must not materially change the lon span ({nw_span} vs {ctr_span}, rel {rel})",
        );
    }

    #[test]
    fn global_cells_match_total_cell_count_axis() {
        // The mosaic extent equals the sum of realized per-column widths — the
        // figure the estimate and centering both consume (spec §2).
        let plan = partition(
            big_bbox(),
            GridMode::Explicit {
                tile_m: 1000.0,
                cols: 4,
                rows: 1,
                anchor: AnchorKind::NwCorner,
            },
            DemSource::AwsTerrarium,
            15,
            1.0,
            1.0,
            false,
        );
        let sum_w: u32 = (0..plan.cols)
            .map(|c| plan.tiles.iter().find(|t| t.id.col == c).unwrap().cells_w)
            .sum();
        assert_eq!(
            sum_w, plan.global_cells_w,
            "global_cells_w must equal Σ realized column widths",
        );
        // And approx_cell_count over the whole area is in the same ballpark
        // (sanity that the lattice covers the box, not a strict equality).
        let whole = BBoxLatLon {
            north: plan.lat_edges[0],
            south: plan.lat_edges[plan.rows as usize],
            east: plan.lon_edges[plan.cols as usize],
            west: plan.lon_edges[0],
        };
        let approx = approx_cell_count(whole, plan.zoom);
        assert!(approx > 0, "non-empty area must report cells: {approx}");
    }

    // ───────────────────────── Stage 4: estimate + output ───────────────────

    /// A `GridSettings` with no imagery, flat colormap, the given output options.
    fn settings_with(output: OutputOptions, imagery: ImagerySource) -> GridSettings {
        GridSettings {
            dem_source: DemSource::AwsTerrarium,
            imagery_source: imagery,
            mapbox_token: None,
            opentopo_key: None,
            block_type: BlockType::SmoothTile,
            glow: false,
            no_collision: false,
            output,
            overwrite: false,
        }
    }

    fn explicit_plan(cols: u32, rows: u32, tile_m: f64) -> GridPlan {
        partition(
            big_bbox(),
            GridMode::Explicit { tile_m, cols, rows, anchor: AnchorKind::NwCorner },
            DemSource::AwsTerrarium,
            15,
            1.0,
            1.0,
            false,
        )
    }

    const GB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn estimate_grid_deterministic() {
        // A 2×2 of 1 km tiles → tile_count==4, total_cells == Σ approx_cell_count
        // (the SAME path the fetch walks), est_bricks==total_cells, fits at 60 GB.
        let plan = explicit_plan(2, 2, 1000.0);
        let settings = settings_with(OutputOptions::default(), ImagerySource::None);
        let est = estimate_grid(&plan, &settings, 60 * GB);
        assert_eq!(est.tile_count, 4);
        let pinned: u64 = plan
            .tiles
            .iter()
            .map(|t| approx_cell_count(t.bbox, plan.zoom))
            .sum();
        assert_eq!(est.total_cells, pinned, "total_cells must sum approx_cell_count");
        assert_eq!(est.est_bricks, est.total_cells, "est_bricks is the cell ceiling");
        assert!(est.fits_ram, "a 2×2 1 km grid must fit in 60 GB");
        // Determinism: a second call with identical inputs returns the same value.
        let est2 = estimate_grid(&plan, &settings, 60 * GB);
        assert_eq!(est, est2, "estimate must be a pure deterministic function");
    }

    /// Force a single-tile plan onto an explicit locked zoom (bypassing
    /// `pick_zoom`'s per-bbox back-off) so the over-budget gate can be exercised
    /// at the source ceiling (BINDING CORRECTION #2: tiles run at the ceiling, and
    /// a too-big tile is rejected pre-commit rather than mid-build).
    fn forced_zoom_single_tile_plan(bbox: BBoxLatLon, zoom: u32) -> GridPlan {
        let off_x = [0u32, approx_cell_count(bbox, zoom) as u32];
        GridPlan {
            zoom,
            cell_m: ground_resolution_m((bbox.north + bbox.south) * 0.5, zoom),
            horizontal_scale: 1,
            vertical_scale: 1.0,
            rows: 1,
            cols: 1,
            lon_edges: vec![bbox.west, bbox.east],
            lat_edges: vec![bbox.north, bbox.south],
            tiles: vec![PlannedTile {
                id: TileId { row: 0, col: 0 },
                bbox,
                off_cells_x: 0,
                off_cells_y: 0,
                cells_w: off_x[1],
            }],
            global_cells_w: off_x[1],
            global_cells_h: 1,
            name: "budget".to_owned(),
        }
    }

    #[test]
    fn over_cell_budget_precheck() {
        // A ~4 km tile forced to z15 blows MAX_DEM_CELLS → over_cell_budget is set
        // pre-commit so the UI blocks before any heavy fetch (no mid-build
        // GridTooLarge). Forcing z15 mirrors the production policy of meshing at
        // the source ceiling; `pick_zoom` would back off and hide the condition.
        let bbox = big_bbox(); // ~4 km across.
        let plan = forced_zoom_single_tile_plan(bbox, 15);
        let settings = settings_with(OutputOptions::default(), ImagerySource::None);
        let cells = approx_cell_count(bbox, 15);
        assert!(
            cells > MAX_DEM_CELLS,
            "fixture precondition: a 4 km z15 tile must exceed {MAX_DEM_CELLS} cells (got {cells})",
        );
        let est = estimate_grid(&plan, &settings, 60 * GB);
        assert!(est.over_cell_budget, "an over-budget tile must trip over_cell_budget");
        // over_brick_cap is now the DISTINCT per-tile OUTPUT brick ceiling
        // (cells > MAX_BRICKS), not an alias of the cell-budget gate (spec
        // correction #5). It matches the honest per-tile brick prediction.
        assert_eq!(
            est.over_brick_cap,
            cells > MAX_BRICKS as u64,
            "over_brick_cap must track the per-tile MAX_BRICKS ceiling, not the cell budget",
        );

        // A within-budget plan trips NEITHER gate (negative control).
        let small = explicit_plan(1, 1, 1000.0);
        let est_small = estimate_grid(&small, &settings, 60 * GB);
        assert!(!est_small.over_cell_budget, "a within-budget tile must not trip the cell gate");
        assert!(!est_small.over_brick_cap, "a within-budget tile must not trip the brick cap");
    }

    #[test]
    fn fits_ram_gate_flips() {
        // The same plan fits at high RAM and does NOT fit at low RAM — the gate
        // is driven by available_ram (spec §6). Use a large satellite grid so the
        // brick-vec dominates.
        let plan = explicit_plan(8, 8, 1000.0);
        let settings = settings_with(OutputOptions::default(), ImagerySource::EsriWorldImagery);
        let lots = estimate_grid(&plan, &settings, 256 * GB);
        let scarce = estimate_grid(&plan, &settings, 8 * GB);
        assert!(lots.fits_ram, "256 GB must clear the gate");
        assert!(!scarce.fits_ram, "8 GB must fail the gate for the same plan");

        // spec correction #1: the SAME scarce-RAM plan that fails fits_ram when
        // STITCHED must PASS as individual-only streaming — the write-peak brick
        // vec is dropped from `need` because `combined` never accumulates on that
        // path (peak = one tile's mesh + resident rasters). Pick a RAM level that
        // sits BELOW the stitched need but ABOVE the streaming need so the layout
        // is the deciding factor (not just more RAM).
        let stitched_only = OutputOptions {
            brdb: true, brz: false, stitched: true, individual: false, install_to_brickadia: false,
        };
        let individual_only = OutputOptions {
            brdb: true, brz: false, stitched: false, individual: true, install_to_brickadia: false,
        };
        let est_stitched = estimate_grid(&plan, &settings_with(stitched_only, ImagerySource::EsriWorldImagery), 0);
        // need_stream = peak_mesh + rasters (no write-peak); need_stitched adds the
        // brick vec. Set available so reserve + need_stream ≤ avail < reserve + need_stitched.
        let need_stream = est_stitched.peak_mesh_bytes + est_stitched.est_rasters_bytes;
        let avail = RAM_RESERVE_BYTES + need_stream + est_stitched.est_brick_vec_bytes / 2;
        let s = estimate_grid(&plan, &settings_with(stitched_only, ImagerySource::EsriWorldImagery), avail);
        let i = estimate_grid(&plan, &settings_with(individual_only, ImagerySource::EsriWorldImagery), avail);
        assert!(!s.fits_ram, "the write-peak brick vec must sink the stitched layout at this RAM");
        assert!(i.fits_ram, "individual-only streaming must clear the gate the stitched layout failed");
    }

    #[test]
    fn available_ram_bytes_parses_fixture() {
        // "MemAvailable:   12345678 kB" → 12345678 * 1024; missing/garbage → None.
        let fixture = "MemTotal:       65footle kB\nMemAvailable:   12345678 kB\nBuffers: 1 kB\n";
        assert_eq!(parse_mem_available_kb(fixture), Some(12_345_678));
        assert_eq!(
            available_from_text(fixture),
            Some(12_345_678u64 * 1024),
            "byte conversion must be value_kB * 1024",
        );
        assert_eq!(parse_mem_available_kb("MemTotal: 100 kB\nBuffers: 2 kB\n"), None);
        assert_eq!(parse_mem_available_kb("garbage with no colon line"), None);
        assert_eq!(parse_mem_available_kb("MemAvailable:   not_a_number kB"), None);
    }

    /// Mirror of `available_ram_bytes`'s byte conversion over an injected string
    /// (the real fn reads /proc/meminfo; this keeps the test hermetic).
    fn available_from_text(meminfo: &str) -> Option<u64> {
        parse_mem_available_kb(meminfo).map(|kb| kb.saturating_mul(1024))
    }

    #[test]
    fn est_tile_mesh_bytes_branches() {
        // imagery: cells^1.5 * 40 within 1%; flat: cells*64, and >100× smaller.
        let cells = 75_625u64;
        let imagery = est_tile_mesh_bytes(cells, true);
        let model = (cells as f64).powf(1.5) * 40.0;
        let rel = (imagery as f64 - model).abs() / model;
        assert!(rel < 0.01, "imagery model must match cells^1.5*40 within 1% (rel {rel})");
        let flat = est_tile_mesh_bytes(cells, false);
        assert_eq!(flat, cells * 64, "flat branch must be cells*64");
        assert!(imagery > flat * 100, "imagery mesh must be >100× the flat mesh");
    }

    // ── Output layer (real brdb/brz round-trips) ─────────────────────────────

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "h2brz-grid-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&d).expect("temp dir");
        d
    }

    fn brick_at(x: i32, y: i32, z: i32) -> brdb::Brick {
        brdb::Brick { position: brdb::Position::new(x, y, z), ..Default::default() }
    }

    /// Sum NumBricks across every chunk of grid 1 (the read-back brick count).
    fn read_brick_count(path: &std::path::Path) -> usize {
        use brdb::IntoReader;
        let db = brdb::Brdb::open(path).expect("open brdb").into_reader();
        db.brick_chunk_index(1)
            .expect("chunk index")
            .iter()
            .map(|c| c.num_bricks as usize)
            .sum()
    }

    /// Read every brick (absolute positions) from a `.brdb` grid 1.
    fn read_bricks_brdb(path: &std::path::Path) -> Vec<brdb::Brick> {
        use brdb::IntoReader;
        let db = brdb::Brdb::open(path).expect("open brdb").into_reader();
        let gd = db.global_data().expect("global data");
        let mut out = Vec::new();
        for chunk in db.brick_chunk_index(1).expect("chunk index") {
            let soa = db.brick_chunk_soa(1, chunk.index).expect("chunk soa");
            for b in soa.iter_bricks(chunk.index, gd.clone()) {
                out.push(b.expect("brick decode"));
            }
        }
        out
    }

    fn count_spawns(bricks: &[brdb::Brick]) -> usize {
        bricks
            .iter()
            .filter(|b| format!("{:?}", b.asset).contains("SpawnPoint"))
            .count()
    }

    #[test]
    #[ignore = "brdb 0.5 write_brdb/brick_chunk_index read-back asymmetry: write_brdb output \
                loads in Brickadia (verified in-game) but the 0.5 reader's brick_chunk_index \
                reports an empty NumBricks array on it. Diagnostic round-trip only — re-enable \
                when the brdb reader round-trips write_brdb output. brz round-trip still covers writes."]
    fn write_save_world_roundtrip_brdb() {
        let dir = tmp_dir("brdb-rt");
        let path = dir.join("rt.brdb");
        let world = bricks_to_save(vec![brick_at(0, 0, 6), brick_at(10, 0, 6), brick_at(20, 0, 6)]);
        write_save_world(&world, path.to_str().unwrap()).expect("write brdb");
        // 3 terrain bricks + 1 injected spawn.
        assert_eq!(read_brick_count(&path), 4, "3 bricks + 1 spawn must round-trip");
        let bricks = read_bricks_brdb(&path);
        assert_eq!(count_spawns(&bricks), 1, "exactly one spawn point");
        assert!(
            bricks.iter().any(|b| (b.position.x, b.position.y) == (20, 0)),
            "a known terrain position must survive the round-trip",
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_save_world_roundtrip_brz() {
        use brdb::IntoReader;
        let dir = tmp_dir("brz-rt");
        let path = dir.join("rt.brz");
        let world = bricks_to_save(vec![brdb::Brick {
            position: brdb::Position::new(5, 0, 6),
            color: (200, 100, 50).into(),
            ..Default::default()
        }]);
        write_save_world(&world, path.to_str().unwrap()).expect("write brz");
        let db = brdb::Brz::open(&path).expect("open brz").into_reader();
        // The single non-spawn terrain brick lives in the (0,0,0) chunk.
        let soa = db.brick_chunk_soa(1, (0, 0, 0).into()).expect("brz chunk soa");
        let gd = db.global_data().expect("brz global data");
        let bricks: Vec<brdb::Brick> = soa
            .iter_bricks((0, 0, 0).into(), gd)
            .map(|b| b.expect("brz brick"))
            .collect();
        // The injected spawn shares the single brick's XY, so filter it out and
        // match the terrain brick by its position AND non-spawn asset.
        let terrain = bricks
            .iter()
            .find(|b| (b.position.x, b.position.y) == (5, 0) && !format!("{:?}", b.asset).contains("SpawnPoint"))
            .expect("terrain brick at (5,0)");
        assert_eq!(
            (terrain.color.r, terrain.color.g, terrain.color.b),
            (200, 100, 50),
            "brz must preserve the brick color",
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[ignore = "brdb 0.5 read-back asymmetry (see write_save_world_roundtrip_brdb) — uses brdb \
                read-back; saves verified loadable in-game"]
    fn stale_brdb_delete_guard() {
        // Re-saving the SAME .brdb path through the delete-then-write guard must
        // NOT pile up revisions: on-disk SIZE must not grow across N re-saves
        // (a brick-count assertion proves nothing — the reader filters deleted).
        let dir = tmp_dir("stale");
        let builds = dir.clone();
        let world = bricks_to_save(vec![brick_at(0, 0, 6), brick_at(10, 0, 6)]);
        let opts = OutputOptions { brdb: true, brz: false, stitched: true, individual: false, install_to_brickadia: false };

        let mut sizes = Vec::new();
        for _ in 0..3 {
            let mut written = Vec::new();
            let mut installed = Vec::new();
            let mut warnings = Vec::new();
            write_tile_outputs(&world, &builds, "stale", opts, &mut written, &mut installed, &mut warnings)
                .expect("write");
            let path = builds.join("stale.brdb");
            sizes.push(std::fs::metadata(&path).expect("stat").len());
        }
        // The 2nd and 3rd writes hit the delete-then-write path; size must match
        // the first (a single-revision world), not grow with each re-save.
        assert_eq!(sizes[0], sizes[1], "re-save must not grow the .brdb (revision pile-up)");
        assert_eq!(sizes[1], sizes[2], "third re-save must also stay flat");
        // And the brick count is the clean single-revision count (2 + 1 spawn).
        assert_eq!(read_brick_count(&builds.join("stale.brdb")), 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn output_options_validate() {
        // Err on empty formats / empty layout; Ok on each non-empty combo.
        let empty_fmt = OutputOptions { brdb: false, brz: false, stitched: true, individual: false, install_to_brickadia: false };
        assert!(empty_fmt.validate().is_err(), "no format selected must be rejected");
        let empty_layout = OutputOptions { brdb: true, brz: false, stitched: false, individual: false, install_to_brickadia: false };
        assert!(empty_layout.validate().is_err(), "no layout selected must be rejected");
        for brdb in [false, true] {
            for brz in [false, true] {
                for stitched in [false, true] {
                    for individual in [false, true] {
                        let o = OutputOptions { brdb, brz, stitched, individual, install_to_brickadia: false };
                        let ok = (brdb || brz) && (stitched || individual);
                        assert_eq!(o.validate().is_ok(), ok, "validate mismatch for {o:?}");
                    }
                }
            }
        }
        // Default reproduces the legacy single-box output (brdb + stitched + install).
        let d = OutputOptions::default();
        assert!(d.brdb && d.stitched && d.install_to_brickadia && !d.brz && !d.individual);
        assert!(d.validate().is_ok());
    }

    #[test]
    fn naming_zero_padded_sorts() {
        // A 2×10 grid → _c00.._c09, and the suffixes sort lexicographically in
        // grid order (width fits the largest index).
        let rows = 2u32;
        let cols = 10u32;
        let mut suffixes: Vec<String> = (0..cols)
            .map(|c| tile_suffix(rows, cols, TileId { row: 0, col: c }))
            .collect();
        // One shared width = digits(max(rows,cols)) = 2 for a 2×10 grid (spec §7
        // `{row:0w}_{col:0w}`), so col runs _c00.._c09 and sorts lexicographically.
        assert_eq!(suffixes[0], "_r00_c00", "two-digit zero padding on both axes");
        assert_eq!(suffixes[9], "_r00_c09");
        let in_order = suffixes.clone();
        suffixes.sort();
        assert_eq!(suffixes, in_order, "zero-padded suffixes must sort in grid order");
        // The unpadded form would mis-sort (_c1 < _c9 < ... but _c10 < _c2);
        // confirm padding fixes that ordering across the ten columns.
        assert!(suffixes[9].ends_with("_c09"), "column 9 keeps its zero pad");
    }

    #[test]
    fn combined_exceeds_max_bricks_allowed() {
        // The COMBINED accumulator path must NOT reject a plan merely for passing
        // the single-box MAX_BRICKS (2 M) — exceeding it is the whole point.
        // MAX_GRID_BRICKS (50 M) is the real ceiling, far above 2 M.
        const {
            assert!(
                MAX_GRID_BRICKS > 2_000_000,
                "the combined cap must sit far above the single-box MAX_BRICKS",
            );
        }
        // estimate_grid must NOT raise over_brick_cap from the COMBINED total —
        // only a single over-budget tile trips it. A multi-tile plan whose SUM
        // exceeds 2 M but whose every tile is under MAX_DEM_CELLS stays clear.
        let plan = explicit_plan(4, 4, 1000.0);
        let settings = settings_with(OutputOptions::default(), ImagerySource::None);
        let est = estimate_grid(&plan, &settings, 256 * GB);
        let per_tile_max = plan
            .tiles
            .iter()
            .map(|t| approx_cell_count(t.bbox, plan.zoom))
            .max()
            .unwrap_or(0);
        assert!(
            per_tile_max <= MAX_DEM_CELLS,
            "fixture: every tile must be under the per-tile budget (max {per_tile_max})",
        );
        assert!(
            !est.over_brick_cap && !est.over_cell_budget,
            "a many-small-tiles plan must not be rejected by the per-tile gate",
        );
    }

    // ── Seam / offset parity over real meshed bricks ─────────────────────────

    /// Mesh a tiny synthetic raster directly (no network) at a given offset and
    /// per-raster-or-global min, returning the bricks. Mirrors the orchestrator's
    /// `build_one_tile` invariant: base_override=Some(0), the passed offset.
    fn mesh_raster(
        raster: DemRaster,
        global_min_m: f32,
        offset: (i32, i32),
        base_override: Option<u32>,
    ) -> Vec<brdb::Brick> {
        let request = BuildRequest {
            bbox: BBoxLatLon { north: 1.0, south: 0.0, west: 0.0, east: 1.0 },
            name: "seam".to_owned(),
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
            omit_below_m: 0.0,
            floor_level_m: 0.0,
        };
        build_one_tile(
            &request,
            raster,
            global_min_m,
            offset,
            base_override,
            Arc::new(|_, _| {}),
            Arc::new(AtomicBool::new(false)),
        )
        .expect("mesh raster")
    }

    #[test]
    fn seam_boundary_columns_equal_in_z() {
        // Two single-row rasters arranged so the SHARED edge column is the
        // HIGHEST cell in each tile (so the tile's top-Z reflects the edge, not a
        // taller interior column). The shared-edge cell is 50 m on both sides; the
        // OTHER column is lower. The tiles differ only in their per-raster min_m.
        //   left  = [10, 50]  → edge is col1 = 50 (the tile max), valley floor 10
        //   right = [50, 30]  → edge is col0 = 50 (the tile max), floor 30
        let left = DemRaster { width: 2, height: 1, heights_m: vec![10.0, 50.0], min_m: 10.0, max_m: 50.0 };
        let right = DemRaster { width: 2, height: 1, heights_m: vec![50.0, 30.0], min_m: 30.0, max_m: 50.0 };
        let global_min = 10.0_f32;

        // GLOBAL min + base 0 on both → the shared-edge cell (50 m) maps to the
        // SAME brick top-Z on each side (each tile's max-Z IS the edge column).
        let lb = mesh_raster(left.clone(), global_min, (0, 0), Some(0));
        let rb = mesh_raster(right.clone(), global_min, (0, 0), Some(0));
        let max_z = |bricks: &[brdb::Brick]| bricks.iter().map(|b| b.position.z).max().unwrap();
        assert_eq!(
            max_z(&lb),
            max_z(&rb),
            "with a global min the shared 50 m edge must reach the same top-Z",
        );

        // NEGATIVE CONTROL: per-raster min (each raster's own min_m) + base None
        // makes the SAME shared edge step — the left's 50 m sits high above its
        // valley floor (min 10), the right's 50 m sits at its own floor (min 50),
        // so their top-Z differ. This is the regression lock for the global datum.
        let lb_local = mesh_raster(left.clone(), left.min_m, (0, 0), None);
        let rb_local = mesh_raster(right.clone(), right.min_m, (0, 0), None);
        assert_ne!(
            max_z(&lb_local),
            max_z(&rb_local),
            "per-raster min must make the shared edge step (regression lock for the global datum)",
        );
    }

    #[test]
    #[ignore = "brdb 0.5 read-back asymmetry (see write_save_world_roundtrip_brdb) — uses brdb \
                read-back; offset logic also covered by generate.rs offset tests"]
    fn keep_individual_offset_parity() {
        // A 1×2 grid written individual+pre-offset: reopen r0c0 and r0c1, assert
        // c1's min-X is exactly c0.min_x + (cells_w0 * 2 * size) — the same world
        // offset the stitched path bakes in (spec §7 keep-individual parity).
        let dir = tmp_dir("parity");
        let builds = dir.clone();
        let size = 5u16; // horizontal_scale(1) * 5 (non-micro).

        // Two abutting tiles, each a 2×1 raster, offsets a world apart.
        let r0 = DemRaster { width: 2, height: 1, heights_m: vec![0.0, 10.0], min_m: 0.0, max_m: 10.0 };
        let r1 = DemRaster { width: 2, height: 1, heights_m: vec![10.0, 20.0], min_m: 0.0, max_m: 20.0 };
        let off0 = (0, 0);
        let off1 = (i32::from(size) * 2 * 2, 0); // cells_w0(2) * 2 * size.

        let b0 = mesh_raster(r0, 0.0, off0, Some(0));
        let b1 = mesh_raster(r1, 0.0, off1, Some(0));
        let opts = OutputOptions { brdb: true, brz: false, stitched: false, individual: true, install_to_brickadia: false };
        let mut w = Vec::new();
        let mut ins = Vec::new();
        let mut warn = Vec::new();
        write_tile_outputs(&bricks_to_save(b0), &builds, "tile_r0_c0", opts, &mut w, &mut ins, &mut warn).unwrap();
        write_tile_outputs(&bricks_to_save(b1), &builds, "tile_r0_c1", opts, &mut w, &mut ins, &mut warn).unwrap();

        let min_x = |path: &std::path::Path| {
            read_bricks_brdb(path)
                .iter()
                .filter(|b| !format!("{:?}", b.asset).contains("SpawnPoint"))
                .map(|b| b.position.x)
                .min()
                .unwrap()
        };
        let c0_min = min_x(&builds.join("tile_r0_c0.brdb"));
        let c1_min = min_x(&builds.join("tile_r0_c1.brdb"));
        assert_eq!(
            c1_min - c0_min,
            i32::from(size) * 2 * 2,
            "c1.min_x must equal c0.min_x + a full tile's world width (pre-offset parity)",
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[ignore = "brdb 0.5 read-back asymmetry (see write_save_world_roundtrip_brdb) — uses brdb \
                read-back; saves verified loadable in-game"]
    fn combined_one_spawn_many_individual() {
        // Stitched output has exactly ONE spawn over the union; the individual
        // files have one spawn EACH; and every stitched tile-B brick has x ≥ the
        // tile-A width (the offset placed it past A).
        let dir = tmp_dir("spawn");
        let builds = dir.clone();
        let size = 5u16;
        let a = mesh_raster(
            DemRaster { width: 2, height: 1, heights_m: vec![0.0, 5.0], min_m: 0.0, max_m: 5.0 },
            0.0,
            (0, 0),
            Some(0),
        );
        let b = mesh_raster(
            DemRaster { width: 2, height: 1, heights_m: vec![5.0, 10.0], min_m: 0.0, max_m: 10.0 },
            0.0,
            (i32::from(size) * 2 * 2, 0),
            Some(0),
        );
        // Stitched: extend ONE Vec, bricks_to_save ONCE.
        let mut combined = a.clone();
        combined.extend(b.clone());
        let stitched_world = bricks_to_save(combined);
        let stitched_path = builds.join("stitched.brdb");
        write_save_world(&stitched_world, stitched_path.to_str().unwrap()).unwrap();
        let stitched_bricks = read_bricks_brdb(&stitched_path);
        assert_eq!(count_spawns(&stitched_bricks), 1, "stitched must have exactly one spawn");

        // Individual: one spawn per tile.
        let opts = OutputOptions { brdb: true, brz: false, stitched: false, individual: true, install_to_brickadia: false };
        let mut w = Vec::new();
        let mut ins = Vec::new();
        let mut warn = Vec::new();
        write_tile_outputs(&bricks_to_save(a.clone()), &builds, "ind_a", opts, &mut w, &mut ins, &mut warn).unwrap();
        write_tile_outputs(&bricks_to_save(b.clone()), &builds, "ind_b", opts, &mut w, &mut ins, &mut warn).unwrap();
        assert_eq!(count_spawns(&read_bricks_brdb(&builds.join("ind_a.brdb"))), 1);
        assert_eq!(count_spawns(&read_bricks_brdb(&builds.join("ind_b.brdb"))), 1);

        // Every tile-B brick sits at x ≥ the A-tile's world width (offset placed
        // it strictly past A). A's max non-spawn x:
        let a_world = i32::from(size) * 2 * 2;
        for brick in &b {
            assert!(
                brick.position.x >= a_world - i32::from(size) * 2,
                "tile-B brick x={} must be in the offset (≥ ~A width)",
                brick.position.x,
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
