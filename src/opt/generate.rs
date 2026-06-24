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
        return gen_greedy_heightmap(
            heightmap,
            colormap,
            options,
            base_height_override,
            offset,
            progress_f,
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
    while 2_i32.pow(scale + 1) * (options.size as i32) < 500 {
        if !progress_f(0.2 + 0.5 * (scale as f32 / (500.0 / (options.size as f32)).log2())) {
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

    let pairs_vec = collect_height_color_pairs(heightmap, colormap, options.cull);
    // Lowest height present, used as the common base plane each terrain column
    // fills down to (see quads_to_bricks). With cull off (the terrain path) this
    // is the true grid minimum; with cull on (img2brick) it is the min non-zero
    // height, which is moot because img mode keeps the flat per-pixel height.
    // Grid mode overrides this with a global floor (Some(0)) so all tiles fill to
    // the same base; None reproduces the single-box per-tile minimum exactly.
    let min_height = base_height_override
        .unwrap_or_else(|| pairs_vec.iter().map(|(h, _)| *h).min().unwrap_or(0));

    let (planes_with_metadata, plane_build_duration) =
        build_planes(heightmap, colormap, options.cull, pairs_vec);
    progress!(0.4);

    let brick_scale = if options.micro { 2 } else { 10 };
    let max_quad_size = 1000 / brick_scale;
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
            let h = heightmap.at(x, y);
            let c = colormap.at(x, y);

            if (!cull || (h > 0 && c[3] > 0))
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
}
