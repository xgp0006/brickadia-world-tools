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
use crate::gui::grid::tile_world_offset;
use crate::gui::map_tab::derive_scale;
use crate::util::{bricks_to_save, write_save_world};

use super::heightfield::HeightField;

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
pub(crate) fn convert_heightfield(
    field: &HeightField,
    out: OutputOptions,
    floor_level_m: f32,
    omit_below_m: f32,
    progress: ProgressFn,
    cancel: Arc<AtomicBool>,
) -> Result<BuildOutcome, BuildError> {
    if !out.brdb && !out.brz {
        return Err(BuildError::BrdbWrite(
            "select at least one output format (.brdb or .brz)".to_owned(),
        ));
    }

    let raster: DemRaster = field.to_dem_raster();
    // Report the field's true range (cells are floor-relative meters).
    let dem_width = raster.width;
    let dem_height = raster.height;
    let elevation_min_m = raster.min_m;
    let elevation_max_m = raster.max_m;

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
    let bricks = generate_bricks_skip_floor(
        &heightmap,
        &flat,
        style,
        Some(base_h),
        offset,
        out.skip_floor,
        h_omit,
        Arc::clone(&progress),
        Arc::clone(&cancel),
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

/// Shared-edge tile boundaries along ONE axis: `extent` cells split into
/// `ceil(extent / tile_cells)` half-open ranges `[start, end)` where adjacent
/// ranges SHARE one edge cell (`end_i == start_{i+1} + 1`). The shared cell is
/// meshed by both neighbors and placed at the same world slot (the cumulative
/// `start` offset cancels the local index), so the seam is watertight and
/// duplicate-on-top rather than gapped. A single range (`tile_cells >= extent`)
/// returns one `[0, extent)` covering the whole axis — the no-split path that
/// stitches byte-identically to a single mesh.
///
/// Each returned range's WIDTH is `<= tile_cells + 1` (the body plus the one
/// shared boundary cell), so a tile never exceeds the per-tile cell budget the
/// caller checks. `extent` and `tile_cells` are both clamped to `>= 1`.
fn tile_bounds(extent: u32, tile_cells: u32) -> Vec<(u32, u32)> {
    let extent = extent.max(1);
    let step = tile_cells.max(1);
    if step >= extent {
        return vec![(0, extent)];
    }
    let mut out = Vec::new();
    let mut start = 0u32;
    // Bounded by ceil(extent/step) iterations; `start` advances by `step` each
    // pass and the loop stops once a range reaches the far edge (Rule 2).
    while start < extent {
        // Body of `step` cells plus one SHARED edge cell with the next tile, so
        // the next tile begins on this tile's last column. The final tile clamps
        // to `extent` (no neighbor to share with).
        let end = (start + step + 1).min(extent);
        out.push((start, end));
        if end == extent {
            break;
        }
        // Next tile starts on the shared edge cell (end - 1).
        start = end - 1;
    }
    out
}

/// Convert an edited [`HeightField`] into ONE stitched save by meshing it as a
/// grid of shared-edge sub-fields (spec §5, the manual "Tile this export" path).
/// The field is split into `ceil(w / tile_cells) × ceil(h / tile_cells)`
/// sub-fields by [`tile_bounds`] (adjacent sub-fields share their exact edge
/// column/row — seams are exact by integer cell identity, no projection drift).
///
/// Each sub-field meshes through the EXACT same pipeline as `convert_heightfield`
/// (uniform `size` from one [`derive_scale`], `base_override` from `floor_level_m`,
/// `global_min = 0`, the meter-space `omit_below_m`, the same scale/micro), with a
/// per-tile world offset from cumulative cells + global centering via
/// [`tile_world_offset`] — the SAME algebra `grid.rs::world_offset` uses. The
/// shared edge cell's cumulative `off_cells` cancels its local index, so a shared
/// column lands on the identical world slot from either neighbor (watertight).
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
            let raster = sub.to_dem_raster();
            // Per-tile world offset from cumulative cells (the tile's NW cell index
            // is its absolute start) + the global-centering term over the FULL
            // field extent — the same algebra grid.rs::world_offset uses.
            let offset = tile_world_offset(x0, y0, dem_width, dem_height, size);
            let heightmap = build_heightmap(&raster, vertical_scale, 0.0);
            // The flat colormap must match THIS sub-field's dimensions (the mesher
            // asserts heightmap and colormap share dims), not the full field's.
            let flat = build::FlatColormap::sculpt_default(sub.width, sub.height);

            // A no-op per-tile progress sink: the aggregate bar advances per tile
            // below; a per-tile stage fraction would thrash the readout.
            let tile_progress: ProgressFn = Arc::new(|_, _| {});
            let bricks = generate_bricks_skip_floor(
                &heightmap,
                &flat,
                style,
                Some(base_h),
                offset,
                out.skip_floor,
                h_omit,
                tile_progress,
                Arc::clone(&cancel),
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
    use crate::gui::dem_sources::DemSource;
    use crate::gui::imagery_sources::ImagerySource;
    use crate::gui::tiles::BBoxLatLon;
    use super::super::heightfield::{FieldMeta, HeightField};
    use super::super::tools::{Raise, Tool};
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
            noop_progress(),
            Arc::new(AtomicBool::new(false)),
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
            noop_progress(),
            Arc::new(AtomicBool::new(false)),
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
            noop_progress(),
            Arc::new(AtomicBool::new(false)),
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

    /// `tile_bounds` splits into shared-edge half-open ranges: a 2×… split steps
    /// by `tile_cells` cells and adjacent ranges share one edge cell, so the
    /// per-tile world offsets abut at exactly the `2*size` per-cell pitch — the
    /// shared column lands on the IDENTICAL world slot from either neighbor.
    #[test]
    fn tiled_export_world_offset_abutment() {
        // A 16×16 field, tile_cells=8 → a 2×2 split with shared edges.
        let field = hill_field(16, 16);
        let x_bounds = tile_bounds(16, 8);
        let y_bounds = tile_bounds(16, 8);
        assert_eq!(x_bounds.len(), 2, "16 cells / tile 8 → 2 columns of tiles");
        assert_eq!(y_bounds.len(), 2, "16 cells / tile 8 → 2 rows of tiles");

        // Adjacent ranges share their edge cell: A = [0,9), B = [8,16) → B starts
        // on A's last column (8). (tile_bounds emits body+1 = 9-wide first tile.)
        assert_eq!(x_bounds[0], (0, 9), "first tile is body(8)+shared(1) wide");
        assert_eq!(x_bounds[1].0, x_bounds[0].1 - 1, "next tile starts on the shared column");

        let (hscale, _v) = derive_scale(
            field.meta.cell_m,
            field.meta.studs_per_meter,
            field.meta.vertical_exaggeration,
            field.meta.micro,
        );
        let size = crate::gui::build::cell_size_units(hscale, field.meta.micro);
        let size_i = i32::from(size);

        // The per-cell world pitch is 2*size: column c (absolute) lands at
        // 2*size*c + center_x for EVERY tile that contains it. Verify the shared
        // column (abs 8) maps to the same world-x from tile 0 and tile 1.
        let off0 = tile_world_offset(x_bounds[0].0, 0, 16, 16, size); // tile col 0, start 0
        let off1 = tile_world_offset(x_bounds[1].0, 0, 16, 16, size); // tile col 1, start 8
        // Shared column is local x=8 in tile 0 and local x=0 in tile 1.
        let from_tile0_x = off0.0 + 2 * size_i * 8;
        let from_tile1_x = off1.0; // local x=0 → no per-cell pitch added
        assert_eq!(
            from_tile0_x, from_tile1_x,
            "the shared column must land on ONE world-x from either neighbor (abut, no gap)",
        );
        // …and it equals the absolute-column world placement a single mesh uses:
        // center_x = -(16*size), so column 8 → -(16*size) + 2*size*8 = 0.
        let center_x = -(16 * size_i);
        assert_eq!(from_tile0_x, center_x + 2 * size_i * 8, "abut at the 2*size per-cell pitch");
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

    /// `tile_bounds` invariants: ranges are contiguous shared-edge, cover the full
    /// extent, each width ≤ tile_cells+1 (the per-tile cell budget the convert
    /// relies on), and an over-size tile collapses to one full-extent range.
    #[test]
    fn tile_bounds_shared_edge_and_coverage() {
        let b = tile_bounds(16, 8);
        // Contiguous via the shared edge: each range starts on the prior's last cell.
        for w in b.windows(2) {
            assert_eq!(w[1].0, w[0].1 - 1, "adjacent tiles share one edge cell");
        }
        assert_eq!(b.first().unwrap().0, 0, "first tile starts at 0");
        assert_eq!(b.last().unwrap().1, 16, "last tile reaches the extent");
        for &(s, e) in &b {
            assert!(e > s, "every range is non-empty");
            assert!(e - s <= 8 + 1, "tile width ≤ tile_cells + the shared edge cell");
        }
        // Over-size tile → one full range; a 1-cell field → one [0,1).
        assert_eq!(tile_bounds(20, 64), vec![(0, 20)]);
        assert_eq!(tile_bounds(1, 8), vec![(0, 1)]);
    }

    /// End-to-end tiled convert (spec §5): a multi-tile field routed through the
    /// public `convert_heightfield_tiled` writes ONE stitched save whose
    /// `brick_count` equals the accumulated geometry of its shared-edge sub-fields
    /// (the seams are watertight duplicate-on-top, never gaps). brz-only +
    /// no-install so the test never touches a Brickadia tree.
    #[test]
    fn tiled_export_stitches_all_tiles() {
        // 18×18 hill, tile_cells=8 → a 3×3 shared-edge split (non-trivial seams).
        let field = hill_field(18, 18);
        let x_bounds = tile_bounds(18, 8);
        let y_bounds = tile_bounds(18, 8);
        assert!(x_bounds.len() >= 2 && y_bounds.len() >= 2, "must actually subdivide");

        // The expected stitched count: mesh each shared-edge sub-field via the SAME
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
            noop_progress(),
            Arc::new(AtomicBool::new(false)),
        )
        .expect_err("no format selected must error");
        assert!(matches!(err, BuildError::BrdbWrite(_)), "expected BrdbWrite, got {err:?}");
    }
}
