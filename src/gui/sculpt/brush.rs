//! Brush geometry and falloff for the sculpt tools.
//!
//! A [`Brush`] describes the *shape* of a single dab: where its influence
//! reaches (`radius_cells`), how hard it pushes (`strength`), and how that push
//! tapers from center to edge ([`Falloff`]). The [`weight`] kernel is the pure
//! falloff function every tool multiplies its per-dab magnitude by — separated
//! out so it is unit-testable without touching a [`HeightField`].

/// The footprint of a brush dab. Each shape is defined to inscribe within the
/// `±radius_cells` bounding box `dab_rect` computes, so the same bound stays
/// valid for every shape — only the distance *metric* feeding [`weight`]
/// changes (see [`shape_distance`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BrushShape {
    /// Euclidean disc — the default round brush.
    Circle,
    /// Axis-aligned square (chebyshev metric); fills the bounding box.
    Square,
    /// 45°-rotated square / rhombus (manhattan metric); vertices at `(±r, 0)`
    /// and `(0, ±r)`.
    Diamond,
    /// Flat-side hexagon with vertices touching the box edges.
    Hexagon,
}

impl BrushShape {
    /// All shapes in UI display order (for the tool-panel ComboBox).
    pub(crate) const ALL: [BrushShape; 4] =
        [BrushShape::Circle, BrushShape::Square, BrushShape::Diamond, BrushShape::Hexagon];

    /// Full tooltip: the footprint and what it's good for (also carries the name).
    pub(crate) fn help(self) -> &'static str {
        match self {
            BrushShape::Circle => "Circle — round falloff, the all-purpose brush for organic terrain.",
            BrushShape::Square => "Square — fills its bounding box with crisp right-angle edges.",
            BrushShape::Diamond => "Diamond — 45° rhombus footprint; good for ridgelines and chamfers.",
            BrushShape::Hexagon => "Hexagon — flat-sided hex footprint that tiles cleanly for honeycomb terrain.",
        }
    }
}

/// Distance from a dab center `(dx, dy)` cells away, normalized so the shape's
/// boundary sits at `1.0` and its interior is `< 1.0` — the value [`weight`]
/// expects. The metric per shape:
///
/// - Circle  → euclidean `√(dx²+dy²)`
/// - Square  → chebyshev `max(|dx|, |dy|)`
/// - Diamond → manhattan `|dx| + |dy|`
/// - Hexagon → `max(|dx|, |dx|/2 + |dy|)` (flat-side hex inscribed in the box)
///
/// Each is divided by `radius_cells`. A non-positive or NaN radius returns
/// `+∞`, which [`weight`] clamps to a zero influence (NaN-safe; no panic).
pub(crate) fn shape_distance(shape: BrushShape, dx: f32, dy: f32, radius_cells: f32) -> f32 {
    if radius_cells <= 0.0 || radius_cells.is_nan() {
        return f32::INFINITY;
    }
    let (ax, ay) = (dx.abs(), dy.abs());
    let metric = match shape {
        BrushShape::Circle => (dx * dx + dy * dy).sqrt(),
        BrushShape::Square => ax.max(ay),
        BrushShape::Diamond => ax + ay,
        BrushShape::Hexagon => ax.max(ax * 0.5 + ay),
    };
    metric / radius_cells
}

/// How a dab's strength tapers from its center (`d = 0`) to its edge (`d = 1`),
/// where `d` is the distance from center normalized by `radius_cells`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Falloff {
    /// Smooth Hermite ease: `1` at center, `0` at the edge, with zero slope at
    /// both ends (the soft, natural-feeling default).
    Smoothstep,
    /// Linear ramp: `1 - d`. Full strength at center, fading straight to `0`.
    Linear,
    /// No taper: `1` everywhere inside the radius (a hard, flat stamp).
    Constant,
}

/// A brush: a shape, a reach, a per-dab magnitude, and a falloff profile.
///
/// `strength` is interpreted by the tool — meters per dab for Raise/Lower, a
/// `0..=1` blend factor for Smooth/Flatten/Set — but is always scaled by the
/// [`weight`] kernel so the edge of the brush moves less than its center.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Brush {
    pub shape: BrushShape,
    /// Reach of the brush in cells. The affected sub-rect is `±radius` around
    /// the dab center; cells beyond `radius` get zero weight.
    pub radius_cells: f32,
    /// Per-dab magnitude, scaled by [`weight`]. Units are tool-specific.
    pub strength: f32,
    pub falloff: Falloff,
}

/// The pure falloff kernel: given a [`Falloff`] and a distance from the dab
/// center normalized to `[0, 1]` by the brush radius, return an influence weight
/// in `[0, 1]`.
///
/// - `nd <= 0` (center) → `1.0` for every profile.
/// - `nd >= 1` (at/past the edge) → `0.0` for every profile.
/// - In between, the profile shapes the taper.
///
/// `nd` is clamped to `[0, 1]` up front so an out-of-range distance can never
/// produce a weight outside `[0, 1]` (the invariant every tool relies on to keep
/// cells `>= FLOOR_M` predictable).
pub(crate) fn weight(falloff: Falloff, normalized_distance: f32) -> f32 {
    let nd = normalized_distance.clamp(0.0, 1.0);
    match falloff {
        // Constant is flat inside the radius but must still vanish exactly at the
        // edge so a dab never influences a cell at or beyond `radius_cells`.
        Falloff::Constant => {
            if nd >= 1.0 {
                0.0
            } else {
                1.0
            }
        }
        Falloff::Linear => 1.0 - nd,
        // Smoothstep: 1 - (3t^2 - 2t^3) with t = nd, i.e. the Hermite ease
        // evaluated on the *inverted* distance so center=1, edge=0 with zero
        // slope at both ends.
        Falloff::Smoothstep => {
            let t = 1.0 - nd;
            t * t * (3.0 - 2.0 * t)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoothstep is ~1 at the center and ~0 at the edge, monotonically
    /// decreasing in between; Constant is flat across the interior then drops to
    /// 0 at the edge; Linear is the straight `1 - d` ramp. (`falloff_weights_
    /// center_and_edge`.)
    #[test]
    fn falloff_weights_center_and_edge() {
        // Center: every profile is full strength.
        assert!((weight(Falloff::Smoothstep, 0.0) - 1.0).abs() < 1e-6);
        assert!((weight(Falloff::Linear, 0.0) - 1.0).abs() < 1e-6);
        assert!((weight(Falloff::Constant, 0.0) - 1.0).abs() < 1e-6);

        // Edge: every profile vanishes.
        assert!(weight(Falloff::Smoothstep, 1.0).abs() < 1e-6);
        assert!(weight(Falloff::Linear, 1.0).abs() < 1e-6);
        assert!(weight(Falloff::Constant, 1.0).abs() < 1e-6);

        // Constant is flat through the interior (does NOT taper), unlike the
        // others.
        assert!((weight(Falloff::Constant, 0.25) - 1.0).abs() < 1e-6);
        assert!((weight(Falloff::Constant, 0.99) - 1.0).abs() < 1e-6);

        // Smoothstep tapers but stays above its linear counterpart near center
        // (eased shoulder) and below it past the midpoint — and is strictly
        // between 0 and 1 in the interior.
        let mid_smooth = weight(Falloff::Smoothstep, 0.5);
        assert!((mid_smooth - 0.5).abs() < 1e-6, "smoothstep midpoint is 0.5 by symmetry");
        let near = weight(Falloff::Smoothstep, 0.25);
        assert!(near > 0.5 && near < 1.0, "smoothstep eases in near center: {near}");

        // Linear is the exact 1 - d ramp.
        assert!((weight(Falloff::Linear, 0.25) - 0.75).abs() < 1e-6);
        assert!((weight(Falloff::Linear, 0.5) - 0.5).abs() < 1e-6);
    }

    /// Footprint metrics: at the center every shape is 0; along the axes every
    /// shape reaches its boundary (=1) at distance `r`; and the box corner
    /// `(r, r)` is inside only the Square — Circle/Diamond/Hexagon all exclude it.
    #[test]
    fn shape_distance_footprints() {
        let r = 4.0;
        for s in BrushShape::ALL {
            assert!(shape_distance(s, 0.0, 0.0, r).abs() < 1e-6, "{s:?} center must be 0");
            // On-axis boundary: every shape's metric equals r along ±x and ±y.
            assert!((shape_distance(s, r, 0.0, r) - 1.0).abs() < 1e-6, "{s:?} +x boundary");
            assert!((shape_distance(s, 0.0, r, r) - 1.0).abs() < 1e-6, "{s:?} +y boundary");
        }
        // The box corner: inside the Square (=1, on its edge), outside the rest.
        assert!((shape_distance(BrushShape::Square, r, r, r) - 1.0).abs() < 1e-6);
        assert!(shape_distance(BrushShape::Circle, r, r, r) > 1.0);
        assert!(shape_distance(BrushShape::Diamond, r, r, r) > 1.0);
        assert!(shape_distance(BrushShape::Hexagon, r, r, r) > 1.0);
        // Diamond excludes a point the Square keeps: (0.6r, 0.6r) → manhattan 1.2r.
        assert!(shape_distance(BrushShape::Diamond, 0.6 * r, 0.6 * r, r) > 1.0);
        assert!(shape_distance(BrushShape::Square, 0.6 * r, 0.6 * r, r) < 1.0);
        // Non-positive radius → +∞ (weight clamps it to zero influence).
        assert!(shape_distance(BrushShape::Circle, 1.0, 1.0, 0.0).is_infinite());
    }

    /// Monotonic non-increasing from center to edge, and always within `[0, 1]`,
    /// for every profile and any (even out-of-range) normalized distance.
    #[test]
    fn weight_is_bounded_and_monotonic() {
        for falloff in [Falloff::Smoothstep, Falloff::Linear, Falloff::Constant] {
            let mut prev = f32::INFINITY;
            // Sample across [-0.5, 1.5] to also exercise the clamp on both ends.
            for i in 0..=20 {
                let nd = -0.5 + (i as f32) * 0.1;
                let w = weight(falloff, nd);
                assert!((0.0..=1.0).contains(&w), "weight {w} out of [0,1] for {falloff:?} @ {nd}");
                // Within the clamped domain the curve never rises.
                if nd >= 0.0 {
                    assert!(w <= prev + 1e-6, "{falloff:?} not monotonic at {nd}: {w} > {prev}");
                    prev = w;
                }
            }
        }
    }
}
