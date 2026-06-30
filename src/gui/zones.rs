//! Freedraw omit/include zones — the surface-agnostic masking core.
//!
//! A zone is a closed polygon in CELL space that acts as a spatial XY
//! cookie-cutter on the export, independent of height. [`rasterize`] turns a set
//! of zones into a row-major keep-mask the mesher consumes. This module has NO
//! dependency on egui, the heightfield, or the map — pure geometry over
//! `(width, height)` — so the sculpt canvas (Phase 1a) and the Map tab (Phase 2)
//! share it verbatim.
//!
//! Combine rule, per cell: `keep = (inside_any_include || no_include_zones) &&
//! !inside_any_omit`. Omit wins on overlap; an empty zone set keeps everything
//! (the byte-identical default).

/// Whether a zone cuts a hole (`Omit`) or restricts the export to its interior
/// (`Include`). Serde derives are intentionally absent in Phase 1a — they arrive
/// with the `.h2bproj` project file in Phase 1b.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ZoneMode {
    Omit,
    Include,
}

/// A closed polygon mask. `polygon` holds vertices in CELL space (fractional
/// cell coordinates); the closing edge (last → first) is implicit. Fewer than 3
/// vertices is degenerate and contains nothing.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct Zone {
    pub mode: ZoneMode,
    pub polygon: Vec<(f32, f32)>,
}

/// Rotate every zone polygon by `theta` radians (CCW) about the source grid's
/// centre into the rotated grid's frame, so the cookie-cutter keep-mask still
/// registers with a [`super::sculpt::heightfield::HeightField::rotated`] /
/// [`super::sculpt::paint::PaintGrid::rotated`] export. Vertices are corner-frame
/// cell coordinates, and the height resample rotates the same corner frame about
/// `(w/2, h/2) → (nw/2, nh/2)`, so this uses the identical transform. `theta == 0`
/// clones verbatim. `new_w`/`new_h` are the rotated grid dims (from the shared
/// `rotated_dims`, computed by the caller — `heightfield` is private to `sculpt`).
pub(crate) fn rotate_zones(
    zones: &[Zone],
    src_w: u32,
    src_h: u32,
    new_w: u32,
    new_h: u32,
    theta: f32,
) -> Vec<Zone> {
    if theta == 0.0 {
        return zones.to_vec();
    }
    let (s, c) = theta.sin_cos();
    let (cs_x, cs_y) = (src_w as f32 * 0.5, src_h as f32 * 0.5);
    let (co_x, co_y) = (new_w as f32 * 0.5, new_h as f32 * 0.5);
    zones
        .iter()
        .map(|z| Zone {
            mode: z.mode,
            polygon: z
                .polygon
                .iter()
                .map(|&(vx, vy)| {
                    // O = co + R(θ)·(V − cs).
                    let (dx, dy) = (vx - cs_x, vy - cs_y);
                    (co_x + dx * c - dy * s, co_y + dx * s + dy * c)
                })
                .collect(),
        })
        .collect()
}

/// Row-major (`width * height`) keep-mask: `true` = export this cell. Each cell
/// is classified at its CENTER `(x + 0.5, y + 0.5)`.
pub(crate) fn rasterize(zones: &[Zone], width: u32, height: u32) -> Vec<bool> {
    let w = width as usize;
    let h = height as usize;
    let mut mask = vec![true; w * h];

    // Empty set → keep everything (the byte-identical default). Also the fast
    // path: no per-cell work at all.
    if zones.is_empty() {
        return mask;
    }

    let has_include = zones.iter().any(|z| z.mode == ZoneMode::Include);

    for cy in 0..h {
        let py = cy as f32 + 0.5;
        for cx in 0..w {
            let px = cx as f32 + 0.5;

            let mut in_include = false;
            let mut in_omit = false;
            for z in zones {
                if point_in_polygon(px, py, &z.polygon) {
                    match z.mode {
                        ZoneMode::Include => in_include = true,
                        ZoneMode::Omit => in_omit = true,
                    }
                }
            }

            mask[cy * w + cx] = (in_include || !has_include) && !in_omit;
        }
    }

    mask
}

/// Even-odd ray-cast point-in-polygon (W. R. Franklin PNPOLY). The strict `>`
/// y-comparison gives a half-open edge convention: a point lying exactly on a
/// shared edge is claimed by exactly one of two abutting polygons, so adjacent
/// zones tile every cell once with no gap or double-count. A polygon with fewer
/// than 3 vertices encloses no area.
///
/// The `(yj - yi)` division is safe: the guard `(yi > py) != (yj > py)` can only
/// be true when `yi != yj`, so the branch is never entered on a horizontal edge.
fn point_in_polygon(px: f32, py: f32, poly: &[(f32, f32)]) -> bool {
    if poly.len() < 3 {
        return false;
    }

    let mut inside = false;
    let n = poly.len();
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];
        if ((yi > py) != (yj > py)) && (px < (xj - xi) * (py - yi) / (yj - yi) + xi) {
            inside = !inside;
        }
        j = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Axis-aligned rectangle covering cell-centre columns `[x0, x1)` and rows
    /// `[y0, y1)` (the polygon spans integer cell coords, centres sit at +0.5).
    fn rect(mode: ZoneMode, x0: f32, y0: f32, x1: f32, y1: f32) -> Zone {
        Zone {
            mode,
            polygon: vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)],
        }
    }

    fn at(mask: &[bool], w: u32, x: u32, y: u32) -> bool {
        mask[(y * w + x) as usize]
    }

    /// Rotating zones by 0 is identity; a +90° turn maps a vertex by the SAME
    /// corner-frame rotation the height/paint resample uses: `O = co + R(θ)·(V−cs)`.
    /// For an 8×8 grid (cs=co=(4,4)), vertex (0,0) → (8,0) at +90°. A wrong sign
    /// (mirror) would send it to (0,8) — so this pins the direction too.
    #[test]
    fn rotate_zones_identity_and_90_maps_vertex_no_mirror() {
        let z = vec![Zone { mode: ZoneMode::Omit, polygon: vec![(0.0, 0.0), (2.0, 0.0), (2.0, 2.0)] }];
        assert_eq!(rotate_zones(&z, 8, 8, 8, 8, 0.0), z, "θ=0 is identity");
        let r = rotate_zones(&z, 8, 8, 8, 8, std::f32::consts::FRAC_PI_2);
        let v = r[0].polygon[0];
        assert!(
            (v.0 - 8.0).abs() < 1e-3 && (v.1 - 0.0).abs() < 1e-3,
            "vertex (0,0) must map to (8,0) at +90° (no mirror), got {v:?}",
        );
        assert_eq!(r[0].mode, ZoneMode::Omit, "mode preserved");
    }

    #[test]
    fn omit_zone_drops_inside_keeps_outside() {
        // Omit cells x∈{2,3,4,5}, y∈{2,3,4,5}.
        let z = rect(ZoneMode::Omit, 2.0, 2.0, 6.0, 6.0);
        let mask = rasterize(&[z], 10, 10);
        for y in 0..10 {
            for x in 0..10 {
                let inside = (2..6).contains(&x) && (2..6).contains(&y);
                assert_eq!(at(&mask, 10, x, y), !inside, "cell ({x},{y})");
            }
        }
    }

    #[test]
    fn include_zone_keeps_only_inside() {
        let z = rect(ZoneMode::Include, 2.0, 2.0, 6.0, 6.0);
        let mask = rasterize(&[z], 10, 10);
        for y in 0..10 {
            for x in 0..10 {
                let inside = (2..6).contains(&x) && (2..6).contains(&y);
                assert_eq!(at(&mask, 10, x, y), inside, "cell ({x},{y})");
            }
        }
    }

    #[test]
    fn omit_wins_over_overlapping_include() {
        let inc = rect(ZoneMode::Include, 1.0, 1.0, 8.0, 8.0); // cells 1..=7
        let omt = rect(ZoneMode::Omit, 3.0, 3.0, 5.0, 5.0); // cells 3..=4
        let mask = rasterize(&[inc, omt], 10, 10);
        assert!(!at(&mask, 10, 4, 4), "overlap cell omitted (omit wins)");
        assert!(at(&mask, 10, 1, 1), "include-only cell kept");
        assert!(!at(&mask, 10, 0, 0), "outside include dropped");
    }

    #[test]
    fn concave_polygon_excludes_the_notch() {
        // Square (2,2)-(8,8) with a rectangular notch cut from the top
        // (y≈8 side) between x=4 and x=6 down to y=5 — a genuinely concave loop.
        let z = Zone {
            mode: ZoneMode::Omit,
            polygon: vec![
                (2.0, 2.0),
                (8.0, 2.0),
                (8.0, 8.0),
                (6.0, 8.0),
                (6.0, 5.0),
                (4.0, 5.0),
                (4.0, 8.0),
                (2.0, 8.0),
            ],
        };
        let mask = rasterize(&[z], 10, 10);
        // Notch interior is OUTSIDE the polygon → not omitted → kept.
        assert!(at(&mask, 10, 5, 6), "notch cell kept (concave handled)");
        // Solid body below the notch and to the side → omitted.
        assert!(!at(&mask, 10, 5, 3), "body-below-notch cell omitted");
        assert!(!at(&mask, 10, 3, 6), "body-beside-notch cell omitted");
    }

    #[test]
    fn no_zones_keeps_everything() {
        let mask = rasterize(&[], 8, 6);
        assert_eq!(mask.len(), 48);
        assert!(mask.iter().all(|&k| k), "empty zone set keeps all cells");
    }

    #[test]
    fn degenerate_omit_has_no_effect() {
        let z = Zone {
            mode: ZoneMode::Omit,
            polygon: vec![(2.0, 2.0), (5.0, 5.0)], // 2 points
        };
        let mask = rasterize(&[z], 6, 6);
        assert!(mask.iter().all(|&k| k), "degenerate omit drops nothing");
    }

    #[test]
    fn degenerate_include_alone_keeps_nothing() {
        // has_include is true but the zone contains no cell → every cell fails
        // the include test (documented semantics: contributes no kept cells).
        let z = Zone {
            mode: ZoneMode::Include,
            polygon: vec![(2.0, 2.0), (5.0, 5.0)],
        };
        let mask = rasterize(&[z], 6, 6);
        assert!(mask.iter().all(|&k| !k), "lone degenerate include keeps nothing");
    }

    #[test]
    fn abutting_zones_tile_the_shared_edge_exactly_once() {
        // Two rectangles sharing the horizontal edge y = 5.5, which is exactly
        // the centre line of cell row 5. The half-open convention must assign
        // every cell on that edge to exactly one polygon — no gap, no double.
        let a = vec![(0.0, 1.5), (10.0, 1.5), (10.0, 5.5), (0.0, 5.5)];
        let b = vec![(0.0, 5.5), (10.0, 5.5), (10.0, 9.5), (0.0, 9.5)];
        for cx in 0u32..10 {
            let px = cx as f32 + 0.5;
            let py = 5.5;
            let n = point_in_polygon(px, py, &a) as u8 + point_in_polygon(px, py, &b) as u8;
            assert_eq!(n, 1, "shared-edge cell {cx} claimed exactly once, got {n}");
        }
    }
}
