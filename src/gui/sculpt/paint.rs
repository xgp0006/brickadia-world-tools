//! Splat paint: a per-cell palette-index grid the Sculpt tab paints onto, meshed
//! into brick colors via a [`Colormap`].
//!
//! The grid is a `Vec<u8>` of palette indices, one per height cell and the same
//! dimensions as the [`HeightField`](super::heightfield::HeightField). Index `0`
//! is the "unpainted" slot; its palette color defaults to the same flat color the
//! no-imagery sculpt build already uses, so an **all-zero grid is byte-identical**
//! to today's output (the same discipline the zone `keep_mask` follows).
//!
//! Color only this pass — per-cell material is a documented follow-up (it widens
//! the greedy mesher's `(height, color)` plane key; see the project plan).

use crate::gui::build::DEFAULT_BRICK_COLOR;
use crate::map::Colormap;

/// Maximum palette swatches (index 0 = unpainted + up to 15 painted layers). A
/// splatmap import maps the 4 RGBA channels onto slots 1..=4.
pub(crate) const MAX_SWATCHES: usize = 16;

/// Number of bands a height-gradient fill produces (palette slots `1..=N`); slot
/// 0 stays the unpainted default so the byte-identity guard holds.
pub(crate) const GRADIENT_BANDS: usize = 8;

/// Build a palette from a height ramp: slot 0 keeps the unpainted default, slots
/// `1..=GRADIENT_BANDS` sample `ramp(t)` at evenly-spaced stops. The ramp is
/// injected (the canvas's `hypsometric`) so the splat colors match the heatmap.
pub(crate) fn gradient_palette(ramp: impl Fn(f32) -> [u8; 3]) -> Vec<[u8; 4]> {
    let mut pal = Vec::with_capacity(GRADIENT_BANDS + 1);
    pal.push(DEFAULT_BRICK_COLOR);
    for i in 0..GRADIENT_BANDS {
        let t = if GRADIENT_BANDS <= 1 { 0.0 } else { i as f32 / (GRADIENT_BANDS as f32 - 1.0) };
        let c = ramp(t);
        pal.push([c[0], c[1], c[2], 0xFF]);
    }
    pal
}

/// Map a normalized height `t ∈ [0,1]` to a gradient band index `1..=GRADIENT_BANDS`.
pub(crate) fn gradient_band(t: f32) -> u8 {
    let t = t.clamp(0.0, 1.0);
    1 + (t * (GRADIENT_BANDS as f32 - 1.0)).round() as u8
}

/// The default palette: slot 0 is the unpainted color (matches `DEFAULT_BRICK_COLOR`
/// for byte-identity), followed by a few distinct starter swatches.
pub(crate) fn default_palette() -> Vec<[u8; 4]> {
    vec![
        DEFAULT_BRICK_COLOR,    // 0 — unpainted (must stay first for byte-identity)
        [0xC8, 0x5A, 0x3C, 0xFF], // 1 — terracotta
        [0x4F, 0x8A, 0x5B, 0xFF], // 2 — moss
        [0x3C, 0x6E, 0xA8, 0xFF], // 3 — water blue
        [0xD9, 0xC2, 0x7A, 0xFF], // 4 — sand
    ]
}

/// Per-cell palette-index grid, parallel to the height field. Resizes with the
/// field (a field swap reallocates it to the new dims, cleared to unpainted).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaintGrid {
    pub width: u32,
    pub height: u32,
    /// Row-major palette indices; `0` = unpainted.
    pub cells: Vec<u8>,
}

impl PaintGrid {
    /// A blank (all-unpainted) grid of the given dimensions.
    pub(crate) fn blank(width: u32, height: u32) -> Self {
        Self { width, height, cells: vec![0u8; (width as usize) * (height as usize)] }
    }

    /// True when nothing has been painted — every cell is the unpainted slot.
    /// The convert path uses this to skip the paint colormap entirely and stay
    /// byte-identical to a no-paint build.
    pub(crate) fn is_blank(&self) -> bool {
        self.cells.iter().all(|&i| i == 0)
    }

    #[inline]
    pub(crate) fn at(&self, x: u32, y: u32) -> u8 {
        self.cells[(y * self.width + x) as usize]
    }

    /// Fill the resolution block of side `r` containing cell `(x, y)` with `idx`,
    /// clamped to the grid edge. `r = 1` sets a single cell; larger `r` paints
    /// blocky R×R splat regions (coarser color = fewer brick color-splits).
    pub(crate) fn set_block(&mut self, x: u32, y: u32, r: u32, idx: u8) {
        let r = r.max(1);
        let bx = (x / r) * r;
        let by = (y / r) * r;
        for yy in by..(by + r).min(self.height) {
            for xx in bx..(bx + r).min(self.width) {
                self.cells[(yy * self.width + xx) as usize] = idx;
            }
        }
    }

    /// Paint every cell from a height gradient: each R×R block samples its
    /// top-left cell's height, normalizes it against `(min, max)`, and fills the
    /// block with the matching band (`1..=GRADIENT_BANDS`). Pairs with
    /// [`gradient_palette`] so the splat matches the canvas heatmap. `height_at`
    /// reads the field; the grid is left fully painted (no unpainted cells).
    pub(crate) fn fill_from_gradient(
        &mut self,
        r: u32,
        min: f32,
        max: f32,
        height_at: impl Fn(u32, u32) -> f32,
    ) {
        let r = r.max(1);
        let span = (max - min).max(1e-6);
        let mut by = 0;
        while by < self.height {
            let mut bx = 0;
            while bx < self.width {
                let t = ((height_at(bx, by) - min) / span).clamp(0.0, 1.0);
                self.set_block(bx, by, r, gradient_band(t));
                bx += r;
            }
            by += r;
        }
    }

    /// Paint-bucket flood fill from `(sx, sy)`: write `idx` into cells whose
    /// height is within `tolerance` meters of the start cell's height. `contiguous`
    /// = a 4-connected flood (fills the touched plateau/step only); otherwise a
    /// global fill (every matching cell, anywhere). Writes snap to the splat
    /// resolution `res`. Returns the matched-cell count.
    ///
    /// Bounded (Rule 2): each cell is enqueued at most once (the `visited` guard),
    /// so the BFS runs in ≤ `width × height` steps.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn flood_fill(
        &mut self,
        sx: u32,
        sy: u32,
        idx: u8,
        tolerance: f32,
        contiguous: bool,
        res: u32,
        height_at: impl Fn(u32, u32) -> f32,
    ) -> usize {
        if sx >= self.width || sy >= self.height {
            return 0;
        }
        let (w, h) = (self.width, self.height);
        let h0 = height_at(sx, sy);
        let matches = |x: u32, y: u32| (height_at(x, y) - h0).abs() <= tolerance;
        let mut filled = 0usize;

        if !contiguous {
            for y in 0..h {
                for x in 0..w {
                    if matches(x, y) {
                        self.set_block(x, y, res, idx);
                        filled += 1;
                    }
                }
            }
            return filled;
        }

        let mut visited = vec![false; (w as usize) * (h as usize)];
        let mut queue = std::collections::VecDeque::new();
        visited[(sy * w + sx) as usize] = true;
        queue.push_back((sx, sy));
        while let Some((x, y)) = queue.pop_front() {
            if !matches(x, y) {
                continue; // boundary cell: visited but not filled, stops the flood
            }
            self.set_block(x, y, res, idx);
            filled += 1;
            // 4-connected neighbors (u32 underflow at x=0/y=0 wraps huge → bounds-out).
            for (nx, ny) in [(x.wrapping_sub(1), y), (x + 1, y), (x, y.wrapping_sub(1)), (x, y + 1)] {
                if nx < w && ny < h {
                    let i = (ny * w + nx) as usize;
                    if !visited[i] {
                        visited[i] = true;
                        queue.push_back((nx, ny));
                    }
                }
            }
        }
        filled
    }

    /// Extract the row-major index window `[x0, x1) × [y0, y1)` — the per-tile
    /// slice the tiled convert needs, mirroring the zone keep-mask slicing so a
    /// painted region spanning a seam stays consistent across tiles.
    pub(crate) fn sub_window(&self, x0: u32, y0: u32, x1: u32, y1: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(((x1 - x0) * (y1 - y0)) as usize);
        for y in y0..y1 {
            for x in x0..x1 {
                out.push(self.at(x, y));
            }
        }
        out
    }
}

/// The paint grid + palette the convert path needs, borrowed together. Passed as
/// `Option` so a build with no paint layer stays on the existing flat-colormap
/// path (byte-identical).
#[derive(Clone, Copy)]
pub(crate) struct PaintLayer<'a> {
    pub grid: &'a PaintGrid,
    pub palette: &'a [[u8; 4]],
}

/// A [`Colormap`] view over a paint-index grid and palette. Resolves each cell to
/// its palette color; an out-of-range index falls back to the unpainted slot 0,
/// so a malformed grid can never panic the mesher.
pub(crate) struct PaintColormap<'a> {
    cells: &'a [u8],
    palette: &'a [[u8; 4]],
    width: u32,
    height: u32,
}

impl<'a> PaintColormap<'a> {
    /// Build a colormap over `cells` (row-major, `width × height`) resolved
    /// against `palette`. `palette` must be non-empty (slot 0 is the fallback).
    pub(crate) fn new(cells: &'a [u8], palette: &'a [[u8; 4]], width: u32, height: u32) -> Self {
        debug_assert!(!palette.is_empty(), "palette needs at least the unpainted slot");
        debug_assert_eq!(cells.len(), (width as usize) * (height as usize), "cells/dims mismatch");
        Self { cells, palette, width, height }
    }
}

impl Colormap for PaintColormap<'_> {
    fn at(&self, x: u32, y: u32) -> [u8; 4] {
        let idx = self.cells[(y * self.width + x) as usize] as usize;
        // Out-of-range index → unpainted slot 0 (never panics on a stale index).
        self.palette.get(idx).copied().unwrap_or(self.palette[0])
    }
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// Decode an RGBA splatmap into palette indices: each pixel's **dominant channel**
/// (argmax of R, G, B, A) selects a layer `1..=4`, the Unity/WorldPainter
/// channel-per-layer convention. The image is nearest-resampled to `dst_w × dst_h`
/// so it aligns with the height field regardless of its native size. A fully
/// transparent-and-black pixel (all channels 0) maps to unpainted (0).
///
/// Returns a row-major `Vec<u8>` of `dst_w × dst_h` palette indices.
pub(crate) fn splatmap_to_indices(
    img: &image::RgbaImage,
    dst_w: u32,
    dst_h: u32,
) -> Vec<u8> {
    let (sw, sh) = (img.width(), img.height());
    let mut out = Vec::with_capacity((dst_w as usize) * (dst_h as usize));
    for dy in 0..dst_h {
        for dx in 0..dst_w {
            // Nearest-neighbor sample (handles any source size; no filtering so
            // hard layer boundaries stay crisp).
            let sx = if dst_w <= 1 { 0 } else { dx * sw / dst_w };
            let sy = if dst_h <= 1 { 0 } else { dy * sh / dst_h };
            let sx = sx.min(sw.saturating_sub(1));
            let sy = sy.min(sh.saturating_sub(1));
            let p = img.get_pixel(sx, sy).0;
            out.push(dominant_channel(p));
        }
    }
    out
}

/// Index `1..=4` of the largest of the four RGBA channels, or `0` (unpainted)
/// when every channel is zero. Ties resolve to the lowest channel (R before G…).
fn dominant_channel(p: [u8; 4]) -> u8 {
    let max = p.iter().copied().max().unwrap_or(0);
    if max == 0 {
        return 0;
    }
    // First channel reaching the max wins; +1 because slot 0 is unpainted.
    (p.iter().position(|&c| c == max).unwrap_or(0) as u8) + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An all-unpainted grid resolves every cell to the palette's slot-0 color,
    /// which equals `DEFAULT_BRICK_COLOR` — the byte-identity guard: a no-paint
    /// build colors exactly as today's flat colormap.
    #[test]
    fn blank_grid_is_default_color() {
        let g = PaintGrid::blank(3, 2);
        assert!(g.is_blank());
        let pal = default_palette();
        let cm = PaintColormap::new(&g.cells, &pal, g.width, g.height);
        for y in 0..2 {
            for x in 0..3 {
                assert_eq!(cm.at(x, y), DEFAULT_BRICK_COLOR, "unpainted cell must be the flat color");
            }
        }
    }

    /// A painted cell resolves to its swatch; a stale out-of-range index falls
    /// back to slot 0 instead of panicking.
    #[test]
    fn painted_cell_uses_swatch_and_clamps() {
        let mut g = PaintGrid::blank(2, 1);
        g.set_block(0, 0, 1, 2);
        g.set_block(1, 0, 1, 99); // out of range
        let pal = default_palette();
        let cm = PaintColormap::new(&g.cells, &pal, g.width, g.height);
        assert_eq!(cm.at(0, 0), pal[2], "painted cell resolves to its swatch");
        assert_eq!(cm.at(1, 0), pal[0], "out-of-range index falls back to unpainted");
        assert!(!g.is_blank());
    }

    /// RGBA splatmap decode: each pixel's dominant channel selects layer 1..=4;
    /// an all-zero pixel maps to unpainted (0).
    #[test]
    fn splatmap_argmax_per_pixel() {
        // 2×2: red-dominant, green-dominant, blue-dominant, transparent-black.
        let mut img = image::RgbaImage::new(2, 2);
        img.put_pixel(0, 0, image::Rgba([200, 10, 10, 0]));
        img.put_pixel(1, 0, image::Rgba([10, 200, 10, 0]));
        img.put_pixel(0, 1, image::Rgba([10, 10, 200, 0]));
        img.put_pixel(1, 1, image::Rgba([0, 0, 0, 0]));
        let idx = splatmap_to_indices(&img, 2, 2);
        assert_eq!(idx, vec![1, 2, 3, 0], "R→1, G→2, B→3, zero→0");
    }

    /// Decode resamples to the destination dimensions (nearest), so a small
    /// splatmap aligns with a larger field.
    #[test]
    fn splatmap_resamples_to_field() {
        let mut img = image::RgbaImage::new(1, 1);
        img.put_pixel(0, 0, image::Rgba([0, 0, 255, 255])); // alpha-dominant? no: blue=255,alpha=255 tie → blue(3)
        let idx = splatmap_to_indices(&img, 4, 4);
        assert_eq!(idx.len(), 16, "decode fills the destination grid");
        assert!(idx.iter().all(|&i| i == idx[0]), "a 1×1 source paints uniformly");
    }

    /// Gradient palette + band mapping: slot 0 stays the unpainted default; the
    /// lowest height maps to band 1, the highest to band GRADIENT_BANDS, and the
    /// palette has one color per band.
    #[test]
    fn gradient_palette_and_bands() {
        let pal = gradient_palette(|t| {
            // A simple black→white ramp so the test is independent of hypsometric.
            let v = (t * 255.0) as u8;
            [v, v, v]
        });
        assert_eq!(pal.len(), GRADIENT_BANDS + 1, "slot 0 + one per band");
        assert_eq!(pal[0], DEFAULT_BRICK_COLOR, "slot 0 stays unpainted default");
        assert_eq!(pal[1], [0, 0, 0, 0xFF], "band 1 = ramp(0)");
        assert_eq!(pal[GRADIENT_BANDS], [255, 255, 255, 0xFF], "top band = ramp(1)");

        assert_eq!(gradient_band(0.0), 1, "floor → band 1");
        assert_eq!(gradient_band(1.0), GRADIENT_BANDS as u8, "peak → top band");
        assert!((1..=GRADIENT_BANDS as u8).contains(&gradient_band(0.5)));
    }

    /// Resolution blocks: `set_block` fills the whole R×R cell containing a point,
    /// and a gradient fill leaves every cell painted with a low→high band ramp.
    #[test]
    fn block_paint_and_gradient_fill() {
        let mut g = PaintGrid::blank(4, 4);
        g.set_block(1, 1, 2, 7); // block (0..2, 0..2) → the top-left 2×2
        assert_eq!(g.at(0, 0), 7);
        assert_eq!(g.at(1, 1), 7);
        assert_eq!(g.at(2, 0), 0, "neighboring block untouched");

        // A height ramp increasing with y: low rows → low bands, high rows → high.
        let mut grid = PaintGrid::blank(4, 4);
        grid.fill_from_gradient(1, 0.0, 3.0, |_x, y| y as f32);
        assert!(!grid.is_blank(), "gradient fill paints every cell");
        assert_eq!(grid.at(0, 0), 1, "lowest row → band 1");
        assert_eq!(grid.at(0, 3), GRADIENT_BANDS as u8, "highest row → top band");
        assert!(grid.cells.iter().all(|&i| i >= 1), "no cell left unpainted");
    }

    /// Bucket fill: contiguous flood fills only the touched region within
    /// tolerance; global fills every matching cell. A wall of different height
    /// stops the contiguous flood but not the global one.
    #[test]
    fn flood_fill_contiguous_vs_global() {
        // Heights: two 2-wide low strips (h=0) split by a tall wall column (h=100).
        // Layout per row (x=0..5): 0 0 100 0 0  → left region, wall, right region.
        let w = 5u32;
        let hgt = |x: u32, _y: u32| if x == 2 { 100.0 } else { 0.0 };

        // Contiguous from the left region (x=0): fills only x=0,1 across all rows,
        // never crossing the wall to x=3,4.
        let mut g = PaintGrid::blank(w, 3);
        let n = g.flood_fill(0, 0, 5, 1.0, true, 1, hgt);
        assert_eq!(n, 6, "contiguous fill covers the 2×3 left region only");
        assert_eq!(g.at(0, 0), 5);
        assert_eq!(g.at(1, 2), 5);
        assert_eq!(g.at(2, 0), 0, "wall not filled");
        assert_eq!(g.at(3, 0), 0, "right region not reached across the wall");

        // Global from the same start: every low cell (both regions) fills.
        let mut gg = PaintGrid::blank(w, 3);
        let ng = gg.flood_fill(0, 0, 7, 1.0, false, 1, hgt);
        assert_eq!(ng, 12, "global fill covers all 4 low columns × 3 rows");
        assert_eq!(gg.at(3, 1), 7, "right region filled globally");
        assert_eq!(gg.at(2, 0), 0, "wall (out of tolerance) still excluded");
    }

    /// `sub_window` extracts the row-major tile slice the tiled convert needs.
    #[test]
    fn sub_window_slices_rowmajor() {
        let mut g = PaintGrid::blank(4, 3);
        g.set_block(1, 1, 1, 5);
        g.set_block(2, 1, 1, 6);
        // Window [1,3) × [1,2) → the two painted cells, row-major.
        assert_eq!(g.sub_window(1, 1, 3, 2), vec![5, 6]);
    }
}
