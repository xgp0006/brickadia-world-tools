//! `HeightField` — the sculpt workspace's editable terrain grid ("layer 0").
//!
//! Cells are heights in **meters above the native Brickadia floor**, row-major,
//! all `>= FLOOR_M`. The field is built from a real-world DEM, a blank canvas,
//! or a grayscale heightmap image, brushed into shape, then converted back to a
//! [`DemRaster`] so the existing `build_heightmap` → `generate_bricks` pipeline
//! consumes it unchanged. `to_dem_raster` is the Sculpt → Convert seam.

use crate::gui::build::DemRaster;

/// Height `0.0` IS the native Brickadia flat ground plane: the model's hard
/// lower bound. Every mutation clamps cells to `>= FLOOR_M` — you can only build
/// up, nothing carves beneath the floor.
pub(crate) const FLOOR_M: f32 = 0.0;

/// Convert + scale metadata carried alongside the height grid. None of these
/// affect the grid values themselves; they parameterize the eventual convert
/// (pitch, exaggeration, brick style) and name the output.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FieldMeta {
    /// Ground meters per cell (pitch). For a DEM this is the lat-corrected cell
    /// size; for a blank canvas it is the user-chosen ground m/cell.
    pub cell_m: f64,
    /// Studs of brick width per real meter (the horizontal play-scale knob).
    pub studs_per_meter: f32,
    /// Vertical exaggeration multiplier (1.0 = faithful 1:1 relief).
    pub vertical_exaggeration: f32,
    /// Micro-brick mode (fine detail); mirrors `BlockType::Micro`.
    pub micro: bool,
    /// Centroid latitude of the source area, for lat-correct pitch via
    /// `derive_scale`. A blank canvas uses `0.0` (a sane default at the equator).
    pub centroid_lat: f64,
    /// Output filename stem.
    pub source_name: String,
}

/// An editable terrain height grid. `cells` is row-major, length
/// `width * height`, every value `>= FLOOR_M`.
#[derive(Clone, Debug)]
pub(crate) struct HeightField {
    pub width: u32,
    pub height: u32,
    pub cells: Vec<f32>,
    pub meta: FieldMeta,
}

impl HeightField {
    /// Build a field from a decoded DEM. CRITICAL: subtract `raster.min_m` so the
    /// DEM's lowest point sits at `FLOOR_M` — cells become meters-above-floor,
    /// all `>= 0`. This is the same zero-floor normalization `build_heightmap`
    /// applies (`m - global_min_m`), pulled forward into the editable model so
    /// what you sculpt is already floor-relative.
    pub(crate) fn from_dem(raster: &DemRaster, meta: FieldMeta) -> Self {
        debug_assert!(
            raster.heights_m.len() == (raster.width as usize) * (raster.height as usize),
            "DemRaster height buffer must match its dimensions",
        );
        let min_m = raster.min_m;
        let cells: Vec<f32> = raster
            .heights_m
            .iter()
            .map(|&m| (m - min_m).max(FLOOR_M))
            .collect();
        Self { width: raster.width, height: raster.height, cells, meta }
    }

    /// A blank canvas: every cell at the floor.
    pub(crate) fn flat(width: u32, height: u32, meta: FieldMeta) -> Self {
        let cells = vec![FLOOR_M; (width as usize) * (height as usize)];
        Self { width, height, cells, meta }
    }

    /// Build a field from a grayscale heightmap image: each luminance sample
    /// becomes meters above the floor (clamped `>= FLOOR_M`). Row-major to match
    /// the image's `(x, y)` iteration.
    pub(crate) fn from_image(img: &image::GrayImage, meta: FieldMeta) -> Self {
        let (width, height) = (img.width(), img.height());
        let mut cells = Vec::with_capacity((width as usize) * (height as usize));
        for y in 0..height {
            for x in 0..width {
                let lum = img.get_pixel(x, y).0[0];
                cells.push((lum as f32).max(FLOOR_M));
            }
        }
        Self { width, height, cells, meta }
    }

    /// Height at integer cell `(x, y)`. Debug-checked bounds (Rule 5/7).
    pub(crate) fn at(&self, x: u32, y: u32) -> f32 {
        debug_assert!(
            x < self.width && y < self.height,
            "HeightField::at out-of-bounds ({x},{y}) vs {}x{}",
            self.width,
            self.height,
        );
        self.cells[(y * self.width + x) as usize]
    }

    /// Set cell `(x, y)`, clamping to `>= FLOOR_M` (the floor invariant: nothing
    /// carves beneath the native ground). Debug-checked bounds.
    pub(crate) fn set(&mut self, x: u32, y: u32, v: f32) {
        debug_assert!(
            x < self.width && y < self.height,
            "HeightField::set out-of-bounds ({x},{y}) vs {}x{}",
            self.width,
            self.height,
        );
        self.cells[(y * self.width + x) as usize] = v.max(FLOOR_M);
    }

    /// Bilinear sample at fractional cell coordinates. Coordinates are clamped
    /// into `[0, width-1] x [0, height-1]` so edge samples saturate rather than
    /// read out of bounds. A 1x1 field returns its single cell.
    ///
    /// A documented `HeightField` accessor (spec §3) with full test coverage; the
    /// MVP's nearest-cell brush + texture render read via `at`, so the only
    /// current caller is the test suite. Kept (with a scoped allow) as the
    /// sub-cell sampler the live-preview resample seam (#3) taps — removing it
    /// would just have #3 re-add an identical, re-tested method.
    #[allow(dead_code)]
    pub(crate) fn sample_bilinear(&self, fx: f32, fy: f32) -> f32 {
        debug_assert!(self.width > 0 && self.height > 0, "empty HeightField");
        let max_x = self.width - 1;
        let max_y = self.height - 1;
        let cx = fx.clamp(0.0, max_x as f32);
        let cy = fy.clamp(0.0, max_y as f32);
        let x0 = cx.floor() as u32;
        let y0 = cy.floor() as u32;
        let x1 = (x0 + 1).min(max_x);
        let y1 = (y0 + 1).min(max_y);
        let tx = cx - x0 as f32;
        let ty = cy - y0 as f32;

        let v00 = self.at(x0, y0);
        let v10 = self.at(x1, y0);
        let v01 = self.at(x0, y1);
        let v11 = self.at(x1, y1);

        let top = v00 + (v10 - v00) * tx;
        let bottom = v01 + (v11 - v01) * tx;
        top + (bottom - top) * ty
    }

    /// Minimum and maximum cell heights. A non-empty field always has both
    /// (cells are constructed non-empty by every constructor); an empty field
    /// returns `(FLOOR_M, FLOOR_M)`.
    pub(crate) fn min_max(&self) -> (f32, f32) {
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        for &c in &self.cells {
            min = min.min(c);
            max = max.max(c);
        }
        if min.is_finite() && max.is_finite() {
            (min, max)
        } else {
            (FLOOR_M, FLOOR_M)
        }
    }

    /// Produce the converter's input. The returned [`DemRaster`] is
    /// **floor-relative**: `min_m = 0.0` and `heights_m = cells`, so converting
    /// it with `build_heightmap(global_min_m = 0.0)` reproduces exactly the
    /// `m - raster.min_m` normalization a direct map build performs — the
    /// passthrough identity (a DEM → HeightField → DemRaster round-trip, no
    /// edits, is lossless at the brick level). This is the Sculpt → Convert seam.
    pub(crate) fn to_dem_raster(&self) -> DemRaster {
        let (_, max) = self.min_max();
        DemRaster {
            width: self.width,
            height: self.height,
            heights_m: self.cells.clone(),
            // Cells are already meters-above-floor (>= 0), so the floor is the
            // datum: a zero global_min lets build_heightmap pass them straight
            // through. min_m is 0.0 by construction (FLOOR_M); max_m tracks the
            // tallest cell so reported elevation stays correct.
            min_m: FLOOR_M,
            max_m: max,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::build::{build_heightmap, generate_bricks, BlockType, BrickStyle, BuildRequest};
    use crate::gui::dem_sources::DemSource;
    use crate::gui::imagery_sources::ImagerySource;
    use crate::gui::tiles::BBoxLatLon;
    use crate::map::Heightmap;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn test_meta() -> FieldMeta {
        FieldMeta {
            cell_m: 30.0,
            studs_per_meter: 1.0,
            vertical_exaggeration: 1.0,
            micro: false,
            centroid_lat: 0.0,
            source_name: "test".to_string(),
        }
    }

    /// A small, irregular DEM with a non-zero minimum so the floor-subtraction
    /// is actually exercised (cells must come out as `m - min_m`).
    fn test_raster() -> DemRaster {
        DemRaster {
            width: 3,
            height: 2,
            heights_m: vec![100.0, 105.0, 100.5, 130.0, 100.0, 162.7],
            min_m: 100.0,
            max_m: 162.7,
        }
    }

    #[test]
    fn from_dem_roundtrips_dims_and_values() {
        let raster = test_raster();
        let f = HeightField::from_dem(&raster, test_meta());
        assert_eq!((f.width, f.height), (raster.width, raster.height));
        // Cells are meters above the floor: m - min_m, clamped >= 0.
        let expected: Vec<f32> =
            raster.heights_m.iter().map(|&m| m - raster.min_m).collect();
        assert_eq!(f.cells, expected, "from_dem must store floor-relative heights");
        // The DEM's lowest point sits exactly at the floor.
        assert_eq!(f.min_max().0, FLOOR_M, "lowest DEM cell must land on FLOOR_M");
        // Spot-check an interior accessor against the floor-relative value.
        assert_eq!(f.at(2, 1), 162.7 - 100.0);
    }

    #[test]
    fn min_max_correct() {
        let f = HeightField::from_dem(&test_raster(), test_meta());
        let (min, max) = f.min_max();
        // min cell is (100 - 100) = 0; max cell is (162.7 - 100) = 62.7.
        assert_eq!(min, 0.0);
        assert!((max - 62.7).abs() < 1e-3, "max cell must be 62.7, got {max}");

        // A flat canvas is all floor.
        let flat = HeightField::flat(4, 4, test_meta());
        assert_eq!(flat.min_max(), (FLOOR_M, FLOOR_M));
    }

    #[test]
    fn sample_bilinear_interpolates() {
        // 2x2 field with known corners: bilinear midpoint is the corner mean,
        // edge midpoints are edge means, corners read back exactly.
        let mut f = HeightField::flat(2, 2, test_meta());
        f.set(0, 0, 0.0);
        f.set(1, 0, 10.0);
        f.set(0, 1, 20.0);
        f.set(1, 1, 30.0);

        // Corners exact.
        assert_eq!(f.sample_bilinear(0.0, 0.0), 0.0);
        assert_eq!(f.sample_bilinear(1.0, 0.0), 10.0);
        assert_eq!(f.sample_bilinear(0.0, 1.0), 20.0);
        assert_eq!(f.sample_bilinear(1.0, 1.0), 30.0);

        // Center = mean of all four = 15.
        assert!((f.sample_bilinear(0.5, 0.5) - 15.0).abs() < 1e-5);
        // Top edge midpoint = (0 + 10)/2 = 5.
        assert!((f.sample_bilinear(0.5, 0.0) - 5.0).abs() < 1e-5);
        // Left edge midpoint = (0 + 20)/2 = 10.
        assert!((f.sample_bilinear(0.0, 0.5) - 10.0).abs() < 1e-5);
        // Out-of-range coords clamp to the nearest edge, not panic.
        assert_eq!(f.sample_bilinear(-5.0, -5.0), 0.0);
        assert_eq!(f.sample_bilinear(99.0, 99.0), 30.0);
    }

    #[test]
    fn set_clamps_to_floor() {
        let mut f = HeightField::flat(2, 2, test_meta());
        f.set(0, 0, -50.0);
        assert_eq!(f.at(0, 0), FLOOR_M, "set must clamp below-floor values up to FLOOR_M");
        f.set(1, 1, 12.5);
        assert_eq!(f.at(1, 1), 12.5, "in-range set must store the value verbatim");
    }

    fn passthrough_request() -> BuildRequest {
        BuildRequest {
            bbox: BBoxLatLon { south: 0.0, north: 1.0, west: 0.0, east: 1.0 },
            name: "passthrough".to_string(),
            dem_source: DemSource::AwsTerrarium,
            imagery_source: ImagerySource::None,
            mapbox_token: None,
            opentopo_key: None,
            vertical_scale: 5.0,
            density_factor: 1,
            horizontal_scale: 1,
            block_type: BlockType::Tile,
            glow: false,
            no_collision: false,
            install_to_brickadia: false,
            overwrite_world: false,
        }
    }

    /// Geometry of a brick, position + procedural size, for `Brick`-has-no-Eq
    /// comparison (mirrors generate.rs's `brick_geom`).
    fn brick_geom(b: &brdb::Brick) -> (brdb::Position, brdb::BrickSize) {
        match &b.asset {
            brdb::BrickType::Procedural { size, .. } => (b.position, *size),
            other => panic!("expected procedural brick, got {other:?}"),
        }
    }

    #[test]
    fn to_dem_raster_lossless() {
        let raster = test_raster();
        let f = HeightField::from_dem(&raster, test_meta());

        // 1) The floor-relative grid reproduces from_dem's cells exactly.
        let rt = f.to_dem_raster();
        assert_eq!((rt.width, rt.height), (raster.width, raster.height));
        assert_eq!(rt.min_m, FLOOR_M, "round-tripped raster must be floor-relative");
        let expected_cells: Vec<f32> =
            raster.heights_m.iter().map(|&m| m - raster.min_m).collect();
        assert_eq!(rt.heights_m, expected_cells, "to_dem_raster must reproduce the floor-relative grid");

        // 2) Passthrough identity at the BRICK level: converting the round-tripped
        //    raster with build_heightmap(global_min = 0) must equal a DIRECT map
        //    build of the source raster (build_heightmap(global_min = raster.min_m)).
        //    Both use generate_bricks with skip_floor's default-off path
        //    (fill_to_base=true inside generate_bricks; base_override = Some(0)).
        let request = passthrough_request();
        let style = BrickStyle::from_request(&request);
        let offset = (0, 0);
        let noop: crate::gui::build::ProgressFn =
            Arc::new(|_: crate::gui::build::BuildStage, _: f32| {});
        let cancel = Arc::new(AtomicBool::new(false));

        let direct_hm = build_heightmap(&raster, request.vertical_scale, raster.min_m);
        let sculpt_hm = build_heightmap(&rt, request.vertical_scale, FLOOR_M);

        // The normalized brick heights must already match before meshing — this is
        // the algebraic core of the identity ((m - min_m) == (cell - 0)).
        for y in 0..raster.height {
            for x in 0..raster.width {
                assert_eq!(
                    direct_hm.at(x, y),
                    sculpt_hm.at(x, y),
                    "normalized height mismatch at ({x},{y})",
                );
            }
        }

        let flat = crate::gui::build::FlatColormap::for_test(raster.width, raster.height);
        let direct = generate_bricks(
            &direct_hm,
            &flat,
            style,
            Some(0),
            offset,
            Arc::clone(&noop),
            Arc::clone(&cancel),
        )
        .expect("direct build must mesh");
        let sculpt = generate_bricks(
            &sculpt_hm,
            &flat,
            style,
            Some(0),
            offset,
            Arc::clone(&noop),
            Arc::clone(&cancel),
        )
        .expect("sculpt passthrough build must mesh");

        assert!(!direct.is_empty(), "fixture must emit bricks");
        assert_eq!(direct.len(), sculpt.len(), "passthrough must emit the same brick count");
        let direct_geom: Vec<_> = direct.iter().map(brick_geom).collect();
        let sculpt_geom: Vec<_> = sculpt.iter().map(brick_geom).collect();
        assert_eq!(
            direct_geom, sculpt_geom,
            "HeightField passthrough must produce brick-identical output to a direct build",
        );
    }
}
