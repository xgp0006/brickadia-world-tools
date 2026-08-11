//! Shared true-scale math for Map / Grid / dem_build (no egui).
//!
//! Keep Map-tab, Grid planner, and Tauri `dem_fetch_build` on ONE formula so
//! predicted size and meshed worlds never drift.

/// Map-tab horizontal scale ceiling (integer `hscale` for normal bricks).
pub(crate) const MAX_HORIZONTAL_SCALE: u16 = 128;

/// Derive the integer horizontal brick scale and the 1:1-matched vertical scale
/// from the user's target studs-per-meter + exaggeration, the effective
/// (post-density) DEM cell size in meters, and whether micro bricks are used.
///
/// True 1:1: a cell spans `2*size = 2*hscale*upf` world units (`upf` = the
/// GenOptions size multiplier — 5 for normal bricks, 1 for micro; 1 stud = 5
/// units) over `cell_m_eff` meters → `2*hscale*upf/cell_m_eff` units/m
/// horizontally. Surface-Z is `(m-min)*vertical` units → `vertical` units/m.
/// Setting them equal makes relief faithful; `exaggeration` scales it.
pub(crate) fn derive_scale(
    cell_m_eff: f64,
    studs_per_meter: f32,
    exaggeration: f32,
    micro: bool,
) -> (u16, f32) {
    debug_assert!(cell_m_eff > 0.0, "derive_scale: cell_m_eff must be positive, got {cell_m_eff}");
    let upf = if micro { 1.0 } else { 5.0 };
    // Clamp the PHYSICAL cell span (`hscale * upf`), not the raw integer scale:
    // micro (upf=1) needs 5× the integer scale of normal (upf=5) to reach the
    // SAME physical world, so its ceiling is 5× higher.
    let max_hscale = f64::from(MAX_HORIZONTAL_SCALE) * 5.0 / upf;
    // Solve `2*hscale*upf / cell_m_eff == studs_per_meter * 5` (5 units/stud).
    let hscale = ((f64::from(studs_per_meter) * 5.0 * cell_m_eff) / (2.0 * upf))
        .round()
        .clamp(1.0, max_hscale) as u16;
    let vertical =
        ((2.0 * f64::from(hscale) * upf / cell_m_eff) * f64::from(exaggeration)) as f32;
    (hscale, vertical)
}

/// Web Mercator ground resolution (meters per 256px-tile pixel) at a latitude.
pub(crate) fn ground_resolution_m(lat_deg: f64, zoom: u32) -> f64 {
    const EQUATOR_M_PER_PX_Z0: f64 = 40_075_016.686 / 256.0;
    EQUATOR_M_PER_PX_Z0 * lat_deg.to_radians().cos() / f64::from(2_u32.pow(zoom.min(30)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_scale_micro_needs_higher_integer_than_normal() {
        let (hn, _) = derive_scale(5.0, 4.0, 1.0, false);
        let (hm, _) = derive_scale(5.0, 4.0, 1.0, true);
        assert!(hm > hn, "micro hscale {hm} should exceed normal {hn}");
    }

    #[test]
    fn ground_resolution_finer_at_higher_zoom() {
        let a = ground_resolution_m(40.0, 10);
        let b = ground_resolution_m(40.0, 15);
        assert!(b < a);
    }
}
