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

/// Vertical scale baked into an exported heightmap PNG: meters × this = the
/// integer height stored per pixel. Matches Stage 1's (`geotiff2heightmap`)
/// default `-v 100` (1 unit ≈ 1 cm), so exports load through the same
/// rgba-encoded `HeightmapPNG` decoder the CLI/map pipeline already uses.
pub(crate) const HEIGHTMAP_PNG_SCALE: f32 = 100.0;

/// Snap a height (meters) to the nearest `step_m` multiple — the "terrace"
/// transform that turns smooth relief into discrete stepped plateaus while
/// keeping the altitude accurate (to within `step_m / 2`). A non-positive step
/// is a no-op (smooth). Non-destructive: applied at render/convert, never to the
/// stored field, so the smooth/stepped choice flips per project.
pub(crate) fn terrace_height(m: f32, step_m: f32) -> f32 {
    if step_m <= 0.0 || !step_m.is_finite() {
        return m;
    }
    ((m / step_m).round() * step_m).max(FLOOR_M)
}

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

    /// Decode an rgba-encoded heightmap PNG produced by [`to_heightmap_png`] (or
    /// the Stage 1 CLI pipeline): each pixel's RGBA bytes are a big-endian `u32`
    /// of `round(meters * HEIGHTMAP_PNG_SCALE)`. The exact inverse of
    /// `to_heightmap_png`, so a sculpted field round-trips losslessly (to 1/scale
    /// precision). Cells are already floor-relative — NO min subtraction —
    /// clamped `>= FLOOR_M`.
    ///
    /// This is the decoder the GUI "load heightmap image" path must use for our
    /// own 4-channel exports; reading them as 8-bit luminance instead crushes
    /// every height by ~0.045× (the round-trip bug).
    pub(crate) fn from_heightmap_png(img: &image::RgbaImage, meta: FieldMeta) -> Self {
        let (width, height) = (img.width(), img.height());
        let mut cells = Vec::with_capacity((width as usize) * (height as usize));
        for y in 0..height {
            for x in 0..width {
                let packed = u32::from_be_bytes(img.get_pixel(x, y).0);
                cells.push((packed as f32 / HEIGHTMAP_PNG_SCALE).max(FLOOR_M));
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

    /// Sample the cell height (meters above the native floor) under fractional
    /// cell coordinates `(fx, fy)` — the canvas-pointer eyedropper sampler (spec
    /// §3). Cells already ARE meters-above-floor, so this is a nearest-cell read
    /// that floors `(fx, fy)` to an integer cell and clamps into
    /// `[0, width-1] × [0, height-1]` so an off-grid or edge pointer saturates to
    /// the nearest cell rather than reading out of bounds. Negative or NaN
    /// coordinates clamp to cell `(0, 0)`. Returns the height in meters.
    pub(crate) fn sample_cell_meters(&self, fx: f32, fy: f32) -> f32 {
        debug_assert!(self.width > 0 && self.height > 0, "empty HeightField");
        let max_x = self.width - 1;
        let max_y = self.height - 1;
        // floor() of a negative/NaN coordinate is handled by the clamp: NaN.max
        // returns the bound, and a negative floor below 0 clamps up to 0.
        let cx = fx.floor().clamp(0.0, max_x as f32) as u32;
        let cy = fy.floor().clamp(0.0, max_y as f32) as u32;
        self.at(cx, cy)
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

    /// Encode the field as an rgba-encoded heightmap PNG — the same format Stage 1
    /// (`geotiff2heightmap/map.py`) emits and `HeightmapPNG::new(.., rgba_encoded =
    /// true)` decodes. Each cell's floor-relative height in meters is scaled by
    /// [`HEIGHTMAP_PNG_SCALE`], rounded, and packed as a big-endian `u32` across the
    /// RGBA channels, so a sculpted field round-trips losslessly through the
    /// CLI/map pipeline (and is a faithful, shareable heightmap).
    pub(crate) fn to_heightmap_png(&self) -> image::RgbaImage {
        image::RgbaImage::from_fn(self.width, self.height, |x, y| {
            let meters = self.at(x, y);
            // clamp guards against absurd heights overflowing the u32 encoding;
            // real terrain (≤ ~9 km × 100) is nowhere near the ceiling.
            let scaled = (meters * HEIGHTMAP_PNG_SCALE).round().clamp(0.0, u32::MAX as f32) as u32;
            image::Rgba(scaled.to_be_bytes())
        })
    }

    /// Extract a sub-grid by INTEGER cell ranges `[x0, x1) × [y0, y1)` (the
    /// grid-tiled export seam, spec §5). Ranges are half-open in cells; an
    /// adjacent sub-field whose `x0` equals this one's `x1` does NOT overlap,
    /// while one whose `x0` equals this one's `x1 - 1` SHARES that exact edge
    /// column. The tiler builds shared-edge ranges so seams are exact by integer
    /// cell identity — there is no projection or zoom drift like the geographic
    /// grid, the values are the same source cells.
    ///
    /// The carried [`FieldMeta`] is cloned verbatim (same pitch/scale/micro), so
    /// every sub-field meshes at the identical scale — the seam abuts. Debug-checks
    /// the ranges are in-bounds and non-empty (Rule 5/7).
    pub(crate) fn sub_field(&self, x0: u32, y0: u32, x1: u32, y1: u32) -> HeightField {
        debug_assert!(
            x0 < x1 && y0 < y1,
            "sub_field range must be non-empty: x[{x0},{x1}) y[{y0},{y1})",
        );
        debug_assert!(
            x1 <= self.width && y1 <= self.height,
            "sub_field range out of bounds: x[{x0},{x1}) y[{y0},{y1}) vs {}x{}",
            self.width,
            self.height,
        );
        let sw = x1 - x0;
        let sh = y1 - y0;
        let mut cells = Vec::with_capacity((sw as usize) * (sh as usize));
        for y in y0..y1 {
            let row = (y * self.width + x0) as usize;
            cells.extend_from_slice(&self.cells[row..row + sw as usize]);
        }
        HeightField { width: sw, height: sh, cells, meta: self.meta.clone() }
    }

    /// Bake a preview view-rotation into the grid: produce an axis-aligned copy of
    /// the field **as it appears rotated by `theta` radians** (CCW), so a feature
    /// the user lined up with screen-down exports along +Y — the brick slice axis.
    /// Output spans the rotated bounding box; height is bilinear-sampled; cells
    /// that fall outside the source rectangle become `FLOOR_M` (native ground → no
    /// bricks). `theta == 0` returns a verbatim clone (identity, byte-for-byte).
    ///
    /// LOSSY by design (resampling fuzzes hard edges / terrace plateaus) — the
    /// stored field is untouched; this runs only at export when `view_rot != 0`.
    pub(crate) fn rotated(&self, theta: f32) -> HeightField {
        if theta == 0.0 {
            return self.clone();
        }
        let (w, h) = (self.width as f32, self.height as f32);
        let (s, c) = theta.sin_cos();
        let (hw, hh) = (w * 0.5, h * 0.5);
        let (new_w, new_h) = rotated_dims(self.width, self.height, theta);
        let (oc_x, oc_y) = (new_w as f32 * 0.5, new_h as f32 * 0.5);
        let mut cells = Vec::with_capacity((new_w as usize) * (new_h as usize));
        for oy in 0..new_h {
            for ox in 0..new_w {
                // Centre the output cell, rotate back by R(-theta), re-centre on the
                // source. R(-θ)·(px,py) = (px·c + py·s, -px·s + py·c).
                let px = ox as f32 + 0.5 - oc_x;
                let py = oy as f32 + 0.5 - oc_y;
                let fx = (px * c + py * s) + hw - 0.5;
                let fy = (-px * s + py * c) + hh - 0.5;
                // Outside the source (beyond a half-cell margin) → floor.
                let v = if fx < -0.5 || fy < -0.5 || fx > w - 0.5 || fy > h - 0.5 {
                    FLOOR_M
                } else {
                    self.sample_bilinear(fx, fy)
                };
                cells.push(v);
            }
        }
        HeightField { width: new_w, height: new_h, cells, meta: self.meta.clone() }
    }
}

/// Axis-aligned `(width, height)` that hold the source `w×h` grid rotated by
/// `theta` radians. Shared by [`HeightField::rotated`], [`super::paint::PaintGrid::rotated`],
/// and the zone rotation so the height, its parallel paint grid, and the keep-mask
/// all land on the IDENTICAL rotated grid. The `-1e-3` trims f32 round-off so
/// axis-aligned angles (90°/180°/270°) give exact integer dims instead of +1.
pub(crate) fn rotated_dims(w: u32, h: u32, theta: f32) -> (u32, u32) {
    let (s, c) = theta.sin_cos();
    let (hw, hh) = (w as f32 * 0.5, h as f32 * 0.5);
    let nw = (2.0 * (hw * c.abs() + hh * s.abs()) - 1e-3).ceil().max(1.0);
    let nh = (2.0 * (hw * s.abs() + hh * c.abs()) - 1e-3).ceil().max(1.0);
    (nw as u32, nh as u32)
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

    /// Rotating by 0 is a verbatim identity; a 45° rotation grows the bounding
    /// box, preserves interior height, and floors the now-empty corners.
    #[test]
    fn rotated_zero_is_identity_and_45_preserves_interior() {
        let f = HeightField { width: 10, height: 10, cells: vec![5.0; 100], meta: test_meta() };
        let id = f.rotated(0.0);
        assert_eq!((id.width, id.height), (10, 10));
        assert_eq!(id.cells, f.cells, "θ=0 must be a byte-identical clone");

        let r = f.rotated(std::f32::consts::FRAC_PI_4);
        assert!(r.width >= 14 && r.height >= 14, "45° grows the bbox: {}×{}", r.width, r.height);
        let center = r.at(r.width / 2, r.height / 2);
        assert!((center - 5.0).abs() < 1e-3, "interior height preserved: {center}");
        assert_eq!(r.at(0, 0), FLOOR_M, "a corner outside the source rotates to floor");
    }

    /// A 90° rotation swaps the grid dimensions exactly.
    #[test]
    fn rotated_90_swaps_dims() {
        let f = HeightField { width: 6, height: 4, cells: vec![3.0; 24], meta: test_meta() };
        let r = f.rotated(std::f32::consts::FRAC_PI_2);
        assert_eq!((r.width, r.height), (4, 6), "90° swaps width/height");
    }

    /// AUDIT — slice↔rotation correlation. A ridge along source +X (a horizontal
    /// row) is PERPENDICULAR to the +Y slice axis, so at 0° it slices into many
    /// short bricks. Rotating the terrain +90° must stand it up along output +Y (a
    /// single tall column) so it slices into one long run — the whole point of
    /// "rotate to align with the slice lanes". This verifies the export half lands
    /// the feature on the slice axis the fixed screen-down overlay advertises.
    #[test]
    fn rotated_stands_a_cross_slice_ridge_up_along_the_slice_axis() {
        let (w, h) = (9u32, 9u32);
        let mut cells = vec![0.0f32; (w * h) as usize];
        for x in 0..w {
            cells[((h / 2) * w + x) as usize] = 10.0; // horizontal ridge (source +X)
        }
        let r = HeightField { width: w, height: h, cells, meta: test_meta() }
            .rotated(std::f32::consts::FRAC_PI_2);
        let mut col_high = vec![0u32; r.width as usize];
        for y in 0..r.height {
            for x in 0..r.width {
                if r.at(x, y) > 5.0 {
                    col_high[x as usize] += 1;
                }
            }
        }
        let total: u32 = col_high.iter().sum();
        let max_col = *col_high.iter().max().unwrap();
        assert!(total > 0, "ridge vanished under rotation");
        assert!(max_col >= 5, "ridge should stand up as a tall column after 90°, max {max_col}");
        assert!(
            max_col as f32 >= 0.6 * total as f32,
            "ridge mass must concentrate in one column (vertical, along +Y slice): {max_col}/{total}",
        );
    }

    /// AUDIT — rotation DIRECTION (no mirror). Display and export share `R(θ)`, so a
    /// +90° turn (`R(90°): (x,y)→(−y,x)`) must carry a source TOP-LEFT block to the
    /// output TOP-RIGHT — not bottom-left, which a sign flip / mirror would produce.
    /// If this fails, the baked export is mirrored vs what the user saw on screen.
    #[test]
    fn rotated_direction_matches_display_no_mirror() {
        let (w, h) = (9u32, 9u32);
        let mut cells = vec![0.0f32; (w * h) as usize];
        for y in 0..3 {
            for x in 0..3 {
                cells[(y * w + x) as usize] = 10.0; // source top-left block
            }
        }
        let r = HeightField { width: w, height: h, cells, meta: test_meta() }
            .rotated(std::f32::consts::FRAC_PI_2);
        let (mut sx, mut sy, mut n) = (0.0f32, 0.0f32, 0.0f32);
        for y in 0..r.height {
            for x in 0..r.width {
                if r.at(x, y) > 5.0 {
                    sx += x as f32;
                    sy += y as f32;
                    n += 1.0;
                }
            }
        }
        assert!(n > 0.0, "block vanished");
        let (cx, cy) = (sx / n, sy / n);
        let (mid_x, mid_y) = (r.width as f32 / 2.0, r.height as f32 / 2.0);
        assert!(cx > mid_x, "top-left must rotate to the RIGHT half at +90° (no mirror): cx={cx} mid={mid_x}");
        assert!(cy < mid_y, "top-left must stay in the TOP half at +90°: cy={cy} mid={mid_y}");
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
    fn sample_cell_meters_reads_nearest_cell() {
        // A field whose cells ARE meters above floor: the eyedropper sampler must
        // return the hovered cell's meters, flooring fractional pointer coords to
        // the cell and clamping off-grid/edge reads to the nearest cell.
        let mut f = HeightField::flat(3, 2, test_meta());
        f.set(0, 0, 0.0);
        f.set(1, 0, 10.0);
        f.set(2, 0, 20.0);
        f.set(0, 1, 30.0);
        f.set(1, 1, 40.0);
        f.set(2, 1, 50.0);

        // Integer-cell hits read that cell's meters exactly.
        assert_eq!(f.sample_cell_meters(0.0, 0.0), 0.0);
        assert_eq!(f.sample_cell_meters(2.0, 1.0), 50.0);
        // Fractional pointer inside a cell floors to that cell (nearest-cell).
        assert_eq!(f.sample_cell_meters(1.7, 0.2), 10.0, "x=1.7 floors to cell 1");
        assert_eq!(f.sample_cell_meters(1.9, 1.9), 40.0, "1.9,1.9 floors to cell (1,1)");
        // Off-grid (negative / beyond the last cell) clamps to the nearest cell.
        assert_eq!(f.sample_cell_meters(-5.0, -5.0), 0.0, "negative clamps to (0,0)");
        assert_eq!(f.sample_cell_meters(99.0, 99.0), 50.0, "beyond-edge clamps to (2,1)");
    }

    #[test]
    fn set_clamps_to_floor() {
        let mut f = HeightField::flat(2, 2, test_meta());
        f.set(0, 0, -50.0);
        assert_eq!(f.at(0, 0), FLOOR_M, "set must clamp below-floor values up to FLOOR_M");
        f.set(1, 1, 12.5);
        assert_eq!(f.at(1, 1), 12.5, "in-range set must store the value verbatim");
    }

    /// `sub_field` extracts an integer cell range verbatim, and two adjacent
    /// shared-edge sub-fields (the right one starting on the left one's last
    /// column) carry the EXACT same values in their shared column — the seam is
    /// exact by integer cell identity, no projection drift (spec §5).
    #[test]
    fn sub_field_shares_exact_edge_cells() {
        // A 6×4 field with a unique value per cell so any mismatch is detectable.
        let mut f = HeightField::flat(6, 4, test_meta());
        for y in 0..4u32 {
            for x in 0..6u32 {
                f.set(x, y, (y * 6 + x) as f32);
            }
        }

        // A sub-field is a verbatim copy of its cell window.
        let sub = f.sub_field(1, 1, 4, 3); // x in [1,4), y in [1,3) → 3×2
        assert_eq!((sub.width, sub.height), (3, 2), "sub-field dims = range size");
        for y in 0..2u32 {
            for x in 0..3u32 {
                assert_eq!(
                    sub.at(x, y),
                    f.at(x + 1, y + 1),
                    "sub_field cell ({x},{y}) must mirror source ({},{})",
                    x + 1,
                    y + 1,
                );
            }
        }
        // The carried meta is identical (uniform scale across tiles → abutting seams).
        assert_eq!(sub.meta, f.meta, "sub_field must clone the field meta verbatim");

        // Two horizontally adjacent SHARED-EDGE sub-fields: A = cols [0,4), B =
        // cols [3,6) (B begins on A's last column, index 3). Their shared column
        // (A's local x=3, B's local x=0) holds the SAME source cells for every row.
        let a = f.sub_field(0, 0, 4, 4);
        let b = f.sub_field(3, 0, 6, 4);
        let a_edge = a.width - 1; // 3
        for y in 0..4u32 {
            assert_eq!(
                a.at(a_edge, y),
                b.at(0, y),
                "shared edge column must be byte-identical from either neighbor (row {y})",
            );
            // …and it is the SAME source cell, column 3.
            assert_eq!(a.at(a_edge, y), f.at(3, y), "shared edge is source column 3");
        }
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
            omit_below_m: 0.0,
            floor_level_m: 0.0,
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

    /// The PNG export encodes each cell as `round(meters * HEIGHTMAP_PNG_SCALE)`
    /// in big-endian RGBA — the exact format Stage 1 (`geotiff2heightmap/map.py`)
    /// emits and `HeightmapPNG::new(.., rgba_encoded = true)` decodes, so a
    /// sculpted field round-trips losslessly through the CLI/map pipeline.
    #[test]
    fn to_heightmap_png_encodes_floor_relative_meters_as_big_endian_u32() {
        use crate::map::HeightmapPNG;

        let mut f = HeightField::flat(2, 1, test_meta());
        f.set(0, 0, 1.0); // 1.0 m  -> 100  -> [0,0,0,100]
        f.set(1, 0, 2.55); // 2.55 m -> 255  -> [0,0,0,255]
        let img = f.to_heightmap_png();
        assert_eq!(img.dimensions(), (2, 1), "image dims must match the field grid");
        assert_eq!(img.get_pixel(0, 0).0, [0, 0, 0, 100], "1.0 m * 100 = 100, big-endian");
        assert_eq!(img.get_pixel(1, 0).0, [0, 0, 0, 255], "2.55 m * 100 = 255, big-endian");

        // Round-trip through the real decoder: write the PNG, load it as an
        // rgba-encoded heightmap, and confirm each cell decodes to its scaled
        // integer height — proving the exported PNG is pipeline-compatible.
        let path = std::env::temp_dir()
            .join(format!("h2brz_export_test_{}.png", std::process::id()));
        img.save(&path).expect("export PNG must be writable");
        let decoded = HeightmapPNG::new(vec![&path], true).expect("exported PNG must load");
        assert_eq!(decoded.at(0, 0), 100, "decoder must recover 1.0 m * 100");
        assert_eq!(decoded.at(1, 0), 255, "decoder must recover 2.55 m * 100");
        let _ = std::fs::remove_file(path);
    }

    /// Terrace snaps heights to step multiples (accurate altitude, discrete
    /// plateaus); a non-positive step is a smooth no-op; floor stays at floor.
    #[test]
    fn terrace_snaps_to_steps() {
        // step 10: values land on the nearest 10 m plateau.
        assert_eq!(terrace_height(192.0, 10.0), 190.0);
        assert_eq!(terrace_height(196.0, 10.0), 200.0);
        assert_eq!(terrace_height(132.0, 50.0), 150.0);
        // accurate within step/2.
        assert!((terrace_height(132.0, 10.0) - 132.0).abs() <= 5.0);
        // step 0 / negative → smooth (identity); floor preserved.
        assert_eq!(terrace_height(123.4, 0.0), 123.4);
        assert_eq!(terrace_height(0.0, 25.0), 0.0);
        // never below floor (a sub-half-step sliver snaps to 0, not negative).
        assert!(terrace_height(3.0, 25.0) >= FLOOR_M);
    }

    /// Round-trip bug regression: a field exported with `to_heightmap_png` and
    /// re-imported with `from_heightmap_png` preserves every height (to 1/scale).
    /// And the OLD luminance path (`from_image` on the same PNG) crushes them —
    /// the bug the GUI importer hit (192 m → ~8 m).
    #[test]
    fn heightmap_png_round_trip_is_lossless() {
        let mut f = HeightField::flat(4, 3, test_meta());
        // Heights spanning the user's range, incl 132 m and 192 m.
        let vals = [0.0f32, 7.25, 132.0, 192.0, 50.5, 1.0, 337.0, 15.0, 0.0, 99.9, 200.0, 4.0];
        for (i, &v) in vals.iter().enumerate() {
            f.set((i as u32) % 4, (i as u32) / 4, v);
        }

        let png = f.to_heightmap_png();
        let rt = HeightField::from_heightmap_png(&png, test_meta());
        assert_eq!((rt.width, rt.height), (f.width, f.height), "dims preserved");
        for y in 0..f.height {
            for x in 0..f.width {
                assert!(
                    (rt.at(x, y) - f.at(x, y)).abs() < 0.01,
                    "({x},{y}) round-trip lost height: {} != {}",
                    rt.at(x, y),
                    f.at(x, y),
                );
            }
        }

        // The buggy path: reading our 4-channel PNG as 8-bit luminance crushes
        // 192 m to a single-digit value (proves why the importer must branch).
        let luma = image::DynamicImage::ImageRgba8(png).to_luma8();
        let crushed = HeightField::from_image(&luma, test_meta());
        assert!(
            crushed.at(3, 0) < 50.0,
            "luminance decode must visibly crush 192 m (got {}) — the bug",
            crushed.at(3, 0),
        );
    }
}
