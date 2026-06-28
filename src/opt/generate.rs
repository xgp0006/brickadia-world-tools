use super::{BitMask, GreedyQuad, QuadTree, greedy_mesh_binary_plane};
use crate::map::*;
use crate::util::*;
use brdb::{
    Brick, BrickSize, BrickType, Collision, Color, Position,
    assets::materials::{GLOW, PLASTIC},
};
use log::{info, warn};
use rayon::prelude::*;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Error string returned when the progress callback requests cancellation.
/// `build.rs` and the GUI converter match on this exact value to distinguish
/// user-cancel from a real failure — never inline the literal elsewhere.
pub const CANCELLED_MSG: &str = "Stopped by user";

/// One greedy-mesh plane (per-column bitmasks) tagged with the (height, color)
/// combination it encodes.
type TaggedPlane = (Vec<BitMask>, u32, [u8; 4]);

/// A meshed quad tagged with its plane's (height, color).
type TaggedQuad = (GreedyQuad, u32, [u8; 4]);

/// Upper bound on linear-optimize convergence passes. Convergence is usually
/// reached in ~5 passes; the bound exists so a `line_optimize` regression can
/// never hang the worker thread (Rule 2). On exhaustion the bricks are valid,
/// just less merged.
const MAX_LINEAR_PASSES: u32 = 64;

/// Generate a heightmap with brick conservation optimizations.
///
/// `base_height_override` and `offset` are additive grid-tiling hooks forwarded
/// to the chosen mesher; passing `(None, None)` reproduces the single-box
/// behavior exactly (per-tile min fill-floor + origin centering).
pub fn gen_opt_heightmap<F: Fn(f32) -> bool>(
    heightmap: &dyn Heightmap,
    colormap: &dyn Colormap,
    options: GenOptions,
    base_height_override: Option<u32>,
    offset: Option<(i32, i32)>,
    progress_f: F,
) -> Result<Vec<Brick>, String> {
    // Use greedy mesh if requested
    if options.greedy {
        // No keep-mask on this generic path: freedraw zones flow through
        // `generate_bricks_skip_floor`, which calls `gen_greedy_heightmap`
        // directly with the mask. Every `gen_opt_heightmap` caller is unmasked.
        return gen_greedy_heightmap(
            heightmap,
            colormap,
            options,
            base_height_override,
            offset,
            progress_f,
            None,
        );
    }

    // Use quad tree optimization
    gen_quad_heightmap(
        heightmap,
        colormap,
        options,
        base_height_override,
        offset,
        progress_f,
    )
}

/// Generate a heightmap using quadtree optimization.
///
/// `base_height_override`/`offset` are accepted for signature parity with the
/// greedy path but the quadtree mesher keeps its own origin-centering and
/// per-tile fill-floor (`quad.rs::into_bricks`); grid tiling uses the greedy
/// path exclusively (spec §1, correction #5), so these stay `None` here and the
/// quadtree output is byte-identical to before.
pub fn gen_quad_heightmap<F: Fn(f32) -> bool>(
    heightmap: &dyn Heightmap,
    colormap: &dyn Colormap,
    options: GenOptions,
    base_height_override: Option<u32>,
    offset: Option<(i32, i32)>,
    progress_f: F,
) -> Result<Vec<Brick>, String> {
    // Grid tiling never routes through the quadtree path; assert the contract so
    // a future caller that wires tiling here can't silently get un-offset output.
    debug_assert!(
        base_height_override.is_none() && offset.is_none(),
        "gen_quad_heightmap does not support grid base/offset overrides; use the greedy path",
    );
    let _ = (base_height_override, offset);
    macro_rules! progress {
        ($e:expr) => {
            if !progress_f($e) {
                return Err(CANCELLED_MSG.to_string());
            }
        };
    }
    progress!(0.0);

    let (width, height) = heightmap.size();
    let (mut quad, quadtree_build_duration) = build_quadtree(heightmap, colormap)?;
    progress!(0.2);

    let (prog_offset, prog_scale, quad_opt_duration) = if options.quadtree {
        let duration = optimize_quadtree(&mut quad, &options, &progress_f)?;
        progress!(0.7);
        (0.7, 0.25, duration)
    } else {
        (0.2, 0.75, Duration::ZERO)
    };

    let linear_opt_duration =
        optimize_linear(&mut quad, &options, prog_offset, prog_scale, &progress_f)?;
    progress!(0.95);

    let (bricks, brick_convert_duration) = quadtree_to_bricks(quad, options, width, height);

    log_total_time(
        "quadtree",
        &[
            ("build", quadtree_build_duration),
            ("quad-opt", quad_opt_duration),
            ("linear-opt", linear_opt_duration),
            ("bricks", brick_convert_duration),
        ],
    );

    progress!(1.0);
    Ok(bricks)
}

/// Build the per-pixel quadtree, logging its wall time.
fn build_quadtree(
    heightmap: &dyn Heightmap,
    colormap: &dyn Colormap,
) -> Result<(QuadTree, Duration), String> {
    info!("Building initial quadtree");
    let start = Instant::now();
    let quad = QuadTree::new(heightmap, colormap)?;
    let duration = start.elapsed();
    info!("Built quadtree in {:.2}s", duration.as_secs_f64());
    Ok((quad, duration))
}

/// Convert the optimized quadtree into bricks, logging reduction stats and
/// wall time.
fn quadtree_to_bricks(
    quad: QuadTree,
    options: GenOptions,
    width: u32,
    height: u32,
) -> (Vec<Brick>, Duration) {
    let area = width * height;
    let start = Instant::now();
    let bricks = quad.into_bricks(options, width, height);
    let duration = start.elapsed();
    let brick_count = bricks.len();
    info!(
        "Reduced {} to {} ({}%; -{} bricks)",
        area,
        brick_count,
        (100. - brick_count as f64 / area as f64 * 100.).floor(),
        area as i32 - brick_count as i32,
    );
    info!("Converted to bricks in {:.2}s", duration.as_secs_f64());
    (bricks, duration)
}

/// Log the per-phase and summed wall time of a generation run.
fn log_total_time(label: &str, phases: &[(&str, Duration)]) {
    let total: Duration = phases.iter().map(|(_, d)| *d).sum();
    let detail = phases
        .iter()
        .map(|(name, d)| format!("{}: {:.2}s", name, d.as_secs_f64()))
        .collect::<Vec<_>>()
        .join(", ");
    info!(
        "Total {} time: {:.2}s ({})",
        label,
        total.as_secs_f64(),
        detail
    );
}

/// Run the quadtree merge passes until bricks would exceed the 500-unit width
/// cap or a pass stops merging. Returns the elapsed time, or `CANCELLED_MSG`
/// if the progress callback requested cancellation.
fn optimize_quadtree<F: Fn(f32) -> bool>(
    quad: &mut QuadTree,
    options: &GenOptions,
    progress_f: &F,
) -> Result<Duration, String> {
    info!("Optimizing quadtree");
    let quad_opt_start = Instant::now();
    let mut scale = 0;

    // loop until the bricks would be too wide or we stop optimizing bricks
    while 2_i32.pow(scale + 1) * (options.size as i32) < i32::from(super::MAX_BRICK_UNITS) {
        if !progress_f(
            0.2 + 0.5 * (scale as f32 / (f32::from(super::MAX_BRICK_UNITS) / (options.size as f32)).log2()),
        ) {
            return Err(CANCELLED_MSG.to_string());
        }
        let count = quad.quad_optimize_level(scale);
        if count == 0 {
            break;
        }
        info!("  Removed {:?} {}x bricks", count, 2_i32.pow(scale));
        scale += 1;
    }
    let quad_opt_duration = quad_opt_start.elapsed();
    info!(
        "Quadtree optimization in {:.2}s",
        quad_opt_duration.as_secs_f64()
    );
    Ok(quad_opt_duration)
}

/// Run line-merge passes until convergence (bounded by `MAX_LINEAR_PASSES`).
/// Returns the elapsed time, or `CANCELLED_MSG` on cancellation.
fn optimize_linear<F: Fn(f32) -> bool>(
    quad: &mut QuadTree,
    options: &GenOptions,
    prog_offset: f32,
    prog_scale: f32,
    progress_f: &F,
) -> Result<Duration, String> {
    info!("Optimizing linear");
    let linear_opt_start = Instant::now();
    let mut converged = false;
    for i in 1..=MAX_LINEAR_PASSES {
        let count = quad.line_optimize(options.size as u32);
        if !progress_f(prog_offset + prog_scale * (i as f32 / 5.0).min(1.0)) {
            return Err(CANCELLED_MSG.to_string());
        }

        if count == 0 {
            converged = true;
            break;
        }
        info!("  Removed {} bricks", count);
    }
    if !converged {
        warn!(
            "linear optimization did not converge in {MAX_LINEAR_PASSES} passes; continuing with current bricks"
        );
    }
    let linear_opt_duration = linear_opt_start.elapsed();
    info!(
        "Linear optimization in {:.2}s",
        linear_opt_duration.as_secs_f64()
    );
    Ok(linear_opt_duration)
}

/// Generate a heightmap using greedy mesh optimization for each height level.
///
/// `base_height_override`: when `Some(b)`, every column fills down to brick
/// height `b` instead of the per-tile present minimum — grid mode passes
/// `Some(0)` so all tiles share one global floor. `None` reproduces the
/// single-box behavior (floor = this map's minimum height).
///
/// `offset`: when `Some((dx, dy))`, the meshed bricks are placed at that world
/// offset (units) instead of being centered on the origin — grid mode passes a
/// per-tile world offset so tiles abut. `None` defaults to exactly
/// `-(width*size), -(height*size)`, byte-identical to the prior centering.
pub fn gen_greedy_heightmap<F: Fn(f32) -> bool>(
    heightmap: &dyn Heightmap,
    colormap: &dyn Colormap,
    options: GenOptions,
    base_height_override: Option<u32>,
    offset: Option<(i32, i32)>,
    progress_f: F,
    keep_mask: Option<&[bool]>,
) -> Result<Vec<Brick>, String> {
    macro_rules! progress {
        ($e:expr) => {
            if !progress_f($e) {
                return Err(CANCELLED_MSG.to_string());
            }
        };
    }
    progress!(0.0);

    info!("Building greedy mesh planes");
    let (width, height) = heightmap.size();

    if colormap.size() != heightmap.size() {
        return Err("Heightmap and colormap must have same dimensions".to_string());
    }

    // Freedraw keep-mask is row-major `width*height`. A mismatch is a checked
    // error (defense-in-depth, like the heightmap/colormap dimension check) so a
    // miscomputed mask can never silently shift which cells are dropped.
    if let Some(mask) = keep_mask
        && mask.len() != (width as usize) * (height as usize)
    {
        return Err(format!(
            "keep_mask length {} != width*height {}",
            mask.len(),
            (width as usize) * (height as usize),
        ));
    }

    let pairs_vec = collect_height_color_pairs(heightmap, colormap, options.cull);
    // Lowest height present, used as the common base plane each terrain column
    // fills down to (see quads_to_bricks). With cull off (the terrain path) this
    // is the true grid minimum; with cull on (img2brick) it is the min non-zero
    // height, which is moot because img mode keeps the flat per-pixel height.
    // Grid mode overrides this with a global floor (Some(0)) so all tiles fill to
    // the same base; None reproduces the single-box per-tile minimum exactly.
    let min_height = base_height_override
        .unwrap_or_else(|| pairs_vec.iter().map(|(h, _)| *h).min().unwrap_or(0));

    // Cross-file invariant lock (sculpt skip_floor): a `skip_floor=true` caller
    // (convert_heightfield / _tiled) must pass an EXPLICIT base plane
    // (`base_override = Some(_)`), never `None`. The omit decision is meter-space —
    // `omit_below_h` is derived upstream against the same vertical_scale as the
    // base — so the base plane it measures against must be the caller's chosen
    // datum, not the per-tile data minimum `None` would compute. At the sculpt
    // default (`base_override = Some(0)` → `min_height == 0`, `omit_below_h == 0`)
    // only true-floor (`h == 0`) columns drop, byte-identical to the legacy
    // `(h - min_height) == 0` skip; a raised floor (`Some(base_h > 0)`) shortens
    // the fill while omit stays meter-space and independent. A `None` base under
    // skip_floor would silently decouple the omit datum from the fill base.
    debug_assert!(
        !options.skip_floor || base_height_override.is_some(),
        "skip_floor implies an explicit base_override so omit_below_h is derived against the chosen base (min_height={min_height})",
    );

    let (planes_with_metadata, plane_build_duration) =
        build_planes(heightmap, colormap, options.cull, pairs_vec, keep_mask);
    progress!(0.4);

    // Per-quad cell cap: a merged brick's world footprint is `cells * size`, so
    // the cap must scale with `options.size` (= horizontal_scale * 5, or *1 for
    // micro) to keep every brick within `MAX_BRICK_UNITS` (the same cap the
    // quadtree path enforces in quad.rs `line_optimize`). The `.max(1)` keeps the
    // cap positive when a single cell already exceeds it (unavoidable: one cell
    // is the minimum brick). A larger `size` merges fewer cells per brick.
    let max_quad_size =
        (u32::from(options.max_brick_units) / u32::from(options.size).max(1)).max(1);
    let (all_quads, greedy_mesh_duration) =
        mesh_planes(planes_with_metadata, width, height, max_quad_size);
    progress!(0.7);

    let (all_bricks, brick_build_duration) = quads_to_bricks(
        all_quads,
        &options,
        width,
        height,
        min_height,
        offset,
        &progress_f,
    )?;

    log_total_time(
        "greedy mesh",
        &[
            ("planes", plane_build_duration),
            ("mesh", greedy_mesh_duration),
            ("bricks", brick_build_duration),
        ],
    );

    progress!(1.0);
    Ok(all_bricks)
}

/// Collect every unique (height, color) combination present in the maps,
/// skipping transparent/zero-height pixels when `cull` is set.
fn collect_height_color_pairs(
    heightmap: &dyn Heightmap,
    colormap: &dyn Colormap,
    cull: bool,
) -> Vec<(u32, [u8; 4])> {
    let (width, height) = heightmap.size();
    let mut height_color_pairs = std::collections::BTreeSet::new();
    for x in 0..width {
        for y in 0..height {
            let h = heightmap.at(x, y);
            let c = colormap.at(x, y);
            if !cull || (h > 0 && c[3] > 0) {
                height_color_pairs.insert((h, c));
            }
        }
    }
    info!(
        "Found {} unique (height, color) combinations",
        height_color_pairs.len()
    );
    height_color_pairs.into_iter().collect()
}

/// Build one binary plane (one `BitMask` per image column) for each
/// (height, color) pair in a single pass over the image. Logs and returns the
/// phase wall time.
fn build_planes(
    heightmap: &dyn Heightmap,
    colormap: &dyn Colormap,
    cull: bool,
    pairs_vec: Vec<(u32, [u8; 4])>,
    keep_mask: Option<&[bool]>,
) -> (Vec<TaggedPlane>, Duration) {
    let start = Instant::now();
    let (width, height) = heightmap.size();

    let mut plane_map: HashMap<(u32, [u8; 4]), usize> = HashMap::with_capacity(pairs_vec.len());
    let mut all_planes: Vec<Vec<BitMask>> = Vec::with_capacity(pairs_vec.len());
    for (idx, &pair) in pairs_vec.iter().enumerate() {
        plane_map.insert(pair, idx);
        all_planes.push(vec![BitMask::new(); width as usize]);
    }

    for x in 0..width {
        for y in 0..height {
            // Freedraw omit/include mask: a `false` cell is excluded from every
            // occupancy plane — the same per-cell skip `cull` performs — so it
            // emits no brick. `None` (and an all-`true` mask) leaves the planes
            // byte-identical, guarded by `keep_mask_none_is_byte_identical`.
            let kept = keep_mask.is_none_or(|m| m[(y * width + x) as usize]);

            let h = heightmap.at(x, y);
            let c = colormap.at(x, y);

            if kept
                && (!cull || (h > 0 && c[3] > 0))
                && let Some(&plane_idx) = plane_map.get(&(h, c))
            {
                all_planes[plane_idx][x as usize].set_bit(y);
            }
        }
    }

    let planes: Vec<_> = all_planes
        .into_iter()
        .zip(pairs_vec)
        .map(|(plane, (h, color))| (plane, h, color))
        .collect();

    let duration = start.elapsed();
    info!(
        "Built {} planes in {:.2}s",
        planes.len(),
        duration.as_secs_f64()
    );
    (planes, duration)
}

/// Greedy-mesh every plane in parallel, tagging each quad with its plane's
/// (height, color). Logs and returns the phase wall time.
fn mesh_planes(
    planes: Vec<TaggedPlane>,
    width: u32,
    height: u32,
    max_quad_size: u32,
) -> (Vec<TaggedQuad>, Duration) {
    let start = Instant::now();
    let all_quads: Vec<_> = planes
        .into_par_iter()
        .flat_map(|(plane, h, color)| {
            greedy_mesh_binary_plane(plane, width, height, max_quad_size)
                .into_iter()
                .map(move |quad| (quad, h, color))
                .collect::<Vec<_>>()
        })
        .collect();
    let duration = start.elapsed();
    info!(
        "Greedy meshed {} quads in {:.2}s",
        all_quads.len(),
        duration.as_secs_f64()
    );
    (all_quads, duration)
}

/// Convert greedy quads into bricks, reporting progress every 1000 quads and
/// returning `CANCELLED_MSG` if the callback requests cancellation. Logs and
/// returns the phase wall time.
fn quads_to_bricks<F: Fn(f32) -> bool>(
    all_quads: Vec<TaggedQuad>,
    options: &GenOptions,
    width: u32,
    height: u32,
    min_height: u32,
    offset: Option<(i32, i32)>,
    progress_f: &F,
) -> Result<(Vec<Brick>, Duration), String> {
    let start = Instant::now();
    let mut all_bricks = Vec::new();
    let total_quads = all_quads.len();

    // World placement offset (units). Grid mode passes a per-tile world offset so
    // tiles abut; single-box passes None and falls back to the origin-centering
    // this function used to compute internally — byte-identical to before.
    let (offset_x, offset_y) = offset.unwrap_or((
        -(width as i32 * options.size as i32),
        -(height as i32 * options.size as i32),
    ));

    for (idx, (quad, h, color)) in all_quads.into_iter().enumerate() {
        if idx % 1000 == 0 && !progress_f(0.7 + 0.25 * (idx as f32 / total_quads as f32)) {
            return Err(CANCELLED_MSG.to_string());
        }

        // When fill_to_base is requested (the Map tab), a terrain column fills
        // from its height DOWN to the common base plane `min_height`, so the
        // surface is solid and watertight — a cell 50 units above its neighbor is
        // a 25-unit riser to the base, not a 2-unit tile floating with a
        // fall-through gap underneath. The `*scale/2` matches the half-unit z
        // step (emit_column_bricks does `z -= brick_height*2`); unlike the
        // quadtree path (quad.rs into_bricks), which fills only to each tile's
        // nearest-neighbor floor, this fills to the GLOBAL min for one watertight
        // base. Off (Convert tab / CLI, or any img2brick build) keeps the legacy
        // flat 2-unit block — those heights are un-normalized / unbounded, and
        // img mode holds position Z constant so a tall fill would overlap.
        // Floor/omit-skip (sculpt/blank-canvas convert only): when fill_to_base
        // is on and this column's brick-Z height is at or below the omit
        // threshold (`h <= omit_below_h`), emit nothing — the native Brickadia
        // ground stands in. `omit_below_h` is derived meter-space upstream
        // (`round(omit_below_m * vertical_scale)`) so the decision is made
        // against the SOURCE height in meters, NOT a scale-dependent quantization
        // artifact: at a proper scale a near-floor cell maps to `h >= 1` and
        // survives (the gap fix), while at the default `omit_below_h == 0` with
        // `base_override`'s `min_height == 0` only true-floor (`h == 0`) columns
        // drop — exactly the old `(h - min_height) == 0` behavior. This requires
        // every `skip_floor=true` caller to pass an EXPLICIT `base_override`
        // (`Some(_)`, never `None`) so `omit_below_h`'s meter-space datum matches
        // the chosen base plane; that cross-file invariant is locked by the
        // `debug_assert!` in `gen_greedy_heightmap`. Gated on the
        // SAME conditions as the fill_to_base branch below (`fill_to_base &&
        // !img`) so `skip_floor` can never alter the legacy flat-surface path.
        // Default-off (skip_floor = false) leaves every existing
        // single-box/grid output byte-identical.
        if options.skip_floor
            && options.fill_to_base
            && !options.img
            && h <= options.omit_below_h
        {
            continue;
        }

        let desired_height = if options.fill_to_base && !options.img {
            (((h as i32 - min_height as i32).max(0)) * options.scale as i32 / 2).max(2)
        } else {
            (options.scale * 2) as i32
        };

        emit_column_bricks(
            &mut all_bricks,
            options,
            BrickColumn {
                z: (options.scale * h) as i32,
                desired_height,
                size_x: quad.w as u16 * options.size,
                size_y: quad.h as u16 * options.size,
                pos_x: quad.x as i32 * options.size as i32 * 2
                    + quad.w as i32 * options.size as i32
                    + offset_x,
                pos_y: quad.y as i32 * options.size as i32 * 2
                    + quad.h as i32 * options.size as i32
                    + offset_y,
                color,
            },
        );
    }

    let duration = start.elapsed();
    info!(
        "Converted to {} bricks in {:.2}s",
        all_bricks.len(),
        duration.as_secs_f64()
    );
    Ok((all_bricks, duration))
}

/// One vertical column of terrain: its footprint, world position, starting
/// grid Z, and the total height it must fill.
pub(crate) struct BrickColumn {
    /// Grid Z of the column top (post-snap), in heightmap half-units.
    pub z: i32,
    /// Total height to fill, in brick height units.
    pub desired_height: i32,
    pub size_x: u16,
    pub size_y: u16,
    pub pos_x: i32,
    pub pos_y: i32,
    pub color: [u8; 4],
}

/// Emit the stack of bricks for one terrain column, splitting at the 250-unit
/// procedural-brick height cap. Shared by the greedy and quadtree paths so
/// their collision/material semantics cannot drift (greedy semantics —
/// validated in-game 2026-06-09).
pub(crate) fn emit_column_bricks(out: &mut Vec<Brick>, options: &GenOptions, col: BrickColumn) {
    let parity = if options.stud { 5 } else { 2 };
    let mut z = col.z;
    let mut desired_height = col.desired_height;

    // Bounded (Rule 2): every iteration removes at least `parity` height.
    while desired_height > 0 {
        let brick_height = desired_height.max(parity).min(250) as u16;
        let brick_height = brick_height + brick_height % parity as u16;

        out.push(Brick {
            asset: BrickType::Procedural {
                asset: options.asset.clone(),
                // if it's a microbrick image, just use the block size so it's cubes
                size: BrickSize::new(
                    col.size_x,
                    col.size_y,
                    if options.img && options.micro {
                        options.size
                    } else {
                        brick_height
                    },
                ),
            },
            position: Position::new(
                col.pos_x,
                col.pos_y,
                options.base_height() - 5
                    + if options.img {
                        0
                    } else {
                        z - brick_height as i32
                    },
            ),
            collision: Collision {
                player: !options.nocollide,
                weapon: !options.nocollide,
                interact: !options.nocollide,
                ..Default::default()
            },
            color: Color {
                r: col.color[0],
                g: col.color[1],
                b: col.color[2],
            },
            owner_index: None,
            material_intensity: if options.glow { 0 } else { 5 },
            material: if options.glow { GLOW } else { PLASTIC },
            ..Default::default()
        });

        desired_height -= brick_height as i32;
        z -= brick_height as i32 * 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brdb::assets::bricks::PB_DEFAULT_TILE;

    fn test_options(stud: bool) -> GenOptions {
        GenOptions {
            size: 5,
            scale: 1,
            asset: PB_DEFAULT_TILE,
            cull: false,
            micro: false,
            stud,
            snap: false,
            img: false,
            glow: false,
            hdmap: false,
            lrgb: false,
            nocollide: false,
            quadtree: false,
            greedy: true,
            fill_to_base: false,
            skip_floor: false,
            omit_below_h: 0,
            max_brick_units: crate::opt::MAX_BRICK_UNITS,
        }
    }

    fn brick_height(brick: &Brick) -> u16 {
        match &brick.asset {
            BrickType::Procedural { size, .. } => size.z,
            other => panic!("expected procedural brick, got {other:?}"),
        }
    }

    /// Procedural footprint+height of a brick, for position-independent equality
    /// (`Brick` derives no `PartialEq`, so we compare the load-bearing fields the
    /// offset/centering actually drive).
    fn brick_geom(b: &Brick) -> (Position, BrickSize) {
        match &b.asset {
            BrickType::Procedural { size, .. } => (b.position, *size),
            other => panic!("expected procedural brick, got {other:?}"),
        }
    }

    #[test]
    fn skip_floor_default_off_is_byte_identical() {
        // The additive `skip_floor` flag, default-OFF, must leave the
        // fill_to_base terrain path byte-identical. A fixed quad set (some quads
        // AT the base plane `min_height`, which `skip_floor=true` would drop) is
        // meshed through `quads_to_bricks` with `skip_floor=false`, then compared
        // against an INDEPENDENT re-derivation of the exact pre-change loop —
        // the same desired_height formula + `emit_column_bricks`, with NO skip.
        // Equality proves the flag is a no-op when off (locks the default).
        let mut options = test_options(false);
        options.fill_to_base = true; // exercise the fill branch the skip gates on
        options.skip_floor = false; // the contract under test
        let (width, height) = (8u32, 6u32);
        let min_height = 7u32;
        let quads: Vec<TaggedQuad> = vec![
            // h == min_height (7): the would-be-skipped floor column.
            (GreedyQuad { x: 0, y: 0, w: 3, h: 2 }, 7, [1, 2, 3, 255]),
            (GreedyQuad { x: 3, y: 0, w: 2, h: 4 }, 40, [4, 5, 6, 255]),
            (GreedyQuad { x: 5, y: 2, w: 3, h: 3 }, 19, [7, 8, 9, 255]),
        ];
        let offset = (-(width as i32 * options.size as i32), -(height as i32 * options.size as i32));

        let (actual, _) = quads_to_bricks(
            quads.clone(),
            &options,
            width,
            height,
            min_height,
            Some(offset),
            &|_| true,
        )
        .expect("quads_to_bricks with skip_floor=false must mesh");

        // Independent reference: the pre-change loop verbatim (no skip branch).
        let mut reference = Vec::new();
        for (quad, h, color) in &quads {
            let desired_height = if options.fill_to_base && !options.img {
                (((*h as i32 - min_height as i32).max(0)) * options.scale as i32 / 2).max(2)
            } else {
                (options.scale * 2) as i32
            };
            emit_column_bricks(
                &mut reference,
                &options,
                BrickColumn {
                    z: (options.scale * h) as i32,
                    desired_height,
                    size_x: quad.w as u16 * options.size,
                    size_y: quad.h as u16 * options.size,
                    pos_x: quad.x as i32 * options.size as i32 * 2
                        + quad.w as i32 * options.size as i32
                        + offset.0,
                    pos_y: quad.y as i32 * options.size as i32 * 2
                        + quad.h as i32 * options.size as i32
                        + offset.1,
                    color: *color,
                },
            );
        }

        assert!(!reference.is_empty(), "fixture must emit bricks");
        assert_eq!(
            actual.len(),
            reference.len(),
            "skip_floor=false must emit the same brick count as the pre-change loop",
        );
        let actual_geom: Vec<_> = actual.iter().map(brick_geom).collect();
        let reference_geom: Vec<_> = reference.iter().map(brick_geom).collect();
        assert_eq!(
            actual_geom, reference_geom,
            "skip_floor=false must be byte-identical to the pre-change emit (including the floor column)",
        );
    }

    #[test]
    fn single_box_offset_unchanged() {
        // The `offset = None` default of quads_to_bricks must reproduce the
        // centering the function used to compute internally
        // (`-(width*size), -(height*size)`) byte-for-byte. A fixed quad set is
        // meshed twice — once with None, once with the explicit centered offset —
        // and the two outputs must be identical position-and-size. This locks the
        // single-box identity guarantee against the grid-offset refactor.
        let options = test_options(false);
        let (width, height) = (8u32, 6u32);
        let quads: Vec<TaggedQuad> = vec![
            (GreedyQuad { x: 0, y: 0, w: 3, h: 2 }, 10, [1, 2, 3, 255]),
            (GreedyQuad { x: 3, y: 0, w: 2, h: 4 }, 40, [4, 5, 6, 255]),
            (GreedyQuad { x: 5, y: 2, w: 3, h: 3 }, 7, [7, 8, 9, 255]),
        ];

        let (from_none, _) =
            quads_to_bricks(quads.clone(), &options, width, height, 0, None, &|_| true)
                .expect("None offset must mesh");

        let centered = (
            -(width as i32 * options.size as i32),
            -(height as i32 * options.size as i32),
        );
        let (from_explicit, _) = quads_to_bricks(
            quads,
            &options,
            width,
            height,
            0,
            Some(centered),
            &|_| true,
        )
        .expect("explicit centered offset must mesh");

        assert_eq!(
            from_none.len(),
            from_explicit.len(),
            "None and explicit-centered offsets must emit the same brick count",
        );
        assert!(!from_none.is_empty(), "fixture must emit bricks");
        let none_geom: Vec<_> = from_none.iter().map(brick_geom).collect();
        let explicit_geom: Vec<_> = from_explicit.iter().map(brick_geom).collect();
        assert_eq!(
            none_geom, explicit_geom,
            "offset=None must be byte-identical to the prior internal centering",
        );

        // Independently confirm the centering matches the documented formula on
        // the first quad's first brick (guards against both paths sharing a bug).
        // First quad is at (x=0, y=0, w=3, h=2): pos = quad*size*2 + dim*size +
        // offset; the x*size*2 / y*size*2 terms are 0 here (quad origin).
        let size = options.size as i32;
        let first = &from_none[0];
        assert_eq!(
            first.position.x,
            3 * size + centered.0,
            "centered pos_x must follow the quad/offset formula",
        );
        assert_eq!(
            first.position.y,
            2 * size + centered.1,
            "centered pos_y must follow the quad/offset formula",
        );
    }

    #[test]
    fn explicit_offset_translates_by_delta() {
        // Grid mode passes a per-tile world offset; emitted positions must shift
        // by exactly the difference between two offsets (pure translation), which
        // is what makes tiles abut at integer pitch.
        let options = test_options(false);
        let (width, height) = (4u32, 4u32);
        let quads: Vec<TaggedQuad> =
            vec![(GreedyQuad { x: 1, y: 1, w: 2, h: 2 }, 12, [9, 9, 9, 255])];

        let (base, _) = quads_to_bricks(
            quads.clone(),
            &options,
            width,
            height,
            0,
            Some((0, 0)),
            &|_| true,
        )
        .expect("mesh at origin offset");
        let (shifted, _) = quads_to_bricks(
            quads,
            &options,
            width,
            height,
            0,
            Some((1000, -2000)),
            &|_| true,
        )
        .expect("mesh at shifted offset");

        assert_eq!(base.len(), shifted.len());
        for (b, s) in base.iter().zip(&shifted) {
            assert_eq!(s.position.x, b.position.x + 1000);
            assert_eq!(s.position.y, b.position.y - 2000);
            assert_eq!(s.position.z, b.position.z, "offset must not touch Z");
        }
    }

    #[test]
    fn emit_column_bricks_splits_tall_columns_at_250() {
        let options = test_options(false);
        let mut bricks = Vec::new();
        emit_column_bricks(
            &mut bricks,
            &options,
            BrickColumn {
                z: 1200,
                desired_height: 600,
                size_x: 10,
                size_y: 15,
                pos_x: 7,
                pos_y: -3,
                color: [10, 20, 30, 255],
            },
        );

        let heights: Vec<u16> = bricks.iter().map(brick_height).collect();
        assert_eq!(
            heights,
            vec![250, 250, 100],
            "600 must split at the 250 cap"
        );
        assert_eq!(
            heights.iter().map(|&h| h as i32).sum::<i32>(),
            600,
            "stacked bricks must preserve the total column height"
        );

        // Each brick's grid Z drops by 2x the previous brick's height; the
        // stored position Z is base_height - 5 + (z - height).
        let base = options.base_height() - 5;
        assert_eq!(bricks[0].position.z, base + 1200 - 250);
        assert_eq!(bricks[1].position.z, base + (1200 - 500) - 250);
        assert_eq!(bricks[2].position.z, base + (1200 - 1000) - 100);
        for b in &bricks {
            assert_eq!((b.position.x, b.position.y), (7, -3));
        }
    }

    #[test]
    fn emit_column_bricks_rounds_odd_heights_to_parity() {
        // Non-stud parity is 2: a desired height of 3 must round up to 4.
        let options = test_options(false);
        let mut bricks = Vec::new();
        emit_column_bricks(
            &mut bricks,
            &options,
            BrickColumn {
                z: 6,
                desired_height: 3,
                size_x: 5,
                size_y: 5,
                pos_x: 0,
                pos_y: 0,
                color: [0, 0, 0, 255],
            },
        );
        let heights: Vec<u16> = bricks.iter().map(brick_height).collect();
        assert_eq!(heights, vec![4], "odd height must round up to parity 2");
    }

    #[test]
    fn emit_column_bricks_respects_stud_minimum() {
        // Stud parity is 5: a desired height of 2 must become at least 5.
        let options = test_options(true);
        let mut bricks = Vec::new();
        emit_column_bricks(
            &mut bricks,
            &options,
            BrickColumn {
                z: 10,
                desired_height: 2,
                size_x: 5,
                size_y: 5,
                pos_x: 0,
                pos_y: 0,
                color: [0, 0, 0, 255],
            },
        );
        let heights: Vec<u16> = bricks.iter().map(brick_height).collect();
        assert_eq!(heights, vec![5], "stud bricks must be at least 5 tall");
    }

    #[test]
    fn emit_column_bricks_uses_validated_material_and_collision() {
        // The greedy-path semantics validated in-game: non-glow bricks get
        // PLASTIC at intensity 5, and tool collision stays at its default.
        let options = test_options(false);
        let mut bricks = Vec::new();
        emit_column_bricks(
            &mut bricks,
            &options,
            BrickColumn {
                z: 4,
                desired_height: 2,
                size_x: 5,
                size_y: 5,
                pos_x: 0,
                pos_y: 0,
                color: [1, 2, 3, 255],
            },
        );
        assert_eq!(bricks.len(), 1);
        assert_eq!(bricks[0].material, PLASTIC);
        assert_eq!(bricks[0].material_intensity, 5);
        assert!(bricks[0].collision.player);
        assert!(bricks[0].collision.tool);
        assert_eq!(
            (bricks[0].color.r, bricks[0].color.g, bricks[0].color.b),
            (1, 2, 3)
        );
    }

    /// A uniform field merges maximally, so it stresses the per-quad cell cap.
    struct UniformMap {
        width: u32,
        height: u32,
    }
    impl Heightmap for UniformMap {
        fn at(&self, _x: u32, _y: u32) -> u32 {
            20
        }
        fn size(&self) -> (u32, u32) {
            (self.width, self.height)
        }
    }
    impl Colormap for UniformMap {
        fn at(&self, _x: u32, _y: u32) -> [u8; 4] {
            [128, 128, 128, 255]
        }
        fn size(&self) -> (u32, u32) {
            (self.width, self.height)
        }
    }

    /// A merged brick's world footprint is `quad_cells * options.size`. The
    /// per-quad cell cap must shrink as `size` (= horizontal_scale * 5) grows so
    /// no brick exceeds `MAX_BRICK_UNITS` — the in-game-render limit (bigger bricks
    /// silently drop in Brickadia → gaps). At `size = 35` a wide flat band merges
    /// up to `floor(MAX_BRICK_UNITS/35)` cells: the cap is exercised (the largest
    /// brick fills the budget to within one cell) but never overruns it.
    #[test]
    fn greedy_merges_never_exceed_the_brick_unit_cap() {
        let map = UniformMap { width: 800, height: 4 };
        let mut options = test_options(false);
        options.size = 35; // horizontal scale 7 (× 5 units/stud)
        options.fill_to_base = true;
        let cell_size = options.size; // captured before `options` moves into the mesher
        let bricks =
            gen_greedy_heightmap(&map, &map, options, Some(0), Some((0, 0)), |_| true, None)
                .expect("greedy mesh must succeed");
        assert!(!bricks.is_empty(), "a uniform field must emit bricks");
        let mut max_axis = 0u16;
        for b in &bricks {
            let (_, size) = brick_geom(b);
            assert!(
                size.x <= crate::opt::MAX_BRICK_UNITS && size.y <= crate::opt::MAX_BRICK_UNITS,
                "merged brick {}×{} units exceeds MAX_BRICK_UNITS ({})",
                size.x,
                size.y,
                crate::opt::MAX_BRICK_UNITS,
            );
            max_axis = max_axis.max(size.x).max(size.y);
        }
        // The cap is actually exercised: the largest brick fills the budget to
        // within one cell (`floor(cap/size)*size`), proving merges aren't
        // artificially small — yet none overruns `MAX_BRICK_UNITS`.
        assert!(
            max_axis > crate::opt::MAX_BRICK_UNITS - cell_size,
            "a wide flat band must merge up to the cap (within one cell), got {max_axis}",
        );
    }

    /// The same cap holds for micro bricks at horizontal scale > 1 (`size > 1`):
    /// micro's larger merge budget must still scale down with `size` so a merged
    /// micro brick never exceeds `MAX_BRICK_UNITS` either.
    #[test]
    fn greedy_micro_merges_never_exceed_the_brick_unit_cap() {
        let map = UniformMap { width: 800, height: 4 };
        let mut options = test_options(false);
        options.micro = true;
        options.size = 40; // micro at horizontal scale 40 (× 1 unit/stud)
        options.fill_to_base = true;
        let bricks =
            gen_greedy_heightmap(&map, &map, options, Some(0), Some((0, 0)), |_| true, None)
                .expect("greedy micro mesh must succeed");
        assert!(!bricks.is_empty(), "a uniform field must emit bricks");
        for b in &bricks {
            let (_, size) = brick_geom(b);
            assert!(
                size.x <= crate::opt::MAX_BRICK_UNITS && size.y <= crate::opt::MAX_BRICK_UNITS,
                "merged micro brick {}×{} units exceeds MAX_BRICK_UNITS ({})",
                size.x,
                size.y,
                crate::opt::MAX_BRICK_UNITS,
            );
        }
    }

    /// Fresh greedy options for the keep-mask tests: fill_to_base on (so every
    /// kept cell emits a column) and a small fixed canvas.
    fn mask_test_options() -> GenOptions {
        let mut o = test_options(false);
        o.fill_to_base = true;
        o
    }

    fn geom_of(bricks: &[Brick]) -> Vec<(Position, BrickSize)> {
        bricks.iter().map(brick_geom).collect()
    }

    #[test]
    fn keep_mask_none_is_byte_identical() {
        // The identity guard: an all-`true` mask must mesh byte-identically to
        // `None`, so the masking machinery is provably inert by default.
        let map = UniformMap { width: 8, height: 4 };
        let none = gen_greedy_heightmap(
            &map, &map, mask_test_options(), Some(0), Some((0, 0)), |_| true, None,
        )
        .expect("None mesh");
        let all_true = vec![true; 8 * 4];
        let masked = gen_greedy_heightmap(
            &map, &map, mask_test_options(), Some(0), Some((0, 0)), |_| true, Some(&all_true),
        )
        .expect("all-true mesh");
        assert!(!none.is_empty(), "fixture must emit bricks");
        assert_eq!(geom_of(&none), geom_of(&masked), "all-true mask == None");
    }

    #[test]
    fn keep_mask_all_false_emits_no_bricks() {
        // Every cell masked out → no occupancy in any plane → zero bricks.
        let map = UniformMap { width: 8, height: 4 };
        let mask = vec![false; 8 * 4];
        let bricks = gen_greedy_heightmap(
            &map, &map, mask_test_options(), Some(0), Some((0, 0)), |_| true, Some(&mask),
        )
        .expect("all-false mesh");
        assert!(bricks.is_empty(), "a fully-masked field emits nothing");
    }

    #[test]
    fn keep_mask_partial_changes_geometry() {
        // Keep only the left half (x < 4). The mesh must differ from the full
        // field and still emit bricks — proof the mask drops real geometry.
        let map = UniformMap { width: 8, height: 4 };
        let mut mask = vec![false; 8 * 4];
        for y in 0..4 {
            for x in 0..4 {
                mask[y * 8 + x] = true;
            }
        }
        let full = gen_greedy_heightmap(
            &map, &map, mask_test_options(), Some(0), Some((0, 0)), |_| true, None,
        )
        .expect("full mesh");
        let masked = gen_greedy_heightmap(
            &map, &map, mask_test_options(), Some(0), Some((0, 0)), |_| true, Some(&mask),
        )
        .expect("half-masked mesh");
        assert!(!masked.is_empty(), "half a field still emits bricks");
        assert_ne!(geom_of(&full), geom_of(&masked), "masking must change the mesh");
    }

    #[test]
    fn keep_mask_wrong_length_is_a_checked_error() {
        let map = UniformMap { width: 8, height: 4 };
        let mask = vec![true; 8 * 4 - 1]; // one short
        let result = gen_greedy_heightmap(
            &map, &map, mask_test_options(), Some(0), Some((0, 0)), |_| true, Some(&mask),
        );
        // `.err()` (Option) sidesteps `expect_err`, which would need the Ok type
        // `Vec<Brick>` to be Debug (brdb::Brick is not).
        assert!(result.is_err(), "a mask length mismatch must error, not panic");
        let err = result.err().expect("checked above");
        assert!(err.contains("keep_mask length"), "informative error: {err}");
    }
}
