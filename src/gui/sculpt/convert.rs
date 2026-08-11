//! The Sculpt → Convert seam: turn an edited [`HeightField`] into Brickadia
//! bricks through the EXISTING converter pipeline, unchanged.
//!
//! `convert_heightfield` mirrors the single-box `run_build` tail: it derives the
//! vertical scale from the field's [`FieldMeta`] exactly as the Map tab does
//! (`derive_scale`), normalizes against the floor (`global_min_m = 0.0`, the
//! field is already floor-relative), meshes with `generate_bricks_skip_floor`
//! (so flat columns reveal the native ground), then writes `.brdb` → Worlds/ and
//! optionally `.brz` → Prefabs/ and installs them.
//!
//! `skip_floor` is the ONLY behavioural difference from a direct map build: with
//! it `false` and the same options, a `from_dem` field with no edits is
//! byte-identical to a Map build of the same area (the passthrough identity).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::gui::build::{
    self, BlockType, BrickStyle, BuildError, BuildOutcome, BuildStage, DemRaster, MAX_GRID_BRICKS,
    ProgressFn, build_heightmap, builds_dir, enforce_cell_budget, generate_bricks_skip_floor,
    install_save, sanitize_name,
};
use crate::gui::grid::{offset_fits_chunk_index, tile_world_offset};
use crate::gui::map_tab::derive_scale;
use crate::gui::zones::{self, Zone};
use crate::map::Colormap;
use crate::util::{bricks_to_save, write_save_world};

use super::heightfield::{terrace_height, HeightField};
use super::layers::LayerStack;
use super::paint::{PaintColormap, PaintLayer};

/// What the sculpt convert writes and where. Mirrors the single-box output
/// model: `.brdb` → Brickadia Worlds/, `.brz` → Prefabs/. At least one format is
/// expected; with neither set the convert errors rather than writing nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OutputOptions {
    /// Write a `.brdb` world (installs into Brickadia's `Worlds/`).
    pub brdb: bool,
    /// Write a `.brz` prefab (installs into Brickadia's `Prefabs/`).
    pub brz: bool,
    /// Copy the written saves into Brickadia's Saved tree.
    pub install_to_brickadia: bool,
    /// Overwrite `<name>.<ext>` in place instead of suffixing `-2`, `-3`, … .
    pub overwrite: bool,
    /// Reveal the native Brickadia floor under flat (zero-height) columns. The
    /// sculpt/blank-canvas convert sets this `true`; a passthrough-identity
    /// build sets it `false` to stay byte-identical to a direct map build.
    pub skip_floor: bool,
}

impl Default for OutputOptions {
    fn default() -> Self {
        // Sculpt's natural output: a world that reveals the native floor under
        // flat areas, installed for immediate in-game load.
        Self {
            brdb: true,
            brz: false,
            install_to_brickadia: true,
            overwrite: false,
            skip_floor: true,
        }
    }
}

/// Convert an edited [`HeightField`] into Brickadia bricks and write/install the
/// selected saves. Reuses the existing `build_heightmap` → `generate_bricks` →
/// write/install pipeline unchanged; the only sculpt-specific knob is
/// `out.skip_floor` (forwarded to `generate_bricks_skip_floor`).
///
/// The vertical scale is derived from the field's metadata exactly as the Map
/// tab derives it at fetch time (`derive_scale` against `cell_m`,
/// `studs_per_meter`, `vertical_exaggeration`, `micro`), so a faithful 1:1 build
/// of a sculpted field matches a Map build of the same resolution.
///
/// `floor_level_m` raises the base plane the terrain fills DOWN to: it maps to
/// `base_override = Some(round(floor_level_m * vertical_scale))` in brick-Z. The
/// default `0.0` keeps `base_override = Some(0)`, byte-identical to today.
///
/// `omit_below_m` is the meter-space omit level: a column whose SOURCE height
/// (m) is at or below it emits no bricks (native floor / "omit water"). It is
/// converted to a brick-Z threshold `h_omit = round(omit_below_m *
/// vertical_scale)` and forwarded to the skip predicate (`skip_floor && h <=
/// h_omit`). The decision is meter-space — made BEFORE quantization picks a
/// scale — so a near-floor cell that maps to `h >= 1` at a proper scale
/// survives (the gap fix). The default `0.0` drops only true-floor columns.
#[allow(clippy::too_many_arguments)]
pub(crate) fn convert_heightfield(
    field: &HeightField,
    out: OutputOptions,
    floor_level_m: f32,
    omit_below_m: f32,
    zones: &[Zone],
    paint: Option<PaintLayer>,
    terrace_step_m: Option<f32>,
    max_brick_units: u16,
    progress: ProgressFn,
    cancel: Arc<AtomicBool>,
) -> Result<BuildOutcome, BuildError> {
    if !out.brdb && !out.brz {
        return Err(BuildError::BrdbWrite(
            "select at least one output format (.brdb or .brz)".to_owned(),
        ));
    }

    let mut raster: DemRaster = field.to_dem_raster();
    // Terrace (stepped) export: snap every source height to a step multiple
    // before quantization, so the build is discrete plateaus instead of smooth
    // relief. None = smooth (untouched, byte-identical to before).
    if let Some(step) = terrace_step_m {
        for m in &mut raster.heights_m {
            *m = terrace_height(*m, step);
        }
    }
    let dem_width = raster.width;
    let dem_height = raster.height;
    // Report the field's true cell range (floor-relative meters), via the SAME
    // source the tiled path uses — `raster.min_m` is the post-normalization floor
    // (≈0), which would make the reported minimum differ by export mode alone.
    let (elevation_min_m, elevation_max_m) = field.min_max();

    // Derive the integer horizontal brick scale + 1:1-matched vertical scale from
    // the field's metadata, mirroring the Map tab's start_fetch path. cell_m is
    // already the effective (post-density) pitch the field was authored at.
    let (horizontal_scale, vertical_scale) = derive_scale(
        field.meta.cell_m,
        field.meta.studs_per_meter,
        field.meta.vertical_exaggeration,
        field.meta.micro,
    );

    let block_type = if field.meta.micro { BlockType::Micro } else { BlockType::SmoothTile };
    let style = BrickStyle::new(block_type, horizontal_scale, false, false);

    // Center the build on the origin exactly as run_build does: offset =
    // -(width*size), -(height*size), in the SAME units the mesher emits.
    let size = i32::from(build::cell_size_units(horizontal_scale, field.meta.micro));
    let offset = (-(dem_width as i32 * size), -(dem_height as i32 * size));

    // Floor-relative normalization: the field's cells are already meters above
    // the floor, so global_min_m = 0.0 passes them straight through
    // build_heightmap (the same algebra a direct map build applies via
    // m - raster.min_m). The base plane every non-floor column fills DOWN to is
    // brick-Z `base_h`, derived meter-space from floor_level_m; default 0.0 keeps
    // base_h = 0 (today's behavior).
    let heightmap = build_heightmap(&raster, vertical_scale, 0.0);
    let base_h = (floor_level_m * vertical_scale).max(0.0).round() as u32;
    // Meter-space omit threshold: convert omit_below_m to a brick-Z height so the
    // skip decision is made against the SOURCE meters, not a scale artifact.
    // Default 0.0 → h_omit = 0, dropping only true-floor columns.
    let h_omit = (omit_below_m * vertical_scale).max(0.0).round() as u32;

    progress(BuildStage::GeneratingBricks, 0.0);
    let flat = build::FlatColormap::sculpt_default(dem_width, dem_height);
    // Painted cells color the bricks; an unpainted (blank) grid stays on the flat
    // colormap so the output is byte-identical to a no-paint build (same discipline
    // as the empty-zones keep-mask below).
    let paint_cm;
    let colormap: &dyn Colormap = match paint {
        Some(p) if !p.grid.is_blank() => {
            paint_cm = PaintColormap::new(&p.grid.cells, p.palette, dem_width, dem_height);
            &paint_cm
        }
        _ => &flat,
    };
    // Freedraw zones → an XY keep-mask at the mesher's grid dims. Empty zones →
    // `None`, byte-identical to a no-zone convert (no allocation, no per-cell
    // work). Zones are NOT consumed/cleared — the caller keeps them for re-export.
    let keep_mask = (!zones.is_empty()).then(|| zones::rasterize(zones, dem_width, dem_height));
    let bricks = generate_bricks_skip_floor(
        &heightmap,
        colormap,
        style,
        Some(base_h),
        offset,
        out.skip_floor,
        h_omit,
        max_brick_units,
        Arc::clone(&progress),
        Arc::clone(&cancel),
        keep_mask.as_deref(),
    )?;
    let brick_count = bricks.len();

    progress(BuildStage::WritingSave, 0.0);
    let world = bricks_to_save(bricks);
    let written = write_and_install(&world, &field.meta.source_name, out)?;
    progress(BuildStage::WritingSave, 1.0);

    // The BuildOutcome's brdb_path field is the headline path the UI shows; when
    // only .brz was requested, surface that instead so "wrote → <path>" is true.
    let primary_path = written
        .brdb_path
        .or_else(|| written.extra_paths.into_iter().next())
        .unwrap_or_default();

    Ok(BuildOutcome {
        brdb_path: primary_path,
        installed_path: written.installed_path,
        install_warning: written.install_warning,
        brick_count,
        dem_width,
        dem_height,
        elevation_min_m,
        elevation_max_m,
    })
}

/// Non-overlapping tile boundaries along ONE axis: `extent` cells split into
/// `ceil(extent / tile_cells)` half-open ranges `[start, end)` that PARTITION
/// the axis — adjacent ranges abut (`end_i == start_{i+1}`) with NO shared cell.
/// Each cell belongs to exactly one tile; the per-tile world offset places every
/// absolute cell at the same integer world slot (`2*size*c + center`), so a
/// tile's last column and its neighbor's first column sit one per-cell pitch
/// (`2*size`) apart — exactly as two adjacent cells in a single mesh. The seam is
/// therefore watertight by integer identity: no gap AND no overlap. A single
/// range (`tile_cells >= extent`) returns one `[0, extent)` covering the whole
/// axis — the no-split path that stitches byte-identically to a single mesh.
///
/// (An earlier variant overlapped ONE shared cell per seam — an `end` of
/// `start + step + 1` — which double-meshed every seam column into two
/// exactly-coincident bricks. Coincident solid bricks Z-fight in Brickadia and
/// render as the seam lines users saw on big/tiled exports. The partition
/// removes them; the offset algebra already made the shared cell unnecessary.)
///
/// Each returned range's WIDTH is `<= tile_cells`, so a tile never exceeds the
/// per-tile cell budget the caller checks. `extent`/`tile_cells` clamp to `>= 1`.
fn tile_bounds(extent: u32, tile_cells: u32) -> Vec<(u32, u32)> {
    let extent = extent.max(1);
    let step = tile_cells.max(1);
    if step >= extent {
        return vec![(0, extent)];
    }
    let mut out = Vec::new();
    let mut start = 0u32;
    // Bounded by ceil(extent/step) iterations; `start` advances by `step` each
    // pass until a range reaches the far edge. Non-overlapping: the next tile
    // begins exactly where this one ends (`start = end`), so each cell is meshed
    // once. The final tile clamps to `extent`.
    while start < extent {
        let end = (start + step).min(extent);
        out.push((start, end));
        start = end;
    }
    out
}

/// Convert an edited [`HeightField`] into ONE stitched save by meshing it as a
/// grid of partition sub-fields (spec §5, the manual "Tile this export" path).
/// The field is split into `ceil(w / tile_cells) × ceil(h / tile_cells)`
/// sub-fields by [`tile_bounds`] (a non-overlapping partition — adjacent
/// sub-fields abut at their exact edge, no shared/duplicated cell).
///
/// Each sub-field meshes through the EXACT same pipeline as `convert_heightfield`
/// (uniform `size` from one [`derive_scale`], `base_override` from `floor_level_m`,
/// `global_min = 0`, the meter-space `omit_below_m`, the same scale/micro), with a
/// per-tile world offset from cumulative cells + global centering via
/// [`tile_world_offset`] — the SAME algebra `grid.rs::world_offset` uses. Every
/// absolute cell lands on the identical integer world slot regardless of its
/// tile, so adjacent tiles abut at the per-cell pitch — watertight, no gap and
/// no overlap.
///
/// Bricks accumulate into ONE `Vec<Brick>` and `bricks_to_save` runs ONCE → one
/// stitched `.brdb`/`.brz`. The aggregate cap is `MAX_GRID_BRICKS` (checked before
/// each tile's bricks are folded in); each tile passes `enforce_cell_budget`. The
/// signature mirrors `convert_heightfield` so the UI worker calls it identically.
#[allow(clippy::too_many_arguments)]
pub(crate) fn convert_heightfield_tiled(
    field: &HeightField,
    out: OutputOptions,
    tile_cells: u32,
    floor_level_m: f32,
    omit_below_m: f32,
    zones: &[Zone],
    paint: Option<PaintLayer>,
    terrace_step_m: Option<f32>,
    max_brick_units: u16,
    progress: ProgressFn,
    cancel: Arc<AtomicBool>,
) -> Result<BuildOutcome, BuildError> {
    if !out.brdb && !out.brz {
        return Err(BuildError::BrdbWrite(
            "select at least one output format (.brdb or .brz)".to_owned(),
        ));
    }

    let dem_width = field.width;
    let dem_height = field.height;
    let (elevation_min_m, elevation_max_m) = field.min_max();

    // ONE derive_scale for the whole field (pillar A / uniform pitch): every
    // sub-field meshes at the identical horizontal+vertical scale, so the seams
    // abut. Mirrors convert_heightfield's single-mesh derivation exactly.
    let (horizontal_scale, vertical_scale) = derive_scale(
        field.meta.cell_m,
        field.meta.studs_per_meter,
        field.meta.vertical_exaggeration,
        field.meta.micro,
    );
    let block_type = if field.meta.micro { BlockType::Micro } else { BlockType::SmoothTile };
    let style = BrickStyle::new(block_type, horizontal_scale, false, false);
    let size = build::cell_size_units(horizontal_scale, field.meta.micro);

    // Meter-space floor/omit thresholds, identical to convert_heightfield.
    let base_h = (floor_level_m * vertical_scale).max(0.0).round() as u32;
    let h_omit = (omit_below_m * vertical_scale).max(0.0).round() as u32;

    // Shared-edge boundaries on each axis. global_cells_{w,h} is the field's full
    // extent in cells (dem_width/dem_height) — the centering datum every tile
    // shares so the mosaic is centered on the origin exactly as the single mesh.
    let x_bounds = tile_bounds(dem_width, tile_cells);
    let y_bounds = tile_bounds(dem_height, tile_cells);
    let tile_total = (x_bounds.len() * y_bounds.len()) as f32;

    // Rasterize zones against the FULL field ONCE; each tile slices its window out
    // of this mask so a zone spanning a seam stays consistent across tiles. Empty
    // zones → `None` (every tile meshes unmasked, byte-identical to today).
    let full_mask = (!zones.is_empty()).then(|| zones::rasterize(zones, dem_width, dem_height));
    // Active paint layer (None if absent or unpainted → every tile stays on the
    // flat colormap, byte-identical to today). Each tile slices its window out of
    // the full grid, mirroring the zone keep-mask slicing.
    let painting = paint.filter(|p| !p.grid.is_blank());

    let mut combined: Vec<brdb::Brick> = Vec::new();
    // Pre-reserve to the summed sub-field cell-count ceiling (greedy meshing only
    // reduces brick count, so cell count is a safe upper bound) so the per-tile
    // `extend` never capacity-doubles — that doubling would transiently hold the
    // old + new contiguous Brick arrays. Mirrors grid.rs's stitched-accumulator
    // reserve. Capped at MAX_GRID_BRICKS so a conservative over-estimate can't
    // over-reserve past the hard ceiling the loop already enforces.
    let est_cells: u64 = y_bounds
        .iter()
        .flat_map(|&(y0, y1)| {
            x_bounds
                .iter()
                .map(move |&(x0, x1)| u64::from(x1 - x0).saturating_mul(u64::from(y1 - y0)))
        })
        .fold(0u64, u64::saturating_add)
        .min(MAX_GRID_BRICKS as u64);
    combined.reserve_exact(est_cells as usize);
    let mut tile_idx = 0usize;
    progress(BuildStage::GeneratingBricks, 0.0);
    for &(y0, y1) in &y_bounds {
        for &(x0, x1) in &x_bounds {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(BuildError::Cancelled);
            }
            // Per-tile mesh-input budget (defense in depth): a sub-field is at most
            // (tile_cells+1)² cells, so a sane tile size never trips it, but a
            // pathological huge tile_cells on a huge canvas would — reject here
            // rather than OOM the mesher.
            enforce_cell_budget(x1 - x0, y1 - y0)?;

            let sub = field.sub_field(x0, y0, x1, y1);
            let mut raster = sub.to_dem_raster();
            if let Some(step) = terrace_step_m {
                for m in &mut raster.heights_m {
                    *m = terrace_height(*m, step);
                }
            }
            // Slice this tile's window out of the full-field mask (row-major), so
            // the per-cell keep decision is identical to the single-mesh path.
            let tile_mask = full_mask.as_ref().map(|full| {
                let mut m = Vec::with_capacity((sub.width * sub.height) as usize);
                for ty in 0..sub.height {
                    for tx in 0..sub.width {
                        m.push(full[((y0 + ty) * dem_width + (x0 + tx)) as usize]);
                    }
                }
                m
            });
            // Per-tile world offset from cumulative cells (the tile's NW cell index
            // is its absolute start) + the global-centering term over the FULL
            // field extent — the same algebra grid.rs::world_offset uses.
            let offset = tile_world_offset(x0, y0, dem_width, dem_height, size);
            let heightmap = build_heightmap(&raster, vertical_scale, 0.0);
            // The colormap must match THIS sub-field's dimensions (the mesher
            // asserts heightmap and colormap share dims), not the full field's.
            let flat = build::FlatColormap::sculpt_default(sub.width, sub.height);
            // Slice this tile's paint window (row-major) so a painted region
            // spanning a seam stays consistent across tiles.
            let tile_paint = painting.map(|p| p.grid.sub_window(x0, y0, x1, y1));
            let paint_cm;
            let colormap: &dyn Colormap = if let (Some(cells), Some(p)) = (&tile_paint, &painting) {
                paint_cm = PaintColormap::new(cells, p.palette, sub.width, sub.height);
                &paint_cm
            } else {
                &flat
            };

            // A no-op per-tile progress sink: the aggregate bar advances per tile
            // below; a per-tile stage fraction would thrash the readout.
            let tile_progress: ProgressFn = Arc::new(|_, _| {});
            let bricks = generate_bricks_skip_floor(
                &heightmap,
                colormap,
                style,
                Some(base_h),
                offset,
                out.skip_floor,
                h_omit,
                max_brick_units,
                tile_progress,
                Arc::clone(&cancel),
                tile_mask.as_deref(),
            )?;

            // Aggregate cap BEFORE folding in (mirrors run_grid_build correction
            // #2): an over-limit accumulator must never be allocated past the hard
            // ceiling. Bail with the count it WOULD reach.
            let would_be = combined.len() + bricks.len();
            if would_be > MAX_GRID_BRICKS {
                return Err(BuildError::TooManyBricks { count: would_be, max: MAX_GRID_BRICKS });
            }
            combined.extend(bricks);

            tile_idx += 1;
            progress(BuildStage::GeneratingBricks, tile_idx as f32 / tile_total.max(1.0));
        }
    }
    let brick_count = combined.len();

    progress(BuildStage::WritingSave, 0.0);
    let world = bricks_to_save(combined);
    let written = write_and_install(&world, &field.meta.source_name, out)?;
    progress(BuildStage::WritingSave, 1.0);

    let primary_path = written
        .brdb_path
        .or_else(|| written.extra_paths.into_iter().next())
        .unwrap_or_default();

    Ok(BuildOutcome {
        brdb_path: primary_path,
        installed_path: written.installed_path,
        install_warning: written.install_warning,
        brick_count,
        dem_width,
        dem_height,
        elevation_min_m,
        elevation_max_m,
    })
}

// ── Layer export: one pre-positioned save per layer (MVP, single resolution) ──
//
// Each visible, non-empty layer exports as its OWN save whose bricks sit at TRUE
// absolute world coordinates on the shared base lattice, so loading every part in
// Brickadia auto-assembles one world. `LayerStack::claim` makes the parts a strict
// partition (top layer wins), so no two parts place a brick in the same cell —
// watertight, no coincident bricks across parts. The pitch is held UNIFORM (the
// single-resolution MVP), so this reuses the exact integer-offset alignment that
// fixed the tiling seams; the per-layer integer resolution multiplier is Phase 1.

/// One planned export part: a layer's claimed-cell bounding box, the keep-mask
/// over that box, and the world offset that lands every cell on the shared global
/// lattice. Field-cell space; `bbox` is half-open `[x0,x1) × [y0,y1)`.
pub(crate) struct PartPlan {
    pub layer_name: String,
    pub bbox: (u32, u32, u32, u32),
    /// Row-major keep-mask over the bbox (len `(x1-x0)·(y1-y0)`): a cell is kept
    /// iff THIS layer owns it after the claim pass.
    pub keep_local: Vec<bool>,
    pub offset: (i32, i32),
}

/// Plan the export parts for a layer stack over `field`, on a shared base pitch
/// `size_base`. Runs the claim pass, derives each visible non-empty layer's
/// owned-cell bbox + local keep-mask + world offset, and hard-gates every part
/// against Brickadia's world-extent limit (a promoted-from-debug check: a
/// pre-positioned part that exceeded it would be silently clamped off-lattice).
/// Empty/hidden layers produce no part.
pub(crate) fn plan_layer_parts(
    stack: &LayerStack,
    field: &HeightField,
    size_base: u16,
) -> Result<Vec<PartPlan>, BuildError> {
    let (fw, fh) = (field.width, field.height);
    let claimed = stack.claim(fw, fh);
    let mut parts = Vec::new();
    for layer in &stack.layers {
        if !layer.visible {
            continue;
        }
        // A layer's selection mask must match the CURRENT field (set_field resets
        // the stack on a field swap). Catch a desync loudly rather than silently
        // producing a truncated partition.
        debug_assert_eq!(
            layer.box_mask.len(),
            (fw as usize) * (fh as usize),
            "layer '{}' box_mask ({}) must match field dims {fw}×{fh}",
            layer.name,
            layer.box_mask.len(),
        );
        // Bounding box of the cells this layer OWNS post-claim (not merely selects).
        let (mut x0, mut y0, mut x1, mut y1) = (fw, fh, 0u32, 0u32);
        let mut any = false;
        for y in 0..fh {
            for x in 0..fw {
                if claimed[(y * fw + x) as usize] == Some(layer.id) {
                    any = true;
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x + 1);
                    y1 = y1.max(y + 1);
                }
            }
        }
        if !any {
            continue;
        }
        let (bw, bh) = (x1 - x0, y1 - y0);
        let mut keep_local = Vec::with_capacity((bw * bh) as usize);
        for y in y0..y1 {
            for x in x0..x1 {
                keep_local.push(claimed[(y * fw + x) as usize] == Some(layer.id));
            }
        }
        // World-extent gate (spec §5.6): a pre-positioned part is far likelier to
        // exceed the i16 ChunkIndex bound than a single centered field. Check the
        // would-be offset against the bound BEFORE `tile_world_offset` (whose
        // debug-assert would mask the release path) and refuse rather than clamp.
        if !offset_fits_chunk_index(part_axis_offset(x0, fw, size_base), fw, size_base)
            || !offset_fits_chunk_index(part_axis_offset(y0, fh, size_base), fh, size_base)
        {
            return Err(BuildError::BrdbWrite(format!(
                "layer '{}' places a part outside Brickadia's world extent — coarsen \
                 background detail or shrink the world",
                layer.name,
            )));
        }
        let offset = tile_world_offset(x0, y0, fw, fh, size_base);
        parts.push(PartPlan {
            layer_name: layer.name.clone(),
            bbox: (x0, y0, x1, y1),
            keep_local,
            offset,
        });
    }
    Ok(parts)
}

/// The per-axis world offset of a part whose NW cell index is `nw_cells`, on a
/// field `global_cells` wide at pitch `size`. Mirrors `tile_world_offset`'s axis
/// formula (`2·size·nw − global_cells·size`) in i64 so the world-extent gate can
/// be evaluated WITHOUT tripping `tile_world_offset`'s debug-assert, then saturate
/// to i32 (the gate rejects anything that would actually saturate).
fn part_axis_offset(nw_cells: u32, global_cells: u32, size: u16) -> i32 {
    let off = i64::from(nw_cells) * 2 * i64::from(size) - i64::from(global_cells) * i64::from(size);
    off.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// Mesh one planned part into bricks at its absolute world offset. Meshes the
/// bbox sub-field with the part's keep-mask, so only the layer's owned cells emit
/// — every emitted cell lands at the same absolute world slot a single full-field
/// mesh would place it (shared pitch + shared global offset).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_part_bricks(
    plan: &PartPlan,
    field: &HeightField,
    style: BrickStyle,
    vertical_scale: f32,
    base_h: u32,
    h_omit: u32,
    skip_floor: bool,
    max_brick_units: u16,
    progress: ProgressFn,
    cancel: Arc<AtomicBool>,
) -> Result<Vec<brdb::Brick>, BuildError> {
    let (x0, y0, x1, y1) = plan.bbox;
    let sub = field.sub_field(x0, y0, x1, y1);
    let raster = sub.to_dem_raster();
    let heightmap = build_heightmap(&raster, vertical_scale, 0.0);
    let flat = build::FlatColormap::sculpt_default(sub.width, sub.height);
    generate_bricks_skip_floor(
        &heightmap,
        &flat,
        style,
        Some(base_h),
        plan.offset,
        skip_floor,
        h_omit,
        max_brick_units,
        progress,
        cancel,
        Some(&plan.keep_local),
    )
}

/// What one exported part wrote: the source layer, its brick count, and the
/// primary save path (+ install destination / non-fatal warning).
pub(crate) struct PartOutcome {
    pub layer_name: String,
    pub brick_count: usize,
    pub primary_path: PathBuf,
    pub installed_path: Option<PathBuf>,
    pub install_warning: Option<String>,
}

/// Export every visible, non-empty layer as its own pre-positioned save. ONE
/// shared `derive_scale` over the field's meta drives every part (uniform pitch =
/// watertight overlay). Each part writes via the SAME `write_and_install` the
/// single/tiled paths use, stem `{source}_{layer}` (disambiguated on collision).
///
/// MVP scope: parts are GEOMETRY-ONLY — the per-layer paint / freedraw-zone /
/// terrace knobs the single-export path applies are NOT yet plumbed per part
/// (Phase 5). The Layers panel therefore exports terrain shape + the global
/// floor/omit only; paint & terrace stay on the single Convert path until then.
#[allow(clippy::too_many_arguments)]
pub(crate) fn export_layer_parts(
    stack: &LayerStack,
    field: &HeightField,
    out: OutputOptions,
    floor_level_m: f32,
    omit_below_m: f32,
    max_brick_units: u16,
    progress: ProgressFn,
    cancel: Arc<AtomicBool>,
) -> Result<Vec<PartOutcome>, BuildError> {
    if !out.brdb && !out.brz {
        return Err(BuildError::BrdbWrite(
            "select at least one output format (.brdb or .brz)".to_owned(),
        ));
    }
    let (horizontal_scale, vertical_scale) = derive_scale(
        field.meta.cell_m,
        field.meta.studs_per_meter,
        field.meta.vertical_exaggeration,
        field.meta.micro,
    );
    let block_type = if field.meta.micro { BlockType::Micro } else { BlockType::SmoothTile };
    let style = BrickStyle::new(block_type, horizontal_scale, false, false);
    let size_base = build::cell_size_units(horizontal_scale, field.meta.micro);
    let base_h = (floor_level_m * vertical_scale).max(0.0).round() as u32;
    let h_omit = (omit_below_m * vertical_scale).max(0.0).round() as u32;

    let plans = plan_layer_parts(stack, field, size_base)?;
    if plans.is_empty() {
        return Err(BuildError::BrdbWrite(
            "no layer has a selection to export — pick some boxes first".to_owned(),
        ));
    }

    let mut outcomes = Vec::with_capacity(plans.len());
    let mut seen_stems = std::collections::HashSet::new();
    let total = plans.len() as f32;
    for (i, plan) in plans.iter().enumerate() {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(BuildError::Cancelled);
        }
        progress(BuildStage::GeneratingBricks, i as f32 / total.max(1.0));
        let tile_progress: ProgressFn = Arc::new(|_, _| {});
        let bricks = build_part_bricks(
            plan,
            field,
            style,
            vertical_scale,
            base_h,
            h_omit,
            out.skip_floor,
            max_brick_units,
            tile_progress,
            Arc::clone(&cancel),
        )?;
        let brick_count = bricks.len();
        // An all-floor / fully-omitted part meshes to nothing — skip it rather than
        // write a junk empty save (mirrors the "empty layer won't export" rule).
        if brick_count == 0 {
            continue;
        }
        if brick_count > MAX_GRID_BRICKS {
            return Err(BuildError::TooManyBricks { count: brick_count, max: MAX_GRID_BRICKS });
        }
        let world = bricks_to_save(bricks);
        // Stem `{source}_{layer}`, disambiguated by index if two layers sanitize to
        // the same name (e.g. after a Phase-3 rename) so one part can't clobber another.
        let mut stem = format!("{}_{}", field.meta.source_name, plan.layer_name);
        if !seen_stems.insert(sanitize_name(&stem)) {
            stem = format!("{}_{}_{}", field.meta.source_name, i + 1, plan.layer_name);
            seen_stems.insert(sanitize_name(&stem));
        }
        let written = write_and_install(&world, &stem, out)?;
        let primary_path = written
            .brdb_path
            .or_else(|| written.extra_paths.into_iter().next())
            .unwrap_or_default();
        outcomes.push(PartOutcome {
            layer_name: plan.layer_name.clone(),
            brick_count,
            primary_path,
            installed_path: written.installed_path,
            install_warning: written.install_warning,
        });
    }
    progress(BuildStage::WritingSave, 1.0);
    Ok(outcomes)
}

/// Result of staging + installing the selected formats: the `.brdb` path (if
/// written), any additional staged paths (e.g. `.brz`), the first installed
/// destination, and a non-fatal warning if an install was skipped.
struct WriteResult {
    brdb_path: Option<PathBuf>,
    extra_paths: Vec<PathBuf>,
    installed_path: Option<PathBuf>,
    install_warning: Option<String>,
}

/// Write each selected format to the staging `builds_dir`, then (if requested)
/// install it into Brickadia's Saved tree. Returns the `.brdb` staging path (if
/// written), any additional staging paths (e.g. `.brz`), the first installed
/// destination, and a non-fatal install warning if an install was skipped.
///
/// Install failure is non-fatal — the save is already on disk, so the convert
/// degrades to "wrote but did not install" exactly as the single-box path does.
fn write_and_install(
    world: &brdb::World,
    name: &str,
    out: OutputOptions,
) -> Result<WriteResult, BuildError> {
    let dir = builds_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| BuildError::Io {
        stage: BuildStage::WritingSave,
        source: e,
    })?;
    let stem = sanitize_name(name);

    let mut exts: Vec<&str> = Vec::with_capacity(2);
    if out.brdb {
        exts.push("brdb");
    }
    if out.brz {
        exts.push("brz");
    }

    let mut brdb_path: Option<PathBuf> = None;
    let mut extra_paths: Vec<PathBuf> = Vec::new();
    let mut installed_path: Option<PathBuf> = None;
    let mut install_warning: Option<String> = None;

    for ext in exts {
        let path = dir.join(format!("{stem}.{ext}"));
        // `.brdb` is open-if-exists + append, so a stale destination would pile
        // revisions; delete first (mirrors write_brdb / the grid path).
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| BuildError::Io {
                stage: BuildStage::WritingSave,
                source: e,
            })?;
        }
        let path_str = path.to_str().ok_or_else(|| {
            BuildError::BrdbWrite(format!("non-UTF-8 output path {}", path.display()))
        })?;
        write_save_world(world, path_str).map_err(BuildError::BrdbWrite)?;
        if ext == "brdb" {
            brdb_path = Some(path.clone());
        } else {
            extra_paths.push(path.clone());
        }

        if out.install_to_brickadia {
            match install_save(&path, ext, out.overwrite) {
                Ok(dest) => {
                    if installed_path.is_none() {
                        installed_path = Some(dest);
                    }
                }
                Err(e) => {
                    // First skipped install wins the warning slot; the file
                    // stays on disk for manual import regardless.
                    if install_warning.is_none() {
                        install_warning = Some(format!(
                            "install of {} skipped ({e}) — the save remains in {}",
                            path.display(),
                            dir.display(),
                        ));
                    }
                }
            }
        }
    }

    Ok(WriteResult { brdb_path, extra_paths, installed_path, install_warning })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::build::{BlockType, BrickStyle, BuildRequest, FlatColormap, generate_bricks};
    use crate::gui::zones::ZoneMode;
    use crate::gui::dem_sources::DemSource;
    use crate::gui::imagery_sources::ImagerySource;
    use crate::gui::tiles::BBoxLatLon;
    use super::super::heightfield::{FieldMeta, HeightField};
    use super::super::tools::{Raise, SetHeight, Tool};
    use super::super::brush::{Brush, BrushShape, Falloff};

    fn meta() -> FieldMeta {
        FieldMeta {
            cell_m: 30.0,
            studs_per_meter: 1.0,
            vertical_exaggeration: 1.0,
            micro: false,
            centroid_lat: 0.0,
            source_name: "sculpt-test".to_string(),
        }
    }

    fn noop_progress() -> ProgressFn {
        Arc::new(|_: BuildStage, _: f32| {})
    }

    /// Geometry of a brick (position + procedural size) for the `Brick`-has-no-Eq
    /// comparison — mirrors generate.rs's `brick_geom`.
    fn brick_geom(b: &brdb::Brick) -> (brdb::Position, brdb::BrickSize) {
        match &b.asset {
            brdb::BrickType::Procedural { size, .. } => (b.position, *size),
            other => panic!("expected procedural brick, got {other:?}"),
        }
    }

    /// Build bricks for a field through the same internal path convert uses, but
    /// returning the Vec so a test can inspect it (no disk write/install).
    fn mesh_field(field: &HeightField, skip_floor: bool) -> Vec<brdb::Brick> {
        mesh_field_omit(field, skip_floor, 0, 0)
    }

    /// As `mesh_field`, but with the meter-derived brick-Z `base_h` (floor plane)
    /// and `h_omit` (omit threshold) the convert computes from `floor_level_m` /
    /// `omit_below_m`. Lets a test drive the meter-space floor/omit model exactly
    /// as `convert_heightfield` does.
    fn mesh_field_omit(
        field: &HeightField,
        skip_floor: bool,
        base_h: u32,
        h_omit: u32,
    ) -> Vec<brdb::Brick> {
        let raster = field.to_dem_raster();
        let (hscale, vscale) = derive_scale(
            field.meta.cell_m,
            field.meta.studs_per_meter,
            field.meta.vertical_exaggeration,
            field.meta.micro,
        );
        let block_type = if field.meta.micro { BlockType::Micro } else { BlockType::SmoothTile };
        let style = BrickStyle::new(block_type, hscale, false, false);
        let size = i32::from(crate::gui::build::cell_size_units(hscale, field.meta.micro));
        let offset = (-(raster.width as i32 * size), -(raster.height as i32 * size));
        let hm = build_heightmap(&raster, vscale, 0.0);
        let flat = FlatColormap::for_test(raster.width, raster.height);
        generate_bricks_skip_floor(
            &hm,
            &flat,
            style,
            Some(base_h),
            offset,
            skip_floor,
            h_omit,
            crate::opt::MAX_BRICK_UNITS,
            noop_progress(),
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .expect("mesh must succeed")
    }

    /// A flat (all-floor) HeightField converted with skip_floor=true yields ZERO
    /// terrain bricks — the native Brickadia floor stands in. Raising one region
    /// then emits bricks ONLY where terrain was raised.
    #[test]
    fn blank_canvas_floor_emits_no_bricks() {
        let blank = HeightField::flat(24, 24, meta());
        let bricks = mesh_field(&blank, true);
        assert!(
            bricks.is_empty(),
            "an all-floor field with skip_floor=true must emit no terrain bricks, got {}",
            bricks.len(),
        );

        // Raise a small region; only those columns become non-floor → only they
        // emit bricks. Everywhere else stays floor (no bricks).
        let mut raised = HeightField::flat(24, 24, meta());
        let brush = Brush {
            shape: BrushShape::Circle,
            radius_cells: 3.0,
            strength: 20.0,
            falloff: Falloff::Constant,
        };
        Raise.apply(&mut raised, &brush, (12.0, 12.0));
        let raised_bricks = mesh_field(&raised, true);
        assert!(
            !raised_bricks.is_empty(),
            "raising a region must emit terrain bricks",
        );

        // Every emitted brick must sit within the raised footprint's XY span, not
        // out over the still-flat remainder. The raised cells are those within
        // radius 3 of (12,12): columns x,y in [9,15] roughly. Convert that to the
        // world-X span and assert no brick falls outside it.
        let size = i32::from(crate::gui::build::cell_size_units(
            derive_scale(30.0, 1.0, 1.0, false).0,
            false,
        ));
        let offset_x = -(24 * size);
        // Raised cell index range (inclusive) ~ [9, 15]; bricks are placed at
        // 2*size pitch from the offset. Use a generous bound: any brick must lie
        // within the raised cells' world-X window, never over the flat border.
        let min_cell = 9i32;
        let max_cell = 15i32;
        let lo_x = offset_x + 2 * size * min_cell;
        let hi_x = offset_x + 2 * size * (max_cell + 1);
        for b in &raised_bricks {
            assert!(
                b.position.x >= lo_x - 2 * size && b.position.x <= hi_x + 2 * size,
                "brick at x={} fell outside the raised footprint [{lo_x}, {hi_x}] — \
                 flat columns must not emit bricks",
                b.position.x,
            );
        }
    }

    /// Passthrough identity: a from_dem field with NO edits, converted with
    /// skip_floor=FALSE and the SAME options as a direct map build, produces
    /// brick-identical output. This is the headline guard (spec §10) — the
    /// HeightField round-trip is lossless independent of the floor-skip cosmetic.
    #[test]
    fn sculpt_passthrough_identity() {
        // A DEM with a non-zero minimum so floor-subtraction is exercised.
        let raster = DemRaster {
            width: 5,
            height: 4,
            heights_m: vec![
                100.0, 105.0, 100.5, 130.0, 100.0, 162.7, 110.0, 145.2, 100.0, 120.0, 155.0, 133.3,
                108.8, 100.0, 170.0, 140.0, 125.5, 118.2, 160.1, 102.3,
            ],
            min_m: 100.0,
            max_m: 170.0,
        };

        // The Map-build request for the SAME area/settings. vertical_scale 5.0,
        // smooth tile, hscale 1 — a faithful direct build.
        let request = BuildRequest {
            bbox: BBoxLatLon { south: 0.0, north: 1.0, west: 0.0, east: 1.0 },
            name: "identity".to_string(),
            dem_source: DemSource::AwsTerrarium,
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

        // DIRECT map build: build_heightmap(global_min = raster.min_m),
        // generate_bricks (skip_floor=false), centered offset, base_override 0.
        let size = i32::from(crate::gui::build::cell_size_units(request.horizontal_scale, false));
        let offset = (-(raster.width as i32 * size), -(raster.height as i32 * size));
        let style = BrickStyle::from_request(&request);
        let direct_hm = build_heightmap(&raster, request.vertical_scale, raster.min_m);
        let flat = FlatColormap::for_test(raster.width, raster.height);
        let direct = generate_bricks(
            &direct_hm,
            &flat,
            style,
            Some(0),
            offset,
            noop_progress(),
            Arc::new(AtomicBool::new(false)),
        )
        .expect("direct map build must mesh");

        // SCULPT passthrough: from_dem → to_dem_raster → build_heightmap(0.0) →
        // generate_bricks_skip_floor(false). Use the SAME vertical_scale so the
        // two normalize identically; cell metadata is set to make derive_scale a
        // no-op is NOT needed here — we mesh with the same explicit vscale.
        let field = HeightField::from_dem(&raster, meta());
        let rt = field.to_dem_raster();
        let sculpt_hm = build_heightmap(&rt, request.vertical_scale, 0.0);
        let sculpt = generate_bricks_skip_floor(
            &sculpt_hm,
            &flat,
            BrickStyle::from_request(&request),
            Some(0),
            offset,
            false,
            0,
            crate::opt::MAX_BRICK_UNITS,
            noop_progress(),
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .expect("sculpt passthrough must mesh");

        assert!(!direct.is_empty(), "fixture must emit bricks");
        assert_eq!(
            direct.len(),
            sculpt.len(),
            "passthrough must emit the same brick count",
        );
        let direct_geom: Vec<_> = direct.iter().map(brick_geom).collect();
        let sculpt_geom: Vec<_> = sculpt.iter().map(brick_geom).collect();
        assert_eq!(
            direct_geom, sculpt_geom,
            "HeightField passthrough (skip_floor=false) must be brick-identical to a direct build",
        );
    }

    /// TEMPORARY DIAGNOSIS (issue: "lots of gaps" + "heights low"). Builds a
    /// smooth raised hill on a flat 32x32 field, then meshes it via the convert
    /// path at a LOW effective vertical scale vs a PROPER (higher) scale, and
    /// counts: total non-floor cells vs columns that actually emit bricks, and
    /// how many near-floor cells quantize to brick-Z h==0 and are dropped by the
    /// skip_floor `(h-min_height)==0` predicate. Run with:
    ///   cargo test --features gui -- --ignored gap_diagnosis_near_floor_drop --nocapture
    #[test]
    #[ignore = "diagnosis-only evidence gathering; not a CI invariant"]
    fn gap_diagnosis_near_floor_drop() {
        // A flat 32x32 field with one smooth Raise dab in the center. Constant
        // falloff makes a clean disc; a smoother falloff would give a feathered
        // rim of small positive heights (the near-floor cells of interest). Use
        // Smooth falloff so we get a rim of tiny heights toward the edge.
        let w = 32u32;
        let h = 32u32;

        fn meta_spm(spm: f32) -> FieldMeta {
            FieldMeta {
                cell_m: 30.0,
                studs_per_meter: spm,
                vertical_exaggeration: 1.0,
                micro: false,
                centroid_lat: 0.0,
                source_name: "gap-diag".to_string(),
            }
        }

        // Build the hill ONCE (geometry is scale-independent; meta only changes
        // the vertical scale at mesh time). A radius-10 dab, strength 8 m, smooth
        // falloff → a center plateau ~8 m tapering to a wide feathered rim of
        // sub-meter heights.
        let make_field = |spm: f32| {
            let mut f = HeightField::flat(w, h, meta_spm(spm));
            let brush = Brush {
                shape: BrushShape::Circle,
                radius_cells: 10.0,
                strength: 8.0,
                falloff: Falloff::Smoothstep,
            };
            Raise.apply(&mut f, &brush, (16.0, 16.0));
            f
        };

        // For a given studs_per_meter, report the derived vertical scale, the
        // total non-floor cells, the columns that emit bricks (skip_floor=true),
        // and the count of non-floor cells whose quantized brick-Z is 0 (dropped).
        let analyze = |spm: f32| -> (f32, u16, usize, usize, usize) {
            let field = make_field(spm);
            let raster = field.to_dem_raster();
            let (hscale, vscale) = derive_scale(
                field.meta.cell_m,
                field.meta.studs_per_meter,
                field.meta.vertical_exaggeration,
                field.meta.micro,
            );
            // Non-floor source cells (height_m > 0).
            let non_floor = raster.heights_m.iter().filter(|&&m| m > 0.0).count();
            // Quantize each cell exactly as build_heightmap does
            // (`((m - 0) * vscale).max(0).round()`), then count cells that are
            // non-floor in meters but round to brick-Z 0 — the set the skip_floor
            // `(h - min_height) == 0` predicate drops (min_height==0 here because
            // base_override=Some(0)).
            // Exactly build_heightmap's per-cell formula (build.rs:853) with
            // global_min_m = 0.0: `((m - 0) * vscale).max(0).round()`.
            let quantize = |m: f32| -> u32 {
                ((m * vscale).max(0.0).round() as i64).max(0) as u32
            };
            let dropped_to_zero = raster
                .heights_m
                .iter()
                .filter(|&&m| m > 0.0 && quantize(m) == 0)
                .count();
            // Real bricks via the convert mesh path, skip_floor=true.
            let bricks = mesh_field(&field, true);
            // Count distinct emitting columns is hard post-greedy-merge; the brick
            // count is the load-bearing observable (0 bricks => total dropout).
            (vscale, hscale, non_floor, dropped_to_zero, bricks.len())
        };

        // (A) LOW effective vertical scale: a small studs_per_meter so derive_scale
        // clamps hscale to 1 and vertical << 1 unit/m. spm=0.05 -> hscale=round(0.05*15)=1,
        // vertical = 2*1*5/30 = 0.333 units/m. Any cell < 1.5 m rounds to 0.
        let (v_lo, hs_lo, nf_lo, drop_lo, bricks_lo) = analyze(0.05);

        // (B) PROPER 1:1 scale: blank_meta's default studs_per_meter=4.0.
        // hscale=round(4*15)=60 (clamped by MAX_HORIZONTAL_SCALE if lower),
        // vertical = 2*hscale*5/30 -> >= ~20 units/m, so 0.3 m -> h>=1, survives.
        let (v_hi, hs_hi, nf_hi, drop_hi, bricks_hi) = analyze(4.0);

        eprintln!("=== GAP DIAGNOSIS (32x32, Raise r=10 s=8 smooth, cell_m=30) ===");
        eprintln!(
            "LOW  scale: spm=0.05 hscale={hs_lo} vertical={v_lo:.4} u/m | non_floor_cells={nf_lo} dropped_to_h0={drop_lo} bricks={bricks_lo}"
        );
        eprintln!(
            "PROPER scale: spm=4.00 hscale={hs_hi} vertical={v_hi:.4} u/m | non_floor_cells={nf_hi} dropped_to_h0={drop_hi} bricks={bricks_hi}"
        );
        eprintln!(
            "Δ dropped near-floor cells (low - proper) = {}",
            drop_lo as i64 - drop_hi as i64
        );

        // The hill must actually have non-floor cells (the geometry is the same at
        // both scales, so non_floor counts match).
        assert!(nf_lo > 0, "fixture must raise some terrain");
        assert_eq!(nf_lo, nf_hi, "geometry is scale-independent");

        // CONFIRM: at low scale, near-floor non-floor cells quantize to h==0 and are
        // dropped; at proper scale far fewer (ideally zero of the rim) are dropped.
        assert!(
            drop_lo > drop_hi,
            "expected MORE near-floor cells dropped at low scale ({drop_lo}) than proper ({drop_hi})",
        );
    }

    /// A FieldMeta at a PROPER scale: studs_per_meter=4.0 over cell_m=30 derives
    /// hscale=60, vertical=20 u/m (so 1 m → brick-Z 20, and a 0.3 m near-floor
    /// cell maps to h=6, well above the floor). Used by the meter-space floor/omit
    /// tests so the quantization can't itself nibble the cells under test.
    fn meta_proper() -> FieldMeta {
        FieldMeta {
            cell_m: 30.0,
            studs_per_meter: 4.0,
            vertical_exaggeration: 1.0,
            micro: false,
            centroid_lat: 0.0,
            source_name: "omit-test".to_string(),
        }
    }

    /// Total Z-extent of bricks: the summed procedural Z size across the set. A
    /// taller fill (lower base plane) yields a larger sum; raising the floor plane
    /// shrinks it. Scale-independent enough to compare two meshes of one field.
    fn total_z_extent(bricks: &[brdb::Brick]) -> i64 {
        bricks
            .iter()
            .map(|b| match &b.asset {
                brdb::BrickType::Procedural { size, .. } => i64::from(size.z),
                other => panic!("expected procedural brick, got {other:?}"),
            })
            .sum()
    }

    /// Meter-space omit (F4): a column whose SOURCE height (m) is at or below the
    /// omit level emits no bricks, while a column above it survives — and the
    /// decision is made in meter space (the threshold is converted to brick-Z via
    /// the SAME vertical_scale build_heightmap uses). At vertical=20 u/m, with
    /// omit_below_m=0.5 (→ h_omit=10): a 0.3 m cell (h=6 ≤ 10) drops; a 1.5 m cell
    /// (h=30 > 10) survives.
    #[test]
    fn omit_below_drops_cells_at_or_under_level() {
        let (_hs, vscale) = derive_scale(30.0, 4.0, 1.0, false);
        assert!((vscale - 20.0).abs() < 1e-3, "fixture assumes vertical=20 u/m, got {vscale}");

        // One low cell (0.3 m) and one high cell (1.5 m); the rest floor.
        let mut field = HeightField::flat(8, 8, meta_proper());
        field.set(2, 2, 0.3);
        field.set(5, 5, 1.5);

        // Baseline: no omit (omit_below_m=0 → h_omit=0). BOTH non-floor cells
        // survive (0.3 m → h=6, 1.5 m → h=30; neither is h==0).
        let baseline = mesh_field_omit(&field, true, 0, 0);
        assert!(!baseline.is_empty(), "both non-floor cells must emit with no omit");

        // omit_below_m = 0.5 m → h_omit = round(0.5 * 20) = 10. The 0.3 m cell
        // (h=6) is dropped; the 1.5 m cell (h=30) survives.
        let h_omit = (0.5_f32 * vscale).round() as u32;
        assert_eq!(h_omit, 10, "0.5 m at 20 u/m must convert to brick-Z 10");
        let omitted = mesh_field_omit(&field, true, 0, h_omit);
        assert!(
            !omitted.is_empty(),
            "the 1.5 m cell is above the omit level and must still emit",
        );
        assert!(
            omitted.len() < baseline.len(),
            "omitting the 0.3 m cell must drop bricks: baseline={} omitted={}",
            baseline.len(),
            omitted.len(),
        );

        // Raise the omit level above the tall cell too (omit_below_m=2.0 →
        // h_omit=40 ≥ 30): now BOTH cells drop → an all-floor result (no bricks).
        let h_omit_all = (2.0_f32 * vscale).round() as u32;
        let none = mesh_field_omit(&field, true, 0, h_omit_all);
        assert!(
            none.is_empty(),
            "an omit level above every cell must drop all terrain, got {} bricks",
            none.len(),
        );
    }

    /// The gap fix (F4): a small positive-height cell that the brick-Z `(h-min)==0`
    /// predicate USED to drop at low scale now SURVIVES at a proper scale with
    /// omit_below=0 — because the omit decision is meter-space and the proper
    /// vertical_scale maps 0.3 m to h=6 (≥ 1). At a low scale the same cell
    /// quantizes to h=0 and is correctly dropped as true floor.
    #[test]
    fn near_floor_cells_survive_at_proper_scale() {
        // Same geometry at two scales: one 0.3 m cell on an otherwise flat field.
        fn one_cell_field(spm: f32) -> HeightField {
            let meta = FieldMeta {
                cell_m: 30.0,
                studs_per_meter: spm,
                vertical_exaggeration: 1.0,
                micro: false,
                centroid_lat: 0.0,
                source_name: "near-floor".to_string(),
            };
            let mut f = HeightField::flat(8, 8, meta);
            f.set(4, 4, 0.3);
            f
        }

        // PROPER scale (spm=4.0 → vertical=20 u/m): 0.3 m → h=round(6)=6 ≥ 1, so
        // with omit_below=0 (h_omit=0) the cell is NOT floor and must emit.
        let proper = one_cell_field(4.0);
        let (_h, v_proper) = derive_scale(30.0, 4.0, 1.0, false);
        assert!((0.3_f32 * v_proper).round() as u32 >= 1, "proper scale must lift 0.3 m off the floor");
        let proper_bricks = mesh_field_omit(&proper, true, 0, 0);
        assert!(
            !proper_bricks.is_empty(),
            "a 0.3 m cell at a proper scale (omit_below=0) must emit — the gap fix",
        );

        // LOW scale (spm=0.05 → vertical≈0.333 u/m): 0.3 m → h=round(0.1)=0, true
        // floor, correctly dropped (this is the bug's symptom, now scoped to only
        // genuinely-sub-quantum cells rather than the whole near-floor rim).
        let low = one_cell_field(0.05);
        let (_h2, v_low) = derive_scale(30.0, 0.05, 1.0, false);
        assert_eq!((0.3_f32 * v_low).round() as u32, 0, "low scale must quantize 0.3 m to floor");
        let low_bricks = mesh_field_omit(&low, true, 0, 0);
        assert!(
            low_bricks.is_empty(),
            "at low scale the sub-quantum cell maps to true floor and is dropped",
        );
    }

    /// Floor level (F4): raising the base plane the terrain fills DOWN to (a
    /// higher brick-Z `base_override`) shortens every column's fill — the summed
    /// brick Z-extent shrinks versus a base plane at 0. The default base (0) keeps
    /// the deepest fill.
    #[test]
    fn floor_level_raises_base_plane() {
        let (_hs, vscale) = derive_scale(30.0, 4.0, 1.0, false);
        // A single tall cell (5 m → h=100) on a flat field. skip_floor=false so the
        // floor columns are NOT dropped and the fill span is the only variable.
        let mut field = HeightField::flat(8, 8, meta_proper());
        field.set(4, 4, 5.0);

        // Base plane at 0 m (today's default): the tall column fills from h=100 all
        // the way down to brick-Z 0.
        let base_zero = mesh_field_omit(&field, false, 0, 0);
        // Base plane raised to 2 m → brick-Z 40: the same column now fills only
        // from h=100 down to 40, a shorter stack.
        let base_h = (2.0_f32 * vscale).round() as u32;
        assert_eq!(base_h, 40, "2 m at 20 u/m must convert to brick-Z 40");
        let base_raised = mesh_field_omit(&field, false, base_h, 0);

        assert!(!base_zero.is_empty() && !base_raised.is_empty(), "both must emit terrain");
        let z_zero = total_z_extent(&base_zero);
        let z_raised = total_z_extent(&base_raised);
        assert!(
            z_raised < z_zero,
            "raising the floor plane must shorten the fill: base0 Z-extent={z_zero}, raised Z-extent={z_raised}",
        );
    }

    /// Floor + skip_floor interaction (F4 + F6): raising the base plane
    /// (`floor_level_m > 0` → `base_h > 0`) with `skip_floor=true` and the default
    /// omit (`omit_below_m = 0` → `h_omit = 0`) must NOT omit columns that sit at
    /// (or above) the raised base plane — omit is meter-space and independent of
    /// the floor, so only true source-floor (`h == 0`) columns drop. This pins the
    /// chosen semantics (a raised floor shortens fills; it never silently drags the
    /// omit threshold up with it) rather than leaving it implicit.
    #[test]
    fn raised_floor_with_skip_floor_still_emits_base_plane_columns() {
        let (_hs, vscale) = derive_scale(30.0, 4.0, 1.0, false);
        assert!((vscale - 20.0).abs() < 1e-3, "fixture assumes vertical=20 u/m, got {vscale}");

        // Floor raised to 2 m → base_h = 40. A column whose SOURCE height is exactly
        // 2 m sits at the raised base plane; another at 5 m sits above it. A flat
        // (0 m) remainder is true source-floor.
        let mut field = HeightField::flat(8, 8, meta_proper());
        field.set(2, 2, 2.0); // exactly at the raised base plane
        field.set(5, 5, 5.0); // above it
        let base_h = (2.0_f32 * vscale).round() as u32;
        assert_eq!(base_h, 40, "2 m at 20 u/m must convert to brick-Z 40");

        // skip_floor=true, omit_below default (h_omit=0): the 0 m remainder drops as
        // true floor, but the 2 m base-plane column and the 5 m column both have
        // source height > 0 (h = 40 and 100, neither ≤ 0) and MUST still emit.
        let bricks = mesh_field_omit(&field, true, base_h, 0);
        assert!(
            !bricks.is_empty(),
            "columns at/above the raised base plane must still emit (omit is meter-space, independent of floor)",
        );

        // Cross-check: an all-true-floor field (every column 0 m) under the SAME
        // raised base still emits nothing — raising the floor does not by itself
        // create terrain, and the 0 m floor is still dropped.
        let flat = HeightField::flat(8, 8, meta_proper());
        let flat_bricks = mesh_field_omit(&flat, true, base_h, 0);
        assert!(
            flat_bricks.is_empty(),
            "an all-floor field under a raised base must still emit no terrain, got {}",
            flat_bricks.len(),
        );
    }

    // ── Stage 4: grid-tiled sculpt export ────────────────────────────────────

    use crate::gui::grid::tile_world_offset;

    /// Mesh a sub-field exactly as `convert_heightfield_tiled`'s inner loop does
    /// (uniform style/scale, base_h/h_omit, the per-tile world offset over the
    /// FULL field extent), returning the placed bricks. The test composition of
    /// sub_field + tile_world_offset + the mesh — the real path's building block.
    fn mesh_sub(
        field: &HeightField,
        x0: u32,
        y0: u32,
        x1: u32,
        y1: u32,
        skip_floor: bool,
    ) -> Vec<brdb::Brick> {
        let (hscale, vscale) = derive_scale(
            field.meta.cell_m,
            field.meta.studs_per_meter,
            field.meta.vertical_exaggeration,
            field.meta.micro,
        );
        let block_type = if field.meta.micro { BlockType::Micro } else { BlockType::SmoothTile };
        let style = BrickStyle::new(block_type, hscale, false, false);
        let size = crate::gui::build::cell_size_units(hscale, field.meta.micro);
        let sub = field.sub_field(x0, y0, x1, y1);
        let raster = sub.to_dem_raster();
        let offset = tile_world_offset(x0, y0, field.width, field.height, size);
        let hm = build_heightmap(&raster, vscale, 0.0);
        let flat = FlatColormap::for_test(sub.width, sub.height);
        generate_bricks_skip_floor(
            &hm,
            &flat,
            style,
            Some(0),
            offset,
            skip_floor,
            0,
            crate::opt::MAX_BRICK_UNITS,
            noop_progress(),
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .expect("sub-field mesh must succeed")
    }

    /// A multiset of brick geometry (position+size) for order-independent set
    /// comparison — the tiled accumulation order differs from a single mesh's.
    fn geom_multiset(bricks: &[brdb::Brick]) -> std::collections::BTreeMap<(i32, i32, i32, u16, u16, u16), usize> {
        let mut m = std::collections::BTreeMap::new();
        for b in bricks {
            let (p, s) = brick_geom(b);
            *m.entry((p.x, p.y, p.z, s.x, s.y, s.z)).or_insert(0) += 1;
        }
        m
    }

    /// A field at a proper scale with a smooth raised hill — non-trivial geometry
    /// so the tiling has real bricks to place and seams to abut.
    fn hill_field(w: u32, h: u32) -> HeightField {
        let mut f = HeightField::flat(w, h, meta_proper());
        let brush = Brush {
            shape: BrushShape::Circle,
            radius_cells: (w.min(h) as f32) * 0.4,
            strength: 6.0,
            falloff: Falloff::Smoothstep,
        };
        Raise.apply(&mut f, &brush, (w as f32 * 0.5, h as f32 * 0.5));
        f
    }

    /// `tile_bounds` partitions into NON-overlapping half-open ranges: a 2×…
    /// split steps by `tile_cells` cells and adjacent ranges abut. The per-tile
    /// world offsets place every ABSOLUTE cell on one fixed world slot, so a
    /// tile's last column and its neighbor's first column sit exactly one
    /// per-cell pitch (`2*size`) apart — watertight, no gap and no overlap.
    #[test]
    fn tiled_export_world_offset_abutment() {
        // A 16×16 field, tile_cells=8 → a 2×2 partition.
        let field = hill_field(16, 16);
        let x_bounds = tile_bounds(16, 8);
        let y_bounds = tile_bounds(16, 8);
        assert_eq!(x_bounds.len(), 2, "16 cells / tile 8 → 2 columns of tiles");
        assert_eq!(y_bounds.len(), 2, "16 cells / tile 8 → 2 rows of tiles");

        // Non-overlapping partition: A = [0,8), B = [8,16) → B starts exactly
        // where A ends, no shared cell.
        assert_eq!(x_bounds[0], (0, 8), "first tile is exactly tile_cells wide");
        assert_eq!(x_bounds[1].0, x_bounds[0].1, "next tile starts where the prior ends");

        let (hscale, _v) = derive_scale(
            field.meta.cell_m,
            field.meta.studs_per_meter,
            field.meta.vertical_exaggeration,
            field.meta.micro,
        );
        let size = crate::gui::build::cell_size_units(hscale, field.meta.micro);
        let size_i = i32::from(size);

        // The per-cell world pitch is 2*size: absolute column c lands at
        // 2*size*c + center_x in whatever tile contains it. Tile 0's last column
        // is absolute 7 (local 7); tile 1's first is absolute 8 (local 0). They
        // must sit exactly one pitch apart — abut with no gap, no overlap.
        let off0 = tile_world_offset(x_bounds[0].0, 0, 16, 16, size); // tile col 0, start 0
        let off1 = tile_world_offset(x_bounds[1].0, 0, 16, 16, size); // tile col 1, start 8
        let tile0_last_x = off0.0 + 2 * size_i * 7; // absolute col 7, local x=7
        let tile1_first_x = off1.0; // absolute col 8, local x=0
        assert_eq!(
            tile1_first_x - tile0_last_x,
            2 * size_i,
            "adjacent tiles abut at exactly one per-cell pitch (no gap, no overlap)",
        );
        // …and each lands where a single mesh places that absolute column:
        // center_x = -(16*size), so column c → -(16*size) + 2*size*c.
        let center_x = -(16 * size_i);
        assert_eq!(tile0_last_x, center_x + 2 * size_i * 7, "tile 0 col 7 at the single-mesh slot");
        assert_eq!(tile1_first_x, center_x + 2 * size_i * 8, "tile 1 col 8 at the single-mesh slot");
    }

    /// A field that fits within ONE tile (`tile_cells >= max(w,h)`) tiles into a
    /// single sub-field == the whole field at the SAME world offset, so the
    /// stitched geometry is IDENTICAL to a single mesh of the field — the no-seam
    /// equivalence (spec §5/§7). Compared as a geometry multiset (accumulation
    /// order differs).
    #[test]
    fn tiled_vs_single_mesh_equivalence() {
        let field = hill_field(20, 14);

        // tile_cells larger than both dims → tile_bounds yields ONE range per axis
        // covering the whole field (the no-split path).
        let xb = tile_bounds(20, 64);
        let yb = tile_bounds(14, 64);
        assert_eq!(xb, vec![(0, 20)], "an over-size tile spans the whole width");
        assert_eq!(yb, vec![(0, 14)], "an over-size tile spans the whole height");

        // Single mesh of the whole field (the convert_heightfield geometry path).
        let single = mesh_field(&field, true);
        // The tiled path's single sub-field == whole field, placed at the same
        // offset (tile_world_offset(0,0,w,h) == -(w*size),-(h*size)).
        let tiled = mesh_sub(&field, 0, 0, 20, 14, true);

        assert!(!single.is_empty(), "the hill must emit bricks");
        assert_eq!(
            geom_multiset(&tiled),
            geom_multiset(&single),
            "a within-one-tile tiled stitch must be geometry-identical to a single mesh",
        );
    }

    /// `tile_bounds` invariants: ranges PARTITION the extent (contiguous,
    /// non-overlapping), cover it fully, each width ≤ tile_cells (the per-tile
    /// cell budget the convert relies on), and an over-size tile collapses to one
    /// full-extent range.
    #[test]
    fn tile_bounds_partition_and_coverage() {
        let b = tile_bounds(16, 8);
        // Contiguous partition: each range starts exactly where the prior ends —
        // no shared cell, no gap.
        for w in b.windows(2) {
            assert_eq!(w[1].0, w[0].1, "adjacent tiles abut with no shared cell");
        }
        assert_eq!(b.first().unwrap().0, 0, "first tile starts at 0");
        assert_eq!(b.last().unwrap().1, 16, "last tile reaches the extent");
        for &(s, e) in &b {
            assert!(e > s, "every range is non-empty");
            assert!(e - s <= 8, "tile width ≤ tile_cells");
        }
        // 16 / 8 → exactly two non-overlapping tiles.
        assert_eq!(b, vec![(0, 8), (8, 16)], "clean partition into tile_cells-wide ranges");
        // Over-size tile → one full range; a 1-cell field → one [0,1).
        assert_eq!(tile_bounds(20, 64), vec![(0, 20)]);
        assert_eq!(tile_bounds(1, 8), vec![(0, 1)]);
    }

    /// End-to-end tiled convert (spec §5): a multi-tile field routed through the
    /// public `convert_heightfield_tiled` writes ONE stitched save whose
    /// `brick_count` equals the accumulated geometry of its partition sub-fields
    /// (the seams abut watertight, never gaps). brz-only + no-install so the test
    /// never touches a Brickadia tree.
    #[test]
    fn tiled_export_stitches_all_tiles() {
        // 18×18 hill, tile_cells=8 → a 3×3 partition split (non-trivial seams).
        let field = hill_field(18, 18);
        let x_bounds = tile_bounds(18, 8);
        let y_bounds = tile_bounds(18, 8);
        assert!(x_bounds.len() >= 2 && y_bounds.len() >= 2, "must actually subdivide");

        // The expected stitched count: mesh each partition sub-field via the SAME
        // building block the path uses and sum (the watertight, seam-duplicated
        // union). This is the geometry the convert must write.
        let mut expected = 0usize;
        for &(y0, y1) in &y_bounds {
            for &(x0, x1) in &x_bounds {
                expected += mesh_sub(&field, x0, y0, x1, y1, true).len();
            }
        }
        assert!(expected > 0, "the tiled hill must emit bricks");

        let out = OutputOptions {
            brdb: false,
            brz: true,
            install_to_brickadia: false,
            overwrite: true,
            skip_floor: true,
        };
        let outcome = convert_heightfield_tiled(
            &field,
            out,
            8,
            0.0,
            0.0,
            &[],
            None,
            None,
            crate::opt::MAX_BRICK_UNITS,
            noop_progress(),
            Arc::new(AtomicBool::new(false)),
        )
        .expect("tiled convert must succeed");
        assert_eq!(
            outcome.brick_count, expected,
            "the stitched save must contain every tile's bricks (watertight seams)",
        );
        assert_eq!((outcome.dem_width, outcome.dem_height), (18, 18), "reported dims = full field");
        // Clean up the staged .brz so the test leaves no artifact.
        if !outcome.brdb_path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&outcome.brdb_path);
        }
    }

    /// A field with a DISTINCT height in every cell, so the greedy mesher can
    /// never merge two cells — each cell becomes its own brick. That makes a
    /// seam-duplicated column an EXACTLY coincident brick (the defect's clean
    /// signature), not a partially-overlapping merge that's harder to detect.
    fn ramp_field(w: u32, h: u32) -> HeightField {
        let mut f = HeightField::flat(w, h, meta_proper());
        for y in 0..h {
            for x in 0..w {
                f.set(x, y, 5.0 + x as f32 * 0.7 + y as f32 * 1.3);
            }
        }
        f
    }

    /// Count bricks that EXACTLY coincide with at least one other (same position
    /// AND size). A single greedy mesh partitions space, so it emits ZERO
    /// coincidences; any coincidence in a tiled stitch is a seam-duplicated cell
    /// — two solid bricds in one voxel, which Z-fight in Brickadia and read as a
    /// seam line ("straight lines on a regular grid" on big/tiled exports).
    fn coincident_brick_count(bricks: &[brdb::Brick]) -> usize {
        let mut seen: std::collections::HashMap<(i32, i32, i32, u16, u16, u16), usize> =
            std::collections::HashMap::new();
        for b in bricks {
            let (p, s) = brick_geom(b);
            *seen.entry((p.x, p.y, p.z, s.x, s.y, s.z)).or_insert(0) += 1;
        }
        seen.values().filter(|&&n| n > 1).map(|&n| n - 1).sum()
    }

    /// REGRESSION (user bug: "cuts in the terrain … only on big/tiled worlds …
    /// straight lines on a regular grid"). A multi-tile stitch must not place two
    /// EXACTLY coincident bricks at a seam — coincident solid bricks Z-fight in
    /// Brickadia and render as the seam lines the user sees. The single mesh of
    /// the same field is the watertight reference: it emits zero coincidences, so
    /// the tiled stitch must too. The shared-edge (overlapping) tiler FAILS this;
    /// a clean half-open partition (abutting, by the integer offset algebra)
    /// passes while still covering every column.
    #[test]
    fn tiled_seams_emit_no_coincident_duplicate_bricks() {
        // Distinct-per-cell heights → no greedy merge → every seam-shared column
        // is a standalone, exactly-coincident brick if the tiler overlaps.
        let field = ramp_field(24, 24);
        let x_bounds = tile_bounds(24, 8);
        let y_bounds = tile_bounds(24, 8);
        assert!(x_bounds.len() >= 3 && y_bounds.len() >= 3, "must subdivide into seams");

        // The watertight reference: a single greedy mesh never coincides.
        let single = mesh_field(&field, true);
        assert_eq!(
            coincident_brick_count(&single),
            0,
            "a single mesh partitions space — it must emit no coincident bricks",
        );

        // The tiled stitch of the SAME field, via the real per-tile building block.
        let mut tiled: Vec<brdb::Brick> = Vec::new();
        for &(y0, y1) in &y_bounds {
            for &(x0, x1) in &x_bounds {
                tiled.extend(mesh_sub(&field, x0, y0, x1, y1, true));
            }
        }
        assert_eq!(
            coincident_brick_count(&tiled),
            0,
            "tiled seams must not duplicate cells — coincident bricks Z-fight into seam lines",
        );
    }

    /// Neither format selected → a checked error, never a silent no-op write.
    #[test]
    fn convert_requires_a_format() {
        let field = HeightField::flat(4, 4, meta());
        let out = OutputOptions {
            brdb: false,
            brz: false,
            install_to_brickadia: false,
            overwrite: false,
            skip_floor: true,
        };
        let err = convert_heightfield(
            &field,
            out,
            0.0,
            0.0,
            &[],
            None,
            None,
            crate::opt::MAX_BRICK_UNITS,
            noop_progress(),
            Arc::new(AtomicBool::new(false)),
        )
        .expect_err("no format selected must error");
        assert!(matches!(err, BuildError::BrdbWrite(_)), "expected BrdbWrite, got {err:?}");
    }

    /// Splat paint reaches the bricks: a cell painted with swatch 1 produces at
    /// least one brick carrying that swatch's RGB, while an unpainted build colors
    /// every brick with the default flat color (the byte-identity baseline).
    #[test]
    fn paint_colors_reach_bricks() {
        use super::super::paint::{default_palette, PaintColormap, PaintGrid};

        // A tall painted cell so it emits bricks. Proper scale lifts 5 m well off
        // the floor.
        let mut field = HeightField::flat(8, 8, meta_proper());
        field.set(4, 4, 5.0);
        let raster = field.to_dem_raster();
        let (hscale, vscale) = derive_scale(30.0, 4.0, 1.0, false);
        let style = BrickStyle::new(BlockType::SmoothTile, hscale, false, false);
        let size = i32::from(crate::gui::build::cell_size_units(hscale, false));
        let offset = (-(raster.width as i32 * size), -(raster.height as i32 * size));
        let hm = build_heightmap(&raster, vscale, 0.0);

        let pal = default_palette();
        let mut grid = PaintGrid::blank(8, 8);
        grid.set_block(4, 4, 1, 1); // paint the tall cell with swatch 1

        let mesh = |cells: &[u8]| {
            let cm = PaintColormap::new(cells, &pal, raster.width, raster.height);
            generate_bricks_skip_floor(
                &hm,
                &cm,
                style,
                Some(0),
                offset,
                true,
                0,
                crate::opt::MAX_BRICK_UNITS,
                noop_progress(),
                Arc::new(AtomicBool::new(false)),
                None,
            )
            .expect("mesh must succeed")
        };

        let painted = mesh(&grid.cells);
        assert!(!painted.is_empty(), "the tall painted cell must emit bricks");
        let want = pal[1];
        assert!(
            painted.iter().any(|b| b.color.r == want[0] && b.color.g == want[1] && b.color.b == want[2]),
            "a brick must carry the painted swatch color {want:?}",
        );

        // Unpainted (blank) grid → every brick is the default flat color, the
        // byte-identity baseline the paint layer must not perturb when unused.
        let blank = PaintGrid::blank(8, 8);
        let unpainted = mesh(&blank.cells);
        let def = crate::gui::build::DEFAULT_BRICK_COLOR;
        assert!(
            unpainted.iter().all(|b| b.color.r == def[0] && b.color.g == def[1] && b.color.b == def[2]),
            "an unpainted grid must color every brick the default flat color",
        );
    }

    /// REPRO (user bug: "flat areas at 192 m and 132 m loaded as ground level").
    /// A mostly-floor field with two flat plateaus at 132 m and 192 m, meshed
    /// through the sculpt path (skip_floor=true, default base/omit), must emit
    /// bricks that REACH those heights — not drop them to native ground.
    #[test]
    fn flat_plateaus_at_height_are_not_flattened() {
        let (_hs, vscale) = derive_scale(30.0, 4.0, 1.0, false);
        assert!((vscale - 20.0).abs() < 1e-3, "fixture assumes vertical=20 u/m");

        let mut field = HeightField::flat(12, 12, meta_proper());
        // Two flat plateaus; the rest stays at floor (0).
        for y in 2..5 {
            for x in 2..5 {
                field.set(x, y, 132.0);
            }
        }
        for y in 7..10 {
            for x in 7..10 {
                field.set(x, y, 192.0);
            }
        }

        let bricks = mesh_field(&field, true);
        assert!(!bricks.is_empty(), "plateaus at height must emit bricks, not vanish to floor");

        // The tallest brick must reach ~192 m worth of brick-Z (192*20=3840), and
        // some must reach ~132 m (2640) — proving neither plateau flattened to 0.
        let max_z = bricks.iter().map(|b| b.position.z).max().unwrap_or(0);
        assert!(
            max_z >= (192.0 * vscale * 0.9) as i32,
            "tallest plateau (192 m) must reach its height; max brick z={max_z}",
        );
        assert!(
            bricks.iter().any(|b| b.position.z >= (132.0 * vscale * 0.9) as i32),
            "the 132 m plateau must emit bricks near its height",
        );
    }

    /// End-to-end of the user's actual workflow: build a flat area with the
    /// Set-height TOOL (target 192 m) then export. If the tool stores the target
    /// and convert keeps it, this passes — isolating the bug away from the tool.
    #[test]
    fn set_height_tool_plateau_survives_export() {
        let mut field = HeightField::flat(16, 16, meta_proper());
        let brush = Brush { shape: BrushShape::Circle, radius_cells: 5.0, strength: 1.0, falloff: Falloff::Constant };
        SetHeight { target: 192.0 }.apply(&mut field, &brush, (8.0, 8.0));
        assert!((field.at(8, 8) - 192.0).abs() < 1e-3, "Set-height must store the typed target");

        let bricks = mesh_field(&field, true);
        assert!(!bricks.is_empty(), "a Set-height 192 m plateau must emit bricks");
        let (_h, vscale) = derive_scale(30.0, 4.0, 1.0, false);
        let max_z = bricks.iter().map(|b| b.position.z).max().unwrap_or(0);
        assert!(max_z >= (192.0 * vscale * 0.9) as i32, "plateau must reach 192 m; max_z={max_z}");
    }

    /// Terrace export: snapping a smooth ramp to steps collapses it onto far
    /// fewer distinct brick heights (the stepped/plateau look) while keeping the
    /// peak altitude. Mirrors what convert does (terrace the heights pre-mesh).
    #[test]
    fn terraced_field_meshes_to_fewer_height_levels() {
        use super::super::heightfield::terrace_height;
        use std::collections::BTreeSet;

        // A smooth diagonal ramp 0..~110 m over a 12×12 field.
        let mut smooth = HeightField::flat(12, 12, meta_proper());
        for y in 0..12 {
            for x in 0..12 {
                smooth.set(x, y, (x + y) as f32 * 5.0);
            }
        }
        let distinct = |bricks: &[brdb::Brick]| -> usize {
            bricks.iter().map(|b| b.position.z).collect::<BTreeSet<_>>().len()
        };
        let smooth_levels = distinct(&mesh_field(&smooth, true));

        // Terrace to 25 m steps (what convert does to the raster before meshing).
        let mut stepped = smooth.clone();
        for c in stepped.cells.iter_mut() {
            *c = terrace_height(*c, 25.0);
        }
        let stepped_bricks = mesh_field(&stepped, true);
        let stepped_levels = distinct(&stepped_bricks);

        assert!(
            stepped_levels < smooth_levels,
            "terracing must collapse heights: smooth={smooth_levels} stepped={stepped_levels}",
        );
        // Altitude preserved: the stepped peak is within a step of the smooth peak.
        let (_h, vscale) = derive_scale(30.0, 4.0, 1.0, false);
        let peak = stepped_bricks.iter().map(|b| b.position.z).max().unwrap_or(0);
        assert!(peak as f32 >= (110.0 - 25.0) * vscale, "stepped peak keeps accurate altitude");
    }

    /// REPRO at the user's scale (cell_m≈4): a LARGE flat plateau at 132 m,
    /// surrounded by floor, meshed through the sculpt path. The big flat region
    /// merges into large greedy quads — assert those quads still emit bricks that
    /// reach 132 m (not collapse to ground).
    #[test]
    fn large_flat_plateau_user_scale_emits() {
        let meta = FieldMeta {
            cell_m: 4.0,
            studs_per_meter: 4.0,
            vertical_exaggeration: 1.0,
            micro: false,
            centroid_lat: 0.0,
            source_name: "user-scale".to_string(),
        };
        let (_hs, vscale) = derive_scale(4.0, 4.0, 1.0, false);

        let mut field = HeightField::flat(96, 96, meta);
        // A 40×40 flat plateau at 132 m in the middle; rest stays floor (0).
        for y in 28..68 {
            for x in 28..68 {
                field.set(x, y, 132.0);
            }
        }

        let bricks = mesh_field(&field, true);
        assert!(!bricks.is_empty(), "the plateau must emit bricks");
        let max_z = bricks.iter().map(|b| b.position.z).max().unwrap_or(0);
        assert!(
            max_z >= (132.0 * vscale * 0.9) as i32,
            "large flat plateau must reach 132 m at user scale; vscale={vscale} max_z={max_z}",
        );
    }

    // ---- Step 5: freedraw zones applied at convert -------------------------

    fn rect_zone(mode: ZoneMode, x0: f32, y0: f32, x1: f32, y1: f32) -> Zone {
        Zone {
            mode,
            polygon: vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)],
        }
    }

    /// As `mesh_field`, but with a freedraw keep-mask — exactly what the single
    /// convert path does (`rasterize` at the field's dims → mesh).
    fn mesh_field_masked(field: &HeightField, skip_floor: bool, mask: &[bool]) -> Vec<brdb::Brick> {
        let raster = field.to_dem_raster();
        let (hscale, vscale) = derive_scale(
            field.meta.cell_m,
            field.meta.studs_per_meter,
            field.meta.vertical_exaggeration,
            field.meta.micro,
        );
        let block_type = if field.meta.micro { BlockType::Micro } else { BlockType::SmoothTile };
        let style = BrickStyle::new(block_type, hscale, false, false);
        let size = i32::from(crate::gui::build::cell_size_units(hscale, field.meta.micro));
        let offset = (-(raster.width as i32 * size), -(raster.height as i32 * size));
        let hm = build_heightmap(&raster, vscale, 0.0);
        let flat = FlatColormap::for_test(raster.width, raster.height);
        generate_bricks_skip_floor(
            &hm, &flat, style, Some(0), offset, skip_floor, 0,
            crate::opt::MAX_BRICK_UNITS,
            noop_progress(), Arc::new(AtomicBool::new(false)), Some(mask),
        )
        .expect("masked mesh must succeed")
    }

    /// Mesh one tile of `field` with the per-tile slice of a full-field mask —
    /// the independent reference for the tiled convert's slicing.
    fn mesh_sub_masked(
        field: &HeightField,
        x0: u32,
        y0: u32,
        x1: u32,
        y1: u32,
        skip_floor: bool,
        full_mask: &[bool],
    ) -> Vec<brdb::Brick> {
        let (hscale, vscale) = derive_scale(
            field.meta.cell_m,
            field.meta.studs_per_meter,
            field.meta.vertical_exaggeration,
            field.meta.micro,
        );
        let block_type = if field.meta.micro { BlockType::Micro } else { BlockType::SmoothTile };
        let style = BrickStyle::new(block_type, hscale, false, false);
        let size = crate::gui::build::cell_size_units(hscale, field.meta.micro);
        let sub = field.sub_field(x0, y0, x1, y1);
        let mut tile_mask = Vec::with_capacity((sub.width * sub.height) as usize);
        for ty in 0..sub.height {
            for tx in 0..sub.width {
                tile_mask.push(full_mask[((y0 + ty) * field.width + (x0 + tx)) as usize]);
            }
        }
        let raster = sub.to_dem_raster();
        let offset = tile_world_offset(x0, y0, field.width, field.height, size);
        let hm = build_heightmap(&raster, vscale, 0.0);
        let flat = FlatColormap::for_test(sub.width, sub.height);
        generate_bricks_skip_floor(
            &hm, &flat, style, Some(0), offset, skip_floor, 0,
            crate::opt::MAX_BRICK_UNITS,
            noop_progress(), Arc::new(AtomicBool::new(false)), Some(&tile_mask),
        )
        .expect("masked sub-field mesh must succeed")
    }

    /// An omit zone over a raised region drops those bricks, and NO surviving
    /// brick is centered inside the omit footprint. An include zone keeps ONLY
    /// its interior. Spatial proof the convert's rasterize lands at the right
    /// cells/orientation (a transposed or mis-dimensioned mask would fail here).
    #[test]
    fn single_convert_omit_clears_footprint_include_keeps_interior() {
        let field = hill_field(24, 24);
        let base = mesh_field(&field, true);
        assert!(!base.is_empty(), "the hill must emit bricks");

        // Omit the central [8,16) × [8,16) block (where the hill's bricks live).
        let omit = rect_zone(ZoneMode::Omit, 8.0, 8.0, 16.0, 16.0);
        let mask = zones::rasterize(std::slice::from_ref(&omit), 24, 24);
        let masked = mesh_field_masked(&field, true, &mask);
        assert!(masked.len() < base.len(), "omit over the hill removes bricks");

        // World window of the omit block: cell c maps to offset + 2*size*c.
        let size = i32::from(crate::gui::build::cell_size_units(
            derive_scale(
                field.meta.cell_m,
                field.meta.studs_per_meter,
                field.meta.vertical_exaggeration,
                field.meta.micro,
            )
            .0,
            field.meta.micro,
        ));
        let off = -(24 * size);
        let lo = off + 2 * size * 8;
        let hi = off + 2 * size * 16;
        for b in &masked {
            let inside = b.position.x >= lo && b.position.x < hi && b.position.y >= lo && b.position.y < hi;
            assert!(!inside, "no surviving brick may be centered inside the omit footprint");
        }

        // Include the SAME central block → only the hill's interior survives, and
        // it is non-empty (the hill peak is inside the block).
        let inc = rect_zone(ZoneMode::Include, 8.0, 8.0, 16.0, 16.0);
        let inc_mask = zones::rasterize(std::slice::from_ref(&inc), 24, 24);
        let included = mesh_field_masked(&field, true, &inc_mask);
        assert!(!included.is_empty(), "include over the hill keeps its bricks");
        for b in &included {
            let inside = b.position.x >= lo && b.position.x < hi && b.position.y >= lo && b.position.y < hi;
            assert!(inside, "every included brick must sit inside the include footprint");
        }
    }

    /// A zone spanning a tile seam: the tiled convert's per-tile slices must
    /// reassemble to the full-field mask. Two independent checks:
    /// (a) SPATIAL — using an ASYMMETRIC omit block (so a transposed slice lands a
    ///     visibly wrong hole), no surviving sub-field brick is centered inside the
    ///     block's world window (the window is derived from the zone's cell bounds,
    ///     NOT from the slice code, so it catches an orientation bug the count-only
    ///     comparison on symmetric data could not);
    /// (b) STITCH — the public `convert_heightfield_tiled` brick_count equals the
    ///     summed masked sub-fields (watertight across seams).
    #[test]
    fn tiled_seam_spanning_zone_matches_summed_masked_subfields() {
        let field = hill_field(18, 18); // tile 8 → 3×3 split
        let x_bounds = tile_bounds(18, 8);
        let y_bounds = tile_bounds(18, 8);
        assert!(x_bounds.len() >= 2 && y_bounds.len() >= 2, "must subdivide");

        // ASYMMETRIC omit block: cells x∈[2,8), y∈[9,15) — its transpose covers a
        // different region, so a transposed slice would relocate the hole.
        let (zx0, zy0, zx1, zy1) = (2u32, 9u32, 8u32, 15u32);
        let zone = rect_zone(ZoneMode::Omit, zx0 as f32, zy0 as f32, zx1 as f32, zy1 as f32);
        let full_mask = zones::rasterize(std::slice::from_ref(&zone), 18, 18);

        let mut ref_bricks: Vec<brdb::Brick> = Vec::new();
        let mut unmasked = 0usize;
        for &(y0, y1) in &y_bounds {
            for &(x0, x1) in &x_bounds {
                ref_bricks.extend(mesh_sub_masked(&field, x0, y0, x1, y1, true, &full_mask));
                unmasked += mesh_sub(&field, x0, y0, x1, y1, true).len();
            }
        }
        let expected = ref_bricks.len();
        assert!(expected < unmasked, "the seam-spanning omit must actually drop bricks");

        // (a) Spatial: world window of the omit block. Absolute cell c maps to
        // world `-(18*size) + 2*size*c` on each axis (the centering the tiled path
        // and a single mesh share — see `tiled_export_world_offset_abutment`).
        let size = i32::from(crate::gui::build::cell_size_units(
            derive_scale(
                field.meta.cell_m,
                field.meta.studs_per_meter,
                field.meta.vertical_exaggeration,
                field.meta.micro,
            )
            .0,
            field.meta.micro,
        ));
        let off = -(18 * size);
        let world = |c: u32| off + 2 * size * c as i32;
        let (wx0, wx1, wy0, wy1) = (world(zx0), world(zx1), world(zy0), world(zy1));
        for b in &ref_bricks {
            let in_hole = b.position.x >= wx0 && b.position.x < wx1 && b.position.y >= wy0 && b.position.y < wy1;
            assert!(!in_hole, "no masked-tile brick may sit inside the omit window (orientation check)");
        }

        // (b) Stitch consistency through the public tiled convert.
        let out = OutputOptions {
            brdb: false,
            brz: true,
            install_to_brickadia: false,
            overwrite: true,
            skip_floor: true,
        };
        let outcome = convert_heightfield_tiled(
            &field,
            out,
            8,
            0.0,
            0.0,
            std::slice::from_ref(&zone),
            None,
            None,
            crate::opt::MAX_BRICK_UNITS,
            noop_progress(),
            Arc::new(AtomicBool::new(false)),
        )
        .expect("tiled zone convert must succeed");
        assert_eq!(
            outcome.brick_count, expected,
            "tiled stitch with a seam-spanning zone == summed masked sub-fields",
        );
        if !outcome.brdb_path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&outcome.brdb_path);
        }
    }

    /// Re-converting the SAME field+zones is deterministic AND the zone actually
    /// shapes the output: a masked convert drops bricks versus no zones, and
    /// repeating it reproduces the exact count. (Non-consumption itself is a
    /// compile-time guarantee of the `&[Zone]` borrow, so it isn't re-asserted —
    /// this exercises the observable behavior instead: stable, mask-affected output.)
    #[test]
    fn convert_with_zones_is_deterministic_and_mask_affects_output() {
        let field = hill_field(16, 16);
        let zones = vec![rect_zone(ZoneMode::Omit, 4.0, 4.0, 12.0, 12.0)];
        let out = OutputOptions {
            brdb: false,
            brz: true,
            install_to_brickadia: false,
            overwrite: true,
            skip_floor: true,
        };
        let run = |z: &[Zone]| {
            let o = convert_heightfield(
                &field,
                out,
                0.0,
                0.0,
                z,
                None,
                None,
                crate::opt::MAX_BRICK_UNITS,
                noop_progress(),
                Arc::new(AtomicBool::new(false)),
            )
            .expect("convert must succeed");
            if !o.brdb_path.as_os_str().is_empty() {
                let _ = std::fs::remove_file(&o.brdb_path);
            }
            o.brick_count
        };
        let no_zone = run(&[]);
        let masked_a = run(&zones);
        let masked_b = run(&zones);
        assert_eq!(masked_a, masked_b, "repeated masked convert is deterministic");
        assert!(masked_a < no_zone, "the omit zone must actually drop bricks ({masked_a} < {no_zone})");
        assert!(masked_a > 0, "the omit leaves the rest of the hill intact");
    }

    // ── Layer export (MVP, single resolution) ────────────────────────────────

    use super::super::layers::LayerStack;

    /// A LayerStack over an `fw×fh` field at `div` box divisions, assigning whole
    /// boxes to layers: each `((bi,bj), li)` paints box `(bi,bj)` into layer index
    /// `li` (layer 0 = the auto-created Base; higher indices are added in order).
    fn stack_with_boxes(
        fw: u32,
        fh: u32,
        div: (u32, u32),
        assignments: &[((u32, u32), usize)],
    ) -> LayerStack {
        let mut s = LayerStack::new(fw, fh);
        s.grid_div = div;
        let max_layer = assignments.iter().map(|&(_, l)| l).max().unwrap_or(0);
        for _ in 1..=max_layer {
            s.add_layer(fw, fh); // inserts above active and activates → indices 1..=max in order
        }
        for &((bi, bj), li) in assignments {
            s.active = li;
            s.paint_box(fw, fh, bi, bj, true);
        }
        s
    }

    /// Mesh a planned part with the convert defaults (skip_floor, base 0, omit 0),
    /// at the field's derived scale — the per-part building block `export_layer_parts`
    /// uses, returning bricks for inspection.
    fn build_part(field: &HeightField, plan: &super::PartPlan) -> Vec<brdb::Brick> {
        let (hscale, vscale) = derive_scale(
            field.meta.cell_m,
            field.meta.studs_per_meter,
            field.meta.vertical_exaggeration,
            field.meta.micro,
        );
        let style = BrickStyle::new(BlockType::SmoothTile, hscale, false, false);
        super::build_part_bricks(
            plan, field, style, vscale, 0, 0, true, crate::opt::MAX_BRICK_UNITS,
            noop_progress(), Arc::new(AtomicBool::new(false)),
        )
        .expect("part mesh must succeed")
    }

    /// INV-REGRESSION: one layer covering the WHOLE field exports a part that is
    /// brick-identical (as a geometry multiset) to convert's single full-field
    /// mesh. The no-layers baseline must never drift.
    #[test]
    fn inv_regression_full_layer_equals_single_mesh() {
        let field = hill_field(20, 16);
        let (hscale, _v) = derive_scale(30.0, 4.0, 1.0, false);
        let size_base = crate::gui::build::cell_size_units(hscale, false);

        let mut s = LayerStack::new(20, 16);
        s.grid_div = (1, 1);
        s.paint_box(20, 16, 0, 0, true); // whole field → Base

        let plans = plan_layer_parts(&s, &field, size_base).expect("plan");
        assert_eq!(plans.len(), 1, "one non-empty layer → exactly one part");
        assert_eq!(plans[0].bbox, (0, 0, 20, 16), "a full selection spans the whole field");

        let part = build_part(&field, &plans[0]);
        let single = mesh_field(&field, true);
        assert_eq!(
            geom_multiset(&part),
            geom_multiset(&single),
            "a full-field layer part must equal the single mesh (INV-REGRESSION)",
        );
    }

    /// INV-POSITION: a part covering a SUBSET places its bricks at the SAME
    /// absolute positions the single full-field mesh uses for those cells. Uses a
    /// distinct-per-cell ramp so no greedy merge spans the bbox edge (which would
    /// legitimately differ) — every part brick must then exist in the single mesh.
    #[test]
    fn inv_position_subset_part_matches_single_mesh_cells() {
        let field = ramp_field(16, 16);
        let (hscale, _v) = derive_scale(30.0, 4.0, 1.0, false);
        let size_base = crate::gui::build::cell_size_units(hscale, false);

        // 2×1 grid: Base owns the left half (box (0,0) → cols 0..8).
        let s = stack_with_boxes(16, 16, (2, 1), &[((0, 0), 0)]);
        let plans = plan_layer_parts(&s, &field, size_base).expect("plan");
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].bbox, (0, 0, 8, 16), "left-half selection → left-half bbox");

        let part = build_part(&field, &plans[0]);
        let single = mesh_field(&field, true);
        let single_set = geom_multiset(&single);
        assert!(!part.is_empty(), "the part must emit bricks");
        for b in &part {
            let (p, sz) = brick_geom(b);
            let key = (p.x, p.y, p.z, sz.x, sz.y, sz.z);
            assert!(
                single_set.contains_key(&key),
                "part brick {key:?} is absent from the single mesh — positions diverged",
            );
        }
    }

    /// INV-NOCOINCIDE + INV-COVERAGE: two layers partitioning the field export
    /// parts that (a) never place two bricks in one cell, and (b) reassemble the
    /// full field exactly. Distinct-per-cell ramp → no greedy merge, so the union
    /// is brick-exact.
    #[test]
    fn inv_nocoincide_and_coverage_two_layer_partition() {
        let field = ramp_field(16, 16);
        let (hscale, _v) = derive_scale(30.0, 4.0, 1.0, false);
        let size_base = crate::gui::build::cell_size_units(hscale, false);

        // 2×1 grid: Base owns the left box, Layer 1 owns the right box.
        let s = stack_with_boxes(16, 16, (2, 1), &[((0, 0), 0), ((1, 0), 1)]);
        let plans = plan_layer_parts(&s, &field, size_base).expect("plan");
        assert_eq!(plans.len(), 2, "two non-empty layers → two parts");

        let mut all = Vec::new();
        for plan in &plans {
            all.extend(build_part(&field, plan));
        }
        assert_eq!(
            coincident_brick_count(&all),
            0,
            "partitioned parts must place no two bricks in one cell (INV-NOCOINCIDE)",
        );
        let single = mesh_field(&field, true);
        assert_eq!(
            geom_multiset(&all),
            geom_multiset(&single),
            "two partitioned parts must reassemble the full field exactly (INV-COVERAGE)",
        );
    }

    /// INV-DETERMINISM: the same stack + field produce byte-identical part
    /// geometry across runs (stable part order, stable claim).
    #[test]
    fn inv_determinism_same_stack_same_bricks() {
        let field = ramp_field(12, 12);
        let (hscale, _v) = derive_scale(30.0, 4.0, 1.0, false);
        let size_base = crate::gui::build::cell_size_units(hscale, false);
        let s = stack_with_boxes(12, 12, (2, 2), &[((0, 0), 0), ((1, 1), 1), ((0, 1), 1)]);

        let run = || {
            let plans = plan_layer_parts(&s, &field, size_base).expect("plan");
            plans.iter().map(|p| build_part(&field, p)).collect::<Vec<_>>()
        };
        let a = run();
        let b = run();
        assert_eq!(a.len(), b.len(), "same part count");
        for (pa, pb) in a.iter().zip(&b) {
            assert_eq!(geom_multiset(pa), geom_multiset(pb), "each part is deterministic");
        }
    }

    /// INV-EXTENT: the world-extent predicate the planner gates on rejects an
    /// offset that would exceed Brickadia's i16 ChunkIndex bound (so a
    /// pre-positioned part is refused, never silently clamped off-lattice). A
    /// normal centered field fits.
    #[test]
    fn inv_extent_predicate_rejects_over_extent_offset() {
        let size = crate::gui::build::cell_size_units(1, false); // 5 units/cell
        assert!(
            offset_fits_chunk_index(part_axis_offset(0, 64, size), 64, size),
            "a small centered world fits the world-extent bound",
        );
        // A pathological pitch pushes |offset| past the bound — must be rejected.
        let huge = u16::MAX;
        assert!(
            !offset_fits_chunk_index(part_axis_offset(0, 4096, huge), 4096, huge),
            "an over-extent offset must be rejected (the gate refuses, never clamps)",
        );
    }

    /// End-to-end: `export_layer_parts` writes ONE save PER visible non-empty
    /// layer (brz-only, no-install) and reports each part's brick count. Cleans up.
    #[test]
    fn export_layer_parts_writes_one_save_per_layer() {
        let field = hill_field(24, 18);
        let s = stack_with_boxes(24, 18, (2, 1), &[((0, 0), 0), ((1, 0), 1)]);
        let out = OutputOptions {
            brdb: false,
            brz: true,
            install_to_brickadia: false,
            overwrite: true,
            skip_floor: true,
        };
        let parts = export_layer_parts(
            &s, &field, out, 0.0, 0.0, crate::opt::MAX_BRICK_UNITS,
            noop_progress(), Arc::new(AtomicBool::new(false)),
        )
        .expect("layer export must succeed");
        assert_eq!(parts.len(), 2, "two non-empty layers → two saves");
        for p in &parts {
            assert!(p.brick_count > 0, "each part must emit bricks");
            if !p.primary_path.as_os_str().is_empty() {
                let _ = std::fs::remove_file(&p.primary_path);
            }
        }
    }
}
