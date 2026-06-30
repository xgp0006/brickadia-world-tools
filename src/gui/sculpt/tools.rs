//! Sculpt tools: the brush-driven height operations.
//!
//! Every tool implements [`Tool`] — the seam future color/scatter tools plug
//! into — and edits a [`HeightField`] one *dab* at a time. A dab touches only
//! the bounded `[cx-r, cx+r] × [cy-r, cy+r]` sub-rect around its center
//! (`O(radius²)`), is deterministic, and clamps every cell to `>= FLOOR_M` (the
//! native-floor invariant: you can only build up).
//!
//! Smooth reads a snapshot of the affected rect before writing so each cell
//! blends toward the *pre-dab* neighborhood mean — the edit is independent of
//! row-major write order, which keeps it deterministic and order-free.

use super::brush::{shape_distance, weight, Brush};
use super::heightfield::HeightField;

/// A single height operation applied as a dab. Implementors edit `f` in place
/// over the brush's bounded footprint. This trait is the extension seam: future
/// color-paint and prefab-scatter tools implement it without changing callers.
pub(crate) trait Tool {
    /// Apply one dab of this tool to `f` centered at fractional cell coordinate
    /// `center`, shaped by `brush`. Touches only the brush's sub-rect; clamps
    /// `>= FLOOR_M`.
    fn apply(&self, f: &mut HeightField, brush: &Brush, center: (f32, f32));
}

/// The integer cell sub-rect a dab of radius `r` centered at `(cx, cy)` covers,
/// clamped to the field bounds. Returns `None` if the rect is empty (entirely
/// off the field, or non-positive radius). Bounds are inclusive `[x0, x1] ×
/// [y0, y1]`.
///
/// This is the single source of truth for "which cells a dab can touch" — both
/// the tools and the `dab_touches_only_affected_rect` test reason about it, so
/// the bound can never drift between application and verification.
fn dab_rect(f: &HeightField, center: (f32, f32), r: f32) -> Option<(u32, u32, u32, u32)> {
    // Non-positive or NaN radius → no rect (NaN-safe: NaN is neither > 0 nor a
    // valid span).
    if r <= 0.0 || r.is_nan() || f.width == 0 || f.height == 0 {
        return None;
    }
    let (cx, cy) = center;
    // Floor/ceil the real-valued ±r span to integer cells, then clamp to the
    // grid. A cell at the very edge of the radius is included; `weight` zeroes
    // anything genuinely at/beyond the rim, so over-including by a cell is safe.
    let min_x = (cx - r).floor();
    let max_x = (cx + r).ceil();
    let min_y = (cy - r).floor();
    let max_y = (cy + r).ceil();

    // Entirely off the grid in either axis → empty.
    let last_x = (f.width - 1) as f32;
    let last_y = (f.height - 1) as f32;
    if max_x < 0.0 || min_x > last_x || max_y < 0.0 || min_y > last_y {
        return None;
    }

    let x0 = min_x.max(0.0) as u32;
    let x1 = (max_x.min(last_x)) as u32;
    let y0 = min_y.max(0.0) as u32;
    let y1 = (max_y.min(last_y)) as u32;
    Some((x0, x1, y0, y1))
}

/// Influence weight of a dab at cell `(x, y)`: the brush falloff evaluated on the
/// shape's normalized distance from `center` (see [`shape_distance`]). Returns
/// `0.0` for cells outside the brush footprint, whatever its shape. Pure; no
/// field mutation.
fn cell_weight(brush: &Brush, center: (f32, f32), x: u32, y: u32) -> f32 {
    let (cx, cy) = center;
    let dx = x as f32 - cx;
    let dy = y as f32 - cy;
    // `shape_distance` returns +∞ for a non-positive/NaN radius; `weight` clamps
    // that to a zero influence, so the kernel stays total without a guard here.
    weight(brush.falloff, shape_distance(brush.shape, dx, dy, brush.radius_cells))
}

/// Raise: push cells **up** by `strength * weight` per dab.
pub(crate) struct Raise;
impl Tool for Raise {
    fn apply(&self, f: &mut HeightField, brush: &Brush, center: (f32, f32)) {
        let Some((x0, x1, y0, y1)) = dab_rect(f, center, brush.radius_cells) else {
            return;
        };
        for y in y0..=y1 {
            for x in x0..=x1 {
                let w = cell_weight(brush, center, x, y);
                if w <= 0.0 {
                    continue;
                }
                let v = f.at(x, y) + brush.strength * w;
                f.set(x, y, v); // set clamps >= FLOOR_M
            }
        }
    }
}

/// Lower: push cells **down** by `strength * weight` per dab, bottoming out at
/// the floor.
pub(crate) struct Lower;
impl Tool for Lower {
    fn apply(&self, f: &mut HeightField, brush: &Brush, center: (f32, f32)) {
        let Some((x0, x1, y0, y1)) = dab_rect(f, center, brush.radius_cells) else {
            return;
        };
        for y in y0..=y1 {
            for x in x0..=x1 {
                let w = cell_weight(brush, center, x, y);
                if w <= 0.0 {
                    continue;
                }
                let v = f.at(x, y) - brush.strength * w;
                f.set(x, y, v); // set clamps >= FLOOR_M (never carves below floor)
            }
        }
    }
}

thread_local! {
    /// Reused scratch buffer for [`Smooth::apply`]'s two passes. `apply_dab`
    /// constructs a fresh `Smooth` per dab, so a struct-carried buffer would
    /// realloc every dab; a thread-local survives across dabs and the GUI is
    /// single-threaded. Holds `(x, y, blended_value)` for one dab, cleared (not
    /// freed) between dabs so a fast stroke reuses its capacity after the first.
    static SMOOTH_SCRATCH: std::cell::RefCell<Vec<(u32, u32, f32)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Smooth: blend each cell toward its local 3×3 neighborhood mean by
/// `strength * weight`. `strength` is a `0..=1` blend factor.
///
/// Two passes over the dab rect keep the edit order-free and deterministic:
/// the first pass reads each target cell's neighborhood mean from the
/// *unmodified* field (no writes) and records the blended result; the second
/// pass applies all those writes. Because every mean is read before any write,
/// a cell's result never depends on an earlier dab-cell's new value, so the
/// outcome is independent of row-major iteration order.
pub(crate) struct Smooth;
impl Tool for Smooth {
    fn apply(&self, f: &mut HeightField, brush: &Brush, center: (f32, f32)) {
        let Some((x0, x1, y0, y1)) = dab_rect(f, center, brush.radius_cells) else {
            return;
        };
        SMOOTH_SCRATCH.with_borrow_mut(|blended| {
            // Pass 1: read every target mean from the unmodified field into the
            // reused scratch (no writes), so later writes can't perturb an
            // earlier cell's neighborhood read.
            blended.clear();
            for y in y0..=y1 {
                for x in x0..=x1 {
                    let w = cell_weight(brush, center, x, y);
                    if w <= 0.0 {
                        continue;
                    }
                    let mean = neighborhood_mean(f, x, y);
                    let cur = f.at(x, y);
                    // Blend factor is strength*weight, clamped to [0,1] so we
                    // never overshoot past the mean (keeps smoothing
                    // variance-reducing).
                    let blend = (brush.strength * w).clamp(0.0, 1.0);
                    let v = cur + (mean - cur) * blend;
                    blended.push((x, y, v));
                }
            }
            // Pass 2: apply the recorded writes.
            for &(x, y, v) in blended.iter() {
                f.set(x, y, v); // set clamps >= FLOOR_M
            }
        });
    }
}

/// Mean of the 3×3 neighborhood centered on `(x, y)`, edge cells clamped into
/// bounds (so border cells average against their nearest in-bounds neighbors).
fn neighborhood_mean(f: &HeightField, x: u32, y: u32) -> f32 {
    let last_x = f.width - 1;
    let last_y = f.height - 1;
    let mut sum = 0.0f32;
    // Fixed 3×3 window: exactly 9 reads, bounded.
    for dy in -1i32..=1 {
        for dx in -1i32..=1 {
            let nx = (x as i32 + dx).clamp(0, last_x as i32) as u32;
            let ny = (y as i32 + dy).clamp(0, last_y as i32) as u32;
            sum += f.at(nx, ny);
        }
    }
    sum / 9.0
}

/// Flatten / Level: pull cells toward a fixed `target` height by
/// `strength * weight`. `strength` is a `0..=1` blend factor; each dab moves a
/// cell a fraction of the way to `target`, never past it.
pub(crate) struct Flatten {
    pub target: f32,
}
impl Tool for Flatten {
    fn apply(&self, f: &mut HeightField, brush: &Brush, center: (f32, f32)) {
        let Some((x0, x1, y0, y1)) = dab_rect(f, center, brush.radius_cells) else {
            return;
        };
        for y in y0..=y1 {
            for x in x0..=x1 {
                let w = cell_weight(brush, center, x, y);
                if w <= 0.0 {
                    continue;
                }
                let cur = f.at(x, y);
                let blend = (brush.strength * w).clamp(0.0, 1.0);
                let v = cur + (self.target - cur) * blend;
                f.set(x, y, v); // set clamps >= FLOOR_M
            }
        }
    }
}

/// Set-height: drive cells toward an absolute `target` by `strength * weight`.
///
/// Mechanically identical to [`Flatten`] (a fractional pull toward a target);
/// kept a distinct type so the UI tool selector and any future divergence (e.g.
/// hard-set vs. eased) have a stable name to dispatch on.
pub(crate) struct SetHeight {
    pub target: f32,
}
impl Tool for SetHeight {
    fn apply(&self, f: &mut HeightField, brush: &Brush, center: (f32, f32)) {
        let Some((x0, x1, y0, y1)) = dab_rect(f, center, brush.radius_cells) else {
            return;
        };
        for y in y0..=y1 {
            for x in x0..=x1 {
                let w = cell_weight(brush, center, x, y);
                if w <= 0.0 {
                    continue;
                }
                let cur = f.at(x, y);
                let blend = (brush.strength * w).clamp(0.0, 1.0);
                let v = cur + (self.target - cur) * blend;
                f.set(x, y, v); // set clamps >= FLOOR_M
            }
        }
    }
}

/// A parametric terrain primitive: which silhouette the [`Stamp`] tool deposits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StampKind {
    /// Peak at center, linear slope to zero at the footprint edge.
    Cone,
    /// Flat top out to `inner_ratio`, then eased shoulder down to the edge.
    Mesa,
    /// Sunken bowl with a raised rim near `inner_ratio`, fading out past it.
    Crater,
    /// Linear gradient (0 → 1) along `angle`, bounded by the footprint.
    Ramp,
}

impl StampKind {
    pub(crate) const ALL: [StampKind; 4] =
        [StampKind::Cone, StampKind::Mesa, StampKind::Crater, StampKind::Ramp];

    /// Full tooltip: the landform this primitive stamps and the knob that shapes
    /// it (also carries the name, e.g. "Cone — …").
    pub(crate) fn help(self) -> &'static str {
        match self {
            StampKind::Cone => "Cone — sharp peak at center sloping straight to the edge. A simple hill or mountain.",
            StampKind::Mesa => {
                "Mesa — flat top with an eased shoulder down to the edge. Plateaus and table-mountains; \
                 Inner ratio sets the flat width."
            }
            StampKind::Crater => {
                "Crater — sunken bowl with a raised rim. Impact craters and calderas; \
                 Inner ratio positions the rim."
            }
            StampKind::Ramp => "Ramp — straight linear slope along the angle. Roads, embankments, and access ramps.",
        }
    }
}

/// Normalized height profile in roughly `[-1, 1]` for a stamp, evaluated at a
/// footprint distance `d ∈ [0, 1]` (0 = center, 1 = edge). `ramp_proj` is the
/// signed projection of the cell onto the ramp direction, normalized to
/// `[-1, 1]` across the radius (only used by [`StampKind::Ramp`]). `inner_ratio`
/// is clamped to a safe interior so divides never blow up.
///
/// Profiles vanish (→ 0) at `d = 1` for Cone/Mesa/Crater so the silhouette
/// blends into existing terrain with no wall; Ramp keeps its edge (a ramp is a
/// wall by nature).
fn stamp_profile(kind: StampKind, d: f32, inner_ratio: f32, ramp_proj: f32) -> f32 {
    let d = d.clamp(0.0, 1.0);
    let inner = inner_ratio.clamp(0.05, 0.95);
    match kind {
        StampKind::Cone => 1.0 - d,
        StampKind::Mesa => {
            if d <= inner {
                1.0
            } else {
                // Eased shoulder: 1 at the plateau rim → 0 at the footprint edge.
                let u = (d - inner) / (1.0 - inner);
                let t = 1.0 - u; // invert so smoothstep gives 1 at inner, 0 at edge
                t * t * (3.0 - 2.0 * t)
            }
        }
        StampKind::Crater => {
            if d <= inner {
                // Pit at center (-1) rising to the rim (+1) at `inner`.
                -(std::f32::consts::PI * d / inner).cos()
            } else {
                // Rim (+1) fading to 0 at the edge.
                (1.0 - d) / (1.0 - inner)
            }
        }
        StampKind::Ramp => 0.5 + 0.5 * ramp_proj.clamp(-1.0, 1.0),
    }
}

/// Stamp a terrain primitive: deposit `peak_m * profile(d)` onto every cell
/// inside the brush footprint, where the profile is chosen by `kind` (see
/// [`stamp_profile`]). Respects the brush's *shape* (a square mesa, hex cone…)
/// via [`shape_distance`], and its `radius_cells` for the footprint extent.
/// Additive on top of existing terrain; `set` clamps the result `>= FLOOR_M`,
/// so a crater pit or negative `peak_m` can carve down to — never below — floor.
pub(crate) struct Stamp {
    pub kind: StampKind,
    pub peak_m: f32,
    pub inner_ratio: f32,
    /// Ramp direction in radians; ignored by the radially-symmetric kinds.
    pub angle: f32,
}

impl Tool for Stamp {
    fn apply(&self, f: &mut HeightField, brush: &Brush, center: (f32, f32)) {
        let r = brush.radius_cells;
        let Some((x0, x1, y0, y1)) = dab_rect(f, center, r) else {
            return;
        };
        let (cx, cy) = center;
        let (dirx, diry) = (self.angle.cos(), self.angle.sin());
        for y in y0..=y1 {
            for x in x0..=x1 {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                // Footprint membership uses the brush shape's metric; cells at or
                // past the rim (d >= 1) get nothing.
                let d = shape_distance(brush.shape, dx, dy, r);
                if d >= 1.0 {
                    continue;
                }
                let proj = if r > 0.0 { (dx * dirx + dy * diry) / r } else { 0.0 };
                let profile = stamp_profile(self.kind, d, self.inner_ratio, proj);
                let v = f.at(x, y) + self.peak_m * profile;
                f.set(x, y, v); // clamps >= FLOOR_M
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::brush::{BrushShape, Falloff};
    use super::super::heightfield::{FieldMeta, FLOOR_M};

    fn test_meta() -> FieldMeta {
        FieldMeta {
            cell_m: 30.0,
            studs_per_meter: 1.0,
            vertical_exaggeration: 1.0,
            micro: false,
            centroid_lat: 0.0,
            source_name: "tool-test".to_string(),
        }
    }

    fn circle(radius_cells: f32, strength: f32, falloff: Falloff) -> Brush {
        Brush { shape: BrushShape::Circle, radius_cells, strength, falloff }
    }

    /// Raise lifts cells inside the radius and leaves every cell outside it
    /// exactly unchanged.
    #[test]
    fn raise_bounded_within_radius() {
        let mut f = HeightField::flat(21, 21, test_meta());
        let before = f.clone();
        let r = 4.0;
        let center = (10.0, 10.0);
        Raise.apply(&mut f, &circle(r, 5.0, Falloff::Smoothstep), center);

        for y in 0..f.height {
            for x in 0..f.width {
                let dx = x as f32 - center.0;
                let dy = y as f32 - center.1;
                let dist = (dx * dx + dy * dy).sqrt();
                let v = f.at(x, y);
                if dist < r - 1e-3 {
                    // Strictly inside: weight > 0, must have risen.
                    assert!(v > before.at(x, y), "cell ({x},{y}) inside radius did not rise: {v}");
                } else if dist > r + 1e-3 {
                    // Strictly outside the radius: zero change, bit-exact.
                    assert_eq!(
                        v,
                        before.at(x, y),
                        "cell ({x},{y}) outside radius changed: {v}",
                    );
                }
            }
        }
        // The very center got the full strength (weight ~1).
        assert!((f.at(10, 10) - 5.0).abs() < 1e-4, "center should gain ~full strength");
    }

    /// Footprint shape selects which cells a dab touches: a Square raises the
    /// box corner a Circle of the same radius leaves untouched, and a Diamond
    /// excludes a diagonal cell the Square keeps.
    #[test]
    fn footprint_shape_selects_cells() {
        let center = (10.0, 10.0);
        let r = 5.0;
        // Box corner of the ±r rect, just inside it.
        let (cx, cy) = (14u32, 14u32); // dx=dy=4 < r=5 → inside the square, outside the circle
        let brush = |shape| Brush { shape, radius_cells: r, strength: 5.0, falloff: Falloff::Constant };

        let mut sq = HeightField::flat(21, 21, test_meta());
        Raise.apply(&mut sq, &brush(BrushShape::Square), center);
        assert!(sq.at(cx, cy) > 0.0, "square footprint must raise the box corner");

        let mut ci = HeightField::flat(21, 21, test_meta());
        Raise.apply(&mut ci, &brush(BrushShape::Circle), center);
        assert_eq!(ci.at(cx, cy), 0.0, "circle of equal radius must not reach the box corner");

        // Diamond excludes a diagonal cell (dx=dy=3 → manhattan 6 > r) the Square keeps.
        let (dx, dy) = (13u32, 13u32);
        let mut di = HeightField::flat(21, 21, test_meta());
        Raise.apply(&mut di, &brush(BrushShape::Diamond), center);
        assert_eq!(di.at(dx, dy), 0.0, "diamond must exclude the diagonal cell");
        assert!(sq.at(dx, dy) > 0.0, "square must include the diagonal cell");
    }

    /// A dab leaves every cell outside its bounded rect untouched (bit-exact),
    /// independent of the circular weighting.
    #[test]
    fn dab_touches_only_affected_rect() {
        let mut f = HeightField::flat(30, 30, test_meta());
        // Seed varied values so an accidental write outside the rect is visible.
        for y in 0..f.height {
            for x in 0..f.width {
                f.set(x, y, (x as f32) + (y as f32) * 0.5);
            }
        }
        let before = f.clone();
        let center = (8.0, 20.0);
        let r = 3.0;
        Raise.apply(&mut f, &circle(r, 7.0, Falloff::Constant), center);

        let rect = dab_rect(&before, center, r).expect("dab must cover a rect");
        let (x0, x1, y0, y1) = rect;
        for y in 0..f.height {
            for x in 0..f.width {
                let in_rect = x >= x0 && x <= x1 && y >= y0 && y <= y1;
                if !in_rect {
                    assert_eq!(
                        f.at(x, y),
                        before.at(x, y),
                        "cell ({x},{y}) outside the dab rect [{x0}..{x1}]x[{y0}..{y1}] changed",
                    );
                }
            }
        }
    }

    /// Smoothing a noisy patch lowers its variance.
    #[test]
    fn smooth_reduces_local_variance() {
        let mut f = HeightField::flat(15, 15, test_meta());
        // A checkerboard of 0 / 40 — maximal local variance.
        for y in 0..f.height {
            for x in 0..f.width {
                let v = if (x + y) % 2 == 0 { 0.0 } else { 40.0 };
                f.set(x, y, v);
            }
        }
        let center = (7.0, 7.0);
        let r = 5.0;

        let var_before = patch_variance(&f, center, r);
        // Strong constant-falloff smooth so the whole patch blends hard.
        Smooth.apply(&mut f, &circle(r, 1.0, Falloff::Constant), center);
        let var_after = patch_variance(&f, center, r);

        assert!(
            var_after < var_before,
            "smoothing must reduce variance: before {var_before}, after {var_after}",
        );
    }

    /// Variance of cells whose center is within `r` of `center`.
    fn patch_variance(f: &HeightField, center: (f32, f32), r: f32) -> f32 {
        let mut vals = Vec::new();
        for y in 0..f.height {
            for x in 0..f.width {
                let dx = x as f32 - center.0;
                let dy = y as f32 - center.1;
                if (dx * dx + dy * dy).sqrt() <= r {
                    vals.push(f.at(x, y));
                }
            }
        }
        let n = vals.len() as f32;
        let mean = vals.iter().sum::<f32>() / n;
        vals.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / n
    }

    /// Each Flatten dab moves cells toward the target and never past it.
    #[test]
    fn flatten_moves_monotonically_toward_target() {
        let mut f = HeightField::flat(15, 15, test_meta());
        for y in 0..f.height {
            for x in 0..f.width {
                f.set(x, y, (x as f32) * 3.0); // a ramp, all above and below target
            }
        }
        let target = 20.0;
        let center = (7.0, 7.0);
        let r = 6.0;
        let brush = circle(r, 0.4, Falloff::Linear);

        // Apply several dabs; after each, every in-radius cell is no farther from
        // the target than before and never crosses it.
        let mut prev = f.clone();
        for _ in 0..5 {
            Flatten { target }.apply(&mut f, &brush, center);
            for y in 0..f.height {
                for x in 0..f.width {
                    let dx = x as f32 - center.0;
                    let dy = y as f32 - center.1;
                    if (dx * dx + dy * dy).sqrt() > r {
                        continue;
                    }
                    let was = prev.at(x, y);
                    let now = f.at(x, y);
                    // Distance to target is non-increasing.
                    assert!(
                        (now - target).abs() <= (was - target).abs() + 1e-4,
                        "cell ({x},{y}) moved away from target: {was} -> {now}",
                    );
                    // Never overshoot: stays on the same side of (or reaches) target.
                    if was > target {
                        assert!(now >= target - 1e-4, "cell ({x},{y}) overshot below target");
                    } else if was < target {
                        assert!(now <= target + 1e-4, "cell ({x},{y}) overshot above target");
                    }
                }
            }
            prev = f.clone();
        }
    }

    /// Lower, Flatten, and Set-height on a near-floor field never produce a cell
    /// below `FLOOR_M`.
    #[test]
    fn brush_never_below_floor() {
        let center = (10.0, 10.0);
        let r = 6.0;

        // Lower a near-floor field hard.
        let mut f = HeightField::flat(21, 21, test_meta());
        for y in 0..f.height {
            for x in 0..f.width {
                f.set(x, y, 0.5); // just above floor
            }
        }
        for _ in 0..10 {
            Lower.apply(&mut f, &circle(r, 5.0, Falloff::Constant), center);
        }
        assert_floor(&f);

        // Flatten toward a below-floor target (clamps up to floor).
        let mut f2 = HeightField::flat(21, 21, test_meta());
        for y in 0..f2.height {
            for x in 0..f2.width {
                f2.set(x, y, 3.0);
            }
        }
        for _ in 0..10 {
            Flatten { target: -100.0 }.apply(&mut f2, &circle(r, 0.9, Falloff::Constant), center);
        }
        assert_floor(&f2);

        // Set toward a below-floor target.
        let mut f3 = HeightField::flat(21, 21, test_meta());
        for y in 0..f3.height {
            for x in 0..f3.width {
                f3.set(x, y, 7.0);
            }
        }
        for _ in 0..10 {
            SetHeight { target: -50.0 }.apply(&mut f3, &circle(r, 0.9, Falloff::Constant), center);
        }
        assert_floor(&f3);
    }

    fn assert_floor(f: &HeightField) {
        for (i, &c) in f.cells.iter().enumerate() {
            assert!(c >= FLOOR_M, "cell {i} fell below floor: {c}");
        }
    }

    /// Stamp profiles: cone peaks at center and vanishes at the edge; mesa is
    /// flat (==1) out to inner_ratio then vanishes; crater pits at center and
    /// rises to a rim.
    #[test]
    fn stamp_profiles_shape() {
        // Cone: 1 at center, ~0 at edge, monotonic down.
        assert!((stamp_profile(StampKind::Cone, 0.0, 0.5, 0.0) - 1.0).abs() < 1e-6);
        assert!(stamp_profile(StampKind::Cone, 1.0, 0.5, 0.0).abs() < 1e-6);
        assert!(stamp_profile(StampKind::Cone, 0.3, 0.5, 0.0) > stamp_profile(StampKind::Cone, 0.7, 0.5, 0.0));

        // Mesa: flat top within inner_ratio, zero at the edge.
        let inner = 0.4;
        assert!((stamp_profile(StampKind::Mesa, 0.0, inner, 0.0) - 1.0).abs() < 1e-6);
        assert!((stamp_profile(StampKind::Mesa, inner * 0.5, inner, 0.0) - 1.0).abs() < 1e-6);
        assert!(stamp_profile(StampKind::Mesa, 1.0, inner, 0.0).abs() < 1e-6);

        // Crater: negative at center (pit), positive at the rim (== inner_ratio).
        assert!(stamp_profile(StampKind::Crater, 0.0, 0.5, 0.0) < 0.0);
        assert!((stamp_profile(StampKind::Crater, 0.5, 0.5, 0.0) - 1.0).abs() < 1e-5);

        // Ramp: 0 at the low edge, 1 at the high edge.
        assert!(stamp_profile(StampKind::Ramp, 0.5, 0.5, -1.0).abs() < 1e-6);
        assert!((stamp_profile(StampKind::Ramp, 0.5, 0.5, 1.0) - 1.0).abs() < 1e-6);
    }

    /// A cone Stamp raises the center to ~peak, tapers outward, leaves cells past
    /// the footprint untouched, and never drops a cell below the floor.
    #[test]
    fn stamp_cone_deposits_peak() {
        let mut f = HeightField::flat(31, 31, test_meta());
        let center = (15.0, 15.0);
        let r = 8.0;
        let brush = Brush { shape: BrushShape::Circle, radius_cells: r, strength: 0.0, falloff: Falloff::Constant };
        Stamp { kind: StampKind::Cone, peak_m: 50.0, inner_ratio: 0.5, angle: 0.0 }.apply(&mut f, &brush, center);

        assert!((f.at(15, 15) - 50.0).abs() < 1e-3, "cone center should reach peak");
        // A mid-radius cell is raised but below the peak.
        let mid = f.at(19, 15);
        assert!(mid > 0.0 && mid < 50.0, "cone should taper: {mid}");
        // Outside the footprint: untouched.
        assert_eq!(f.at(0, 0), 0.0, "cell outside footprint must stay at floor");
        for &c in f.cells.iter() {
            assert!(c >= FLOOR_M, "stamp must not breach floor");
        }
    }

    /// Same (op, brush, center, field) → bit-identical result, for every tool.
    #[test]
    fn apply_is_deterministic() {
        let base = {
            let mut f = HeightField::flat(25, 25, test_meta());
            for y in 0..f.height {
                for x in 0..f.width {
                    f.set(x, y, ((x * 7 + y * 13) % 11) as f32);
                }
            }
            f
        };
        let center = (12.3, 11.7);
        let brush = circle(5.0, 0.6, Falloff::Smoothstep);

        // Each tool, applied twice from the same start, yields identical cells.
        let run = |apply: &dyn Fn(&mut HeightField)| {
            let mut a = base.clone();
            let mut b = base.clone();
            apply(&mut a);
            apply(&mut b);
            assert_eq!(a.cells, b.cells, "tool application is not deterministic");
        };
        run(&|f| Raise.apply(f, &brush, center));
        run(&|f| Lower.apply(f, &brush, center));
        run(&|f| Smooth.apply(f, &brush, center));
        run(&|f| Flatten { target: 4.0 }.apply(f, &brush, center));
        run(&|f| SetHeight { target: 9.0 }.apply(f, &brush, center));
    }
}
