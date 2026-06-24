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
    self, BlockType, BrickStyle, BuildError, BuildOutcome, BuildStage, DemRaster, ProgressFn,
    build_heightmap, builds_dir, generate_bricks_skip_floor, install_save, sanitize_name,
};
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
pub(crate) fn convert_heightfield(
    field: &HeightField,
    out: OutputOptions,
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
    // m - raster.min_m). base_override = Some(0) fills every non-floor column to
    // the shared floor plane.
    let heightmap = build_heightmap(&raster, vertical_scale, 0.0);

    progress(BuildStage::GeneratingBricks, 0.0);
    let flat = build::FlatColormap::sculpt_default(dem_width, dem_height);
    let bricks = generate_bricks_skip_floor(
        &heightmap,
        &flat,
        style,
        Some(0),
        offset,
        out.skip_floor,
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
            Some(0),
            offset,
            skip_floor,
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
            noop_progress(),
            Arc::new(AtomicBool::new(false)),
        )
        .expect_err("no format selected must error");
        assert!(matches!(err, BuildError::BrdbWrite(_)), "expected BrdbWrite, got {err:?}");
    }
}
