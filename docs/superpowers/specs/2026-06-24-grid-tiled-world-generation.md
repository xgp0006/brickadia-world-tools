# Grid / Tiled Large-World Generation — Implementation Spec

Project: `~/Projects/brickadia/heightmap2brz` · Status: **ready to implement (run via ultracode)** · Developed by a 4-lens design workflow + synthesis + independent red-team (`wf_e6901fe5-eff`).

## How to use this doc
The **BINDING CORRECTIONS** below OVERRIDE the *Detailed Design* body wherever they conflict — read them first. Line numbers throughout are approximate (the working tree drifts a few lines): **grep for the cited symbol, don't trust exact line numbers.** The three correctness pillars and the brdb API were verified against source and re-derived numerically.

## Red-team verdict
SOLID FOUNDATION. The three correctness pillars — (1) pixel-snapped geographic partition, (2) GLOBAL elevation-min datum, (3) explicit per-tile world offset — are VERIFIED CORRECT and re-derived numerically (with per-tile min a shared edge steps ~7736 brick units; with `global_min` + `base_override=Some(0)` both tiles produce identical top-Z **and** identical fill floor). The brdb `.brz`/`.brdb` write API is real (`World::write_brdb`/`write_brz`, brdb 0.4 `world.rs:31-50`). The MVP (**Phase A: sequential auto-grid → stitched `.brdb`**) is buildable and seam-correct as written. One blocking flaw (parallelism) + the corrections below must be folded in.

## BINDING CORRECTIONS (override the detailed body)
1. **[BLOCKING] Tiles run SEQUENTIALLY in mesh phase; each tile's `mesh_planes` uses the FULL global rayon pool.** The body's nested W-thread tile-pool is a false premise — rayon `install()` runs inner `par_iter` on the *current* pool, so a W-thread tile pool CONFINES meshing to W threads (rayon-core `thread_pool/mod.rs:24,35`). Delete `MAX_TILE_WORKERS`/`tile_pool`/the meshing adaptive-worker logic. Keep bounded-parallel **FETCH** only (Phase A, `NET_WORKERS≈8`, I/O-bound, no inner rayon). Peak RAM = one tile's mesh + the accumulator + all cached rasters.
2. **Forced zoom = source ceiling (z15) for every tile; reject tiles over `MAX_DEM_CELLS` at z15.** Do NOT let `pick_zoom` back off per tile (seam + coarseness trap). Constrain `tile_m` to ~250–1100 m (NOT 4000). This is the "true full resolution" policy; the `over_cell_budget` gate rejects a too-big tile pre-commit.
3. **Force `density_factor = 1` in grid mode.** `downsample` (`build.rs` `div_ceil`) is not edge-aware → breaks the shared-edge-pixel invariant. Full-res is the point. Omit density from `GridSettings`; `cell_m_eff = predicted_cell_m(centroid)` with no density multiply.
4. **World-extent bound is the i16 `ChunkIndex` (±2048×32767 ≈ ±67M units/axis), NOT i32.** A 28 km mosaic reaches ~270k units (0.4%), so this is a far-edge `debug_assert`, not a likely trip. Guard `(global_cells*size)/CHUNK_SIZE` fits i16.
5. **Drop ALL quadtree-path edits.** The GUI uses greedy exclusively. Touch ONLY `gen_opt_heightmap` (forward the 2 new params), `gen_greedy_heightmap`, `quads_to_bricks`. Smaller diff; protects the single-box identity tests.
6. **Add `base_override: Option<u32>` + `offset: (i32,i32)` to `generate_bricks` itself; reuse `BrickStyle::from_request` (no separate `generate_bricks_styled`).** Grid mode builds a synthetic `BuildRequest` per tile (needed to reuse `fetch_and_decode_dem`/`fetch_imagery` anyway). Single-box caller passes `(None, centered offset)`.
7. **Delete the stale destination before every `.brdb` write** (mirror brdb `examples/write_brz.rs` `remove_file`). Reason: avoid revision-history/blob growth + read-tree overhead — NOT brick duplication (reader filters `deleted_at`). Fix the test to assert on-disk size/revision count doesn't grow across N re-saves (a brick-count assertion proves nothing). `.brz` truncates (safe).
8. **Prefab install path UNVERIFIED — do NOT auto-install `.brz`.** Install only `.brdb` to `Worlds/`; write `.brz` to `builds_dir` for manual import until the in-game Brickadia `Prefabs/` path is confirmed (open decision).

## MUST-ADD (for it to be runnable)
- **Edge-contiguity regression test** (the #1 seam guard): make `lat_lon_to_world_px` `pub(crate)`; assert `round(world_px(shared_edge,z))` is equal from A-east and B-west AND A's last absolute column +1 == B's first.
- **EmptyDem / ocean-tile policy.** Phase A fetches all tiles first, so one bad tile aborts a multi-minute fetch. Choose: skip-and-leave-hole (documented seam at the hole) vs abort vs substitute-flat-at-global-min. Wire `decode_to_raster`'s empty guard into the orchestrator.
- **Two-phase = fetch-ALL-then-mesh-ALL** (global_min requires it). RAM precondition: all rasters resident (~`cells*4B`, ~186 MB for 616 tiles) is ADDITIVE to the mesh peak + accumulator in the estimate.
- **OpenTopography HARD-DISABLE for grid mode** (non-Mercator → aspect-distorted shape + per-day REST quota). Force AWS Terrarium / Mapbox (Mercator, z15).
- **Calibrate RAM-gate constants** (`size_of::<Brick>()` owned bytes, stitched-write peak ~2–4×) against a measured 1 km z15 tile before trusting the stitched gate (Phase C; gates stitched-only enablement).

## OPEN DECISIONS (confirm at/before implementation)
- **Does Brickadia Load append-merge multiple `.brdb` at stored offsets, or load each at origin?** Gates the keep-individual `.brdb` convention. Until answered: keep-individual ships as world-offset `.brz` prefabs (paste-join is well-defined) + stitched-only for `.brdb`. Does NOT block Phase A.
- Per-tile coordinate convention: world-offset (default) vs an origin-centered portable-prefab mode.
- Per-tile install target: stitched-only install (tiles to `builds_dir`) vs an "install tiles too" toggle.
- Confirm/tune host defaults: `MAX_GRID_BRICKS=50M`, fetch `NET_WORKERS≈8`, RAM reserve (~12 GB) on the 60 GB box.

---

## PHASED PLAN
> Apply BINDING CORRECTION #1 to every parallelism step below: tiles are SEQUENTIAL in the mesh phase; only FETCH is bounded-parallel.


### Phase A — MVP: seamless auto-grid → stitched .brdb (sequential, then parallel)
**Goal:** Prove true seamlessness end-to-end with the simplest path: Mode 1 auto-subdivide → one stitched .brdb world. Close all three correctness pillars. Single-box path stays byte-identical.

- Additive signature changes with single-box-preserving defaults: build_heightmap(raster, vertical_scale, global_min_m) replacing raster.min_m at build.rs:778; gen_greedy_heightmap/gen_opt_heightmap/quads_to_bricks gain base_height_override:Option<u32> + offset_x/offset_y, deleting the internal -(width*size) at generate.rs:388-389. Wire single-box callers to pass raster.min_m / None / -(width*size). Run existing tests — must stay green (single_box_offset_unchanged, build_heightmap_single_box_identity).
- Add forced_zoom:Option<u32> to fetch_bbox (tiles.rs:208); make lat_lon_to_world_px pub(crate); widen build.rs stage fns + DemRaster/DemHeightmap/FlatColormap/ImageColormap to pub(crate). Factor build_one_tile out of run_build (build.rs §4.4); run_build calls it.
- src/gui/grid.rs: GridPlan/TileId/PlannedTile + partition() for AutoSubdivide only (shared lon_edges/lat_edges lattice, forced zoom from centroid probe, derive_scale once). Duplicate haversine_km+EARTH_RADIUS_KM. world_offset() with the cumulative-cells + global-centering formula. Unit tests: partition_shares_exact_edges, world_offset_abutment, uniform_scale_across_tiles.
- run_grid_build: Phase A sequential fetch+decode all tiles + global_min_m; Phase B sequential mesh each tile (build_one_tile with global_min_m, base_override=Some(0), offset). Accumulate into one Vec<Brick>; bricks_to_save once; write ONE .brdb to builds_dir + install_to_worlds. Bypass MAX_BRICKS; add MAX_GRID_BRICKS=50M incremental cap. Tests: seam_boundary_columns_equal_in_z (+ negative control), combined_exceeds_max_bricks_allowed.
- Minimal UI in grid_ui.rs: grid_enabled toggle + tile_m DragValue + 'Build grid' button gated behind the collapsing header; mirror start_fetch worker thread + Promise + GridProgress(tiles_done/total) + cancel + poll_grid_promise; render GridOutcome list. No estimate dialog yet (or a read-only readout).
- Turn on bounded parallelism: NET_WORKERS=8 fetch pool (Phase A), tile_pool of MAX_TILE_WORKERS=4 (Phase B) over the global mesh pool; Arc<Mutex<Vec<Brick>>> accumulator. Verify determinism (output identical regardless of worker count). Visual seam check on a 2x2 Horsetooth grid (AWS Terrarium).

### Phase B — modes 2/3 + keep-individual-tiles + .brz prefab + estimate gate
**Goal:** Full grid-definition UX, all output combinations, and the pre-commit safety gate.

- src/util.rs: extract write_save_world(&World, out_file) from write_save (util.rs:169); write_save becomes a wrapper. Generalize build.rs install/path helpers: brickadia_saved_dir + saved_subdir(ext), unique_save_path(dir,stem,ext), install_save(path,ext,overwrite). Tests: write_save_world_roundtrip_brdb/brz, stale_brdb_delete_guard (delete-before-write), install routing.
- OutputOptions (in grid.rs) + validate() + draw_output_options UI (Formats: brdb/brz ; Layout: stitched/individual ; Install). Output layer in run_grid_build: stitched (bricks_to_save once) and/or per-tile (bricks_to_save per tile, pre-offset, _r{row}_c{col} naming). Memory routing: individual-only streams to disk (no accumulator); both → one per-tile World serves both formats + extend accumulator. Tests: output_options_validate, combined_one_spawn_many_individual, keep_individual_offset_parity, naming_zero_padded_sorts.
- partition() ClickMask + Explicit variants. Mode 2 overlay: draw_grid_overlay (paint lattice via paint_bbox Rect pattern, dim excluded) + update_grid_pick hit-test on the single map Response (gated !draw_mode; disable Draw Box in Overlay) + All/None/Invert. Mode 3: numeric cols/rows/anchor → analytic bbox → same partition path. Tests: plan_selection_drops_excluded_keeps_offsets, plan_explicit_center_vs_nw, tile_at_world_px_hit_test.
- estimate_grid (pure) + GridEstimate + draw_grid_estimate dialog: tiles/cells/bricks/peak-RAM/time + over_brick_cap/over_cell_budget/fits_ram disable + remedy text; available_ram_bytes() via /proc/meminfo. Gate the Grid Build button on the estimate before launching. Two-level progress bar + prompt cancellation. Tests: estimate_grid_deterministic, over_cell_budget_precheck, fits_ram_gate_flips, available_ram_bytes_parses_fixture, est_tile_mesh_bytes.
- OpenTopography guard: warn (or disable) grid mode for non-Mercator sources; surface the per-day-quota note in the estimate.

### Phase C — free-RAM-adaptive parallelism + estimate tuning
**Goal:** Fully exploit the 9950X3D/60 GB with self-limiting concurrency and calibrated estimates; harden large-grid runs.

- adaptive_worker_count: W = clamp((free*0.8 - reserve)/per_tile, 1, 4), reserve = 12GB + est_brick_vec_bytes; per_tile branches imagery (cells^1.5*40) vs flat (cells*64). Re-read MemAvailable per recompute. Test adaptive_worker_count_clamps + the imagery/flat branch split.
- Calibrate the estimate constants against a measured run: mesh+write a known 1 km z15 tile, watch RSS to fix size_of::<Brick>() owned bytes, the write-peak factor (to_unsaved sort + to_pending blobs), and k_fetch/k_mesh seconds. Refine est_seconds live from the first completed tile.
- i32/ChunkIndex bound: estimate bounds max|pos| = global_cells*size; warn/block if near i32::MAX; debug_assert in the offset path. Test the bound on a max-size mosaic.
- Harden large grids: HTTP 429 handling + clear error, WritingCombined progress phase so the GUI never looks hung during the multi-GB single combined write, and confirm the combine path prefers streaming per-tile output when stitched is off. Optional: refine est_bricks with the ~0.6 Horsetooth merge factor as the 'likely' readout while keeping the ceiling for the abort guard.

---

## DETAILED DESIGN (body — superseded by BINDING CORRECTIONS where they conflict)


# Grid / Tiled Large-World Generation — Consolidated Implementation Spec

**App:** `heightmap2brz-gui` (`/home/mastermind/Projects/brickadia/heightmap2brz`, git HEAD `8f230f6`).
**Goal:** subdivide a large drawn area into ~1 km tiles, mesh each at full resolution (bounded memory), and combine the bricks into one or more Brickadia outputs — overcoming the single-mesh `~O(cells^1.5)` memory wall while producing a true world of tens of millions of bricks. The existing single-box path stays byte-identical; this is purely additive.

All file:line references below were verified against the working tree and against `brdb-0.4.0` at `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/brdb-0.4.0`.

---

## 1. Overview & the three correctness pillars

Tiling has exactly three ways to break, and the engine must close all three. Every other concern (color, spawn, material, parallelism order) is provably seam-irrelevant (§7).

| Pillar | Today's behavior (verified) | Fix |
|---|---|---|
| **A. Geographic continuity** | `crop_window` (tiles.rs:385-409) rounds each bbox edge to an absolute world pixel and crops half-open `[x0,x1)`. | Build ONE shared lattice of edge lat/lons; adjacent tiles share the interior edge **by f64 value**, so `round(world_px)` is identical → last column of tile A and first column of tile B are **consecutive real DEM pixels**. |
| **B. Elevation datum** | `build_heightmap` normalizes `(m - raster.min_m)*vertical` per-raster (build.rs:778); `min_height` fill-floor is the per-tile present minimum (generate.rs:245). | Two-phase build: observe every tile's `min_m`, take `global_min_m`, normalize **all** tiles against it; pass `base_height_override = Some(0)` so every column fills to the same global floor. |
| **C. World placement** | `quads_to_bricks` centers EVERY build at origin via `offset = -(width*size)` (generate.rs:388-389). | Replace the internal centering with explicit `offset_x/offset_y` params; the grid passes a per-tile world offset (cumulative cell count × pitch) so tiles abut in exact integer units. |

The change is one new module (`src/gui/grid.rs`), one new UI module (`src/gui/grid_ui.rs`), and **surgical additive signature changes** to `build_heightmap`, `gen_greedy_heightmap`/`quads_to_bricks`/`gen_opt_heightmap`, plus a few visibility widenings in `build.rs` and `tiles.rs`. Every new parameter takes a default that **reproduces today's single-box behavior exactly** (`global_min_m = raster.min_m`, `base_override = None`, `offset = -(width*size)`).

---

## 2. Canonical data model (single source of truth for all lenses)

Everything flows through ONE plan type. The three UI modes are pure front-ends that produce a `GridPlan`; the orchestrator and the estimate consume only `GridPlan` + `GridSettings`.

```rust
// src/gui/grid.rs

/// Row 0 = NORTH, Col 0 = WEST. Matches Web-Mercator y-grows-south.
pub(crate) struct TileId { pub row: u32, pub col: u32 }

pub(crate) struct PlannedTile {
    pub id: TileId,
    pub bbox: BBoxLatLon,        // sub-box; shares edges with neighbors by value
    pub off_cells_x: u32,        // global NW cell index (sum of column widths to the west)
    pub off_cells_y: u32,        // global NW cell index (sum of row heights to the north)
    pub cells_w: u32,            // realized cropped width  (filled in Phase A)
    pub cells_h: u32,            // realized cropped height (filled in Phase A)
}

pub(crate) struct GridPlan {
    pub zoom: u32,               // SINGLE locked zoom for ALL tiles (pillar A/uniform pitch)
    pub cell_m: f64,             // ground m/cell at the locked zoom + grid-center lat
    pub horizontal_scale: u16,   // derive_scale() ONCE → identical `size` for every tile
    pub vertical_scale: f32,     // derive_scale() ONCE → identical affine Z map
    pub rows: u32, pub cols: u32,
    pub lon_edges: Vec<f64>,     // len = cols+1, monotonic, shared by value
    pub lat_edges: Vec<f64>,     // len = rows+1, monotonic (north→south)
    pub tiles: Vec<PlannedTile>, // included tiles only, row-major; excluded already dropped
    pub global_cells_w: u32,     // full mosaic extent (from edge deltas; for centering+estimate)
    pub global_cells_h: u32,
    pub name: String,
}

pub(crate) enum GridMode {
    AutoSubdivide { tile_m: f64 },                                  // mode 1
    ClickMask     { tile_m: f64, excluded: HashSet<TileId> },       // mode 2 (store EXCLUDED → default all-in)
    Explicit      { tile_m: f64, cols: u32, rows: u32, anchor: AnchorKind },  // mode 3
}
pub(crate) enum AnchorKind { NwCorner, Center }

pub(crate) struct GridSettings {            // the non-bbox, non-name shaping fields, stamped per tile
    pub dem_source: DemSource,
    pub imagery_source: ImagerySource,
    pub mapbox_token: Option<String>,
    pub opentopo_key: Option<String>,
    pub block_type: BlockType,
    pub glow: bool,
    pub no_collision: bool,
    pub output: OutputOptions,
    pub overwrite: bool,
}

pub(crate) struct OutputOptions {           // src/gui/grid.rs (no separate module — ponytail)
    pub brdb: bool, pub brz: bool,          // ≥1 true
    pub stitched: bool, pub individual: bool, // ≥1 true
    pub install_to_brickadia: bool,
}
impl Default for OutputOptions {            // legacy single-box defaults
    fn default() -> Self { Self { brdb:true, brz:false, stitched:true, individual:false, install_to_brickadia:true } }
}
```

`cw[c] = lon_edge_world_px[c+1]-lon_edge_world_px[c]` and `ch[r]` are the realized per-column/row cell widths; `off_cells_x = Σ_{j<c} cw[j]`. Because tiles share edges by value and the zoom is locked, `cw`/`ch` are deterministic and a tile's east column world-x equals its neighbor's west column world-x exactly.

---

## 3. Alignment engine (the paramount seamlessness requirement)

### 3.1 Pixel-snapped geographic partition (pillar A)

`lat_lon_to_world_px(lat,lon,zoom)` (tiles.rs:180-188) is a pure deterministic function; `crop_window` rounds each edge to an absolute world pixel and crops half-open. **Build the edge lattice ONCE and index it — never recompute a tile's edge independently** (float drift in the last f64 bit → `round(world_px)` differs by 1 → a duplicated/missing column = a faint seam; this is the single most likely regression).

```
// equal-fraction division guarantees every interior edge is shared by value
lon_edges[i] = lerp(big.west, big.east, i / cols)   for i in 0..=cols
lat_edges[j] = lerp(big.north, big.south, j / rows)  for j in 0..=rows  // north→south
tile(r,c).bbox = { west: lon_edges[c], east: lon_edges[c+1],
                   north: lat_edges[r], south: lat_edges[r+1] }
```

`cols = ceil(width_km*1000 / tile_m)`, `rows = ceil(height_km*1000 / tile_m)` using `haversine_km` (map_tab.rs:153). The remainder row/col is simply a thinner tile; its smaller `cw/ch` is handled by the offset math. Adjacent tiles share `lon_edges[c+1]`/`lat_edges[r+1]` as the same `Vec` element → identical f64 → identical `round(world_px)` → A's last column (abs px K−1) and B's first column (abs px K) are consecutive real DEM pixels.

`haversine_km` + `EARTH_RADIUS_KM` live in `map_tab.rs`; `grid.rs` is a sibling. **Duplicate the ~10 lines into `grid.rs`** rather than widen a public surface (ponytail: laziest correct, no speculative shared module).

### 3.2 Forced uniform zoom (pillar A, critical)

`pick_zoom` (tiles.rs:132) chooses zoom from a tile's OWN bbox, so a smaller remainder tile could pick a HIGHER zoom → finer cells → different `size` → seam. **Compute the zoom ONCE** from a representative single-tile probe (a `tile_m`-square box at the grid centroid) and force every fetch to it:

```
zoom = pick_zoom(probe_bbox_at_centroid, source.max_zoom())   // computed once, stored in GridPlan.zoom
```

Thread a `forced_zoom: Option<u32>` into the fetch path (§5). When `Some(z)`, `fetch_bbox` skips `pick_zoom` and uses `z` (still asserts `tile_count ≤ MAX_TILES_PER_FETCH`). `pick_zoom`/`crop_window`/`lat_lon_to_world_px` are otherwise UNCHANGED — their round-to-nearest half-open behavior is exactly what makes neighbors contiguous.

### 3.3 Uniform `derive_scale` (pillar A → uniform pitch)

`derive_scale(cell_m_eff, studs_per_meter, exaggeration, micro)` (map_tab.rs:1047) is pure. Compute `(horizontal_scale, vertical_scale)` **ONCE** from the grid-center latitude at the forced zoom and store in `GridPlan`; stamp the SAME values into every tile's `BuildRequest`. This makes `size = horizontal_scale * (5 normal | 1 micro)` (build.rs:826) bit-identical across tiles — required by the offset math. (`derive_scale` rounds `hscale` to int and clamps `[1,128]`, so sub-1% lat-driven `cell_m` variation could not change it anyway; computing once removes the risk entirely.)

### 3.4 Global elevation datum (pillar B)

Two-phase pipeline:

- **Phase A** — for each tile: `fetch_and_decode_dem` → `downsample(density)` → `enforce_cell_budget`. Cache the resulting `DemRaster` (cheap: `cells*4 B`). Record realized `cells_w/cells_h` into `PlannedTile`. Then `global_min_m = min over tiles of raster.min_m`, `global_max_m = max …` — O(tiles), no pixel re-scan (`min_m`/`max_m` already computed at decode/downsample, build.rs:758-759,711-735).
- **Phase B** — mesh each tile against the global reference.

Signature change (additive; single-box passes the per-raster value → byte-identical):
```rust
// src/gui/build.rs:773
fn build_heightmap(raster: &DemRaster, vertical_scale: f32, global_min_m: f32) -> DemHeightmap
//   normalized = (((m - global_min_m) * vertical_scale).max(0.0).round() as i64).max(0) as u32
//   single-box caller (run_build) passes raster.min_m
```

The fill-to-base floor must also be global. Once every tile is normalized against `global_min_m`, the global floor in brick units is **0** (the lowest real point maps to brick-Z 0 by construction). Thread an override:
```rust
// src/opt/generate.rs:218 (and gen_opt_heightmap:32 forwards it)
pub fn gen_greedy_heightmap<F>(.., base_height_override: Option<u32>, offset: Option<(i32,i32)>, ..)
//   let min_height = base_height_override.unwrap_or_else(|| pairs_vec.iter().map(|(h,_)| *h).min().unwrap_or(0));
//   tiled path: Some(0); single-box: None → unchanged
```
This flows unchanged into `quads_to_bricks`'s existing `((h - min_height).max(0) * scale/2).max(2)` fill (generate.rs:407-411): two tiles at the same real elevation now share both top-Z (global normalization) and base plane (floor 0) → identical column extents across the seam.

### 3.5 World offsets — exact abutment (pillar C)

Replace the internal centering in `quads_to_bricks` with passed offsets:
```rust
// src/opt/generate.rs:375
fn quads_to_bricks<F>(all_quads, options, width, height, min_height,
                      offset_x: i32, offset_y: i32, progress_f) -> ...
//   DELETE lines 388-389; use offset_x/offset_y at lines 421-426.
```

The grid offset (units), with uniform `size` and per-column/row cell counts `cw[c]`/`ch[r]`:
```
world_off_x(c) = (Σ_{j<c} cw[j]) * 2 * size = off_cells_x * 2 * size
world_off_y(r) = off_cells_y * 2 * size
GLOBAL_CENTER_X = -(global_cells_w * size)   // = -(global_cells_w * 2 * size)/2
GLOBAL_CENTER_Y = -(global_cells_h * size)
offset_x(c) = world_off_x(c) + GLOBAL_CENTER_X
offset_y(r) = world_off_y(r) + GLOBAL_CENTER_Y
```
Per-brick (unchanged structure, generate.rs:421-426):
```
pos_x = quad.x*size*2 + quad.w*size + offset_x(c)
pos_y = quad.y*size*2 + quad.h*size + offset_y(r)
```

**Single-box reduction (proves byte-identity):** one tile, `cw=[width]`, `off_cells_x=0`, `global_cells_w=width` ⇒ `offset_x = 0 + (-(width*size)) = -(width*size)` — exactly today's value. The single-box caller passes `-(width as i32 * size as i32), -(height as i32 * size as i32)`.

**Abutment proof:** tile c right edge (units) `= world_off_x(c) + cw[c]*2*size = world_off_x(c+1)` = tile (c+1) left edge; boundary cell-center pitch = `2*size` = interior pitch. Combined with §3.1 (consecutive real pixels) and §3.4 (same height → same Z), the boundary is indistinguishable from an interior column. All integer arithmetic (`size:u16`, counts `u32`, products fit `i32` within any sane grid).

**Apply offsets in-place, not via clone.** `Brick` is `Clone` but `clone()` drops `id` and re-boxes components (brick.rs:144-165). Terrain bricks have `id:None`/empty components so this is bounded, but the orchestrator owns each tile's `Vec<Brick>` and mutates `position` in place. The offsets are already baked in by `quads_to_bricks` for the stitched-from-mesh path, so combine is a literal `Vec::extend` with no re-positioning. (Equivalently, if a tile's bricks were meshed origin-centered, the orchestrator can `for b in &mut bricks { b.position += Position::new(dx,dy,0); }` using `AddAssign`, position.rs:76 — but the chosen design bakes the offset in the mesher to avoid a second pass.)

### 3.6 i32 / ChunkIndex bound (verified)

`Position` is `i32`; `to_relative` maps to `ChunkIndex{i16}` + `RelativePosition{i16}` with `CHUNK_SIZE=2048` (position.rs:29-44, brick.rs:463-467). World extent caps at `±2048*32767 ≈ ±67 M units/axis`. A 28 km mosaic at `studs_per_meter≈1` is ~5.6 M units — safe. But at high `studs_per_meter`/large `size` a big mosaic can approach the bound: the estimate (§6) bounds `max|pos| = global_cells * size` and warns/blocks if near `i32::MAX`. Add a `debug_assert!` in the offset path that emitted positions fit i32.

---

## 4. Orchestrator — `src/gui/grid.rs`

A SIBLING entry point to `run_build`, reusing build.rs's per-tile stages. Runs on ONE worker thread spawned by the GUI (poll-promise), exactly like `start_fetch` (map_tab.rs:781-793).

```rust
pub(crate) fn run_grid_build(
    plan: GridPlan,
    settings: GridSettings,
    progress: Arc<Mutex<GridProgress>>,
    cancel: Arc<AtomicBool>,
) -> Result<GridOutcome, GridError>;
```

### 4.1 Phase A — fetch + decode all rasters (network-bound, bounded-parallel)
- For each tile bbox: `build_one_tile_raster(tile, &settings, forced_zoom)` = `fetch_and_decode_dem` (with `forced_zoom`) → `downsample` → `enforce_cell_budget`. Record `cells_w/cells_h`, `min_m`.
- Parallelize with a local pool of `NET_WORKERS = 8` (`rayon::ThreadPoolBuilder::new().num_threads(8).build()`, `pool.install(|| tiles.par_iter()...)`). Each `fetch_bbox` builds its own `ureq::Agent` (tiles.rs:234) so concurrent fetches are independent.
- Cancellation: shared `Arc<AtomicBool>`; `fetch_bbox` checks it between tiles (tiles.rs:242).
- An all-nodata tile (ocean void / beyond provider data) decodes to `EmptyDem`. Treat as a hard `GridError::Tile{..}` (do NOT silently leave a hole that mismatches a populated neighbor); surface row/col.
- Compute `global_min_m`, `global_max_m`.

### 4.2 Phase B — mesh + accumulate (memory-bound, RAM-adaptive parallel)
Per tile: `build_heightmap(raster, plan.vertical_scale, global_min_m)` → `fetch_imagery_if_requested` (per-tile, cropped to the same sub-box → colors align per tile; satellite is continuous so the seam carries no worse a color step than any interior cell-to-cell change) → `generate_bricks(.., base_override=Some(0), offset=(offset_x(c),offset_y(r)))`.

**Concurrency model (avoid nested-rayon oversubscription).** `mesh_planes` already does `into_par_iter` on the GLOBAL rayon pool (generate.rs:354). Run tiles on a SMALL dedicated pool so each tile still fans its plane-mesh across the global 32-thread pool:
```
tile_pool = ThreadPoolBuilder::new().num_threads(W).build()
tile_pool.install(|| tile_jobs.par_iter().for_each(mesh_one_tile))
```
`W` is the **RAM governor, not the CPU governor** — the inner par_iter keeps all 32 hardware threads busy even at `W=2`.

```
W = adaptive_worker_count(free, per_tile, reserve):
  free     = available_ram_bytes() * 0.8     // headroom
  per_tile = est_tile_mesh_bytes(max cells among tiles)
  reserve  = 12 GB + est_brick_vec_bytes      // OS + egui + growing accumulator + write-time copy
  by_ram   = max(1, (free - reserve) / per_tile)
  W        = clamp(by_ram, 1, MAX_TILE_WORKERS=4)   // >4 the inner par_iter contention erases gains
```
On 60 GB this resolves to `W=4` for 1 km satellite tiles; flat (no-imagery) tiles are ~100× cheaper so stay at 4; only low-RAM or huge tiles drop toward 1.

`est_tile_mesh_bytes(cells)`:
- imagery selected: `cells^1.5 * 40` (documented model, tiles.rs:15-29; 75 625 cells → ~831 MB).
- imagery None (`FlatColormap`): `cells * 64` (unique planes collapse to ~unique heights) — branch on `settings.imagery_source == None`.

**Determinism is total** regardless of tile order/concurrency: every tile's output depends only on its raster + scalars (`global_min_m`, `size`, `vertical_scale`, `offset`) fixed before Phase B. Parallelism is purely scheduling and cannot affect seams.

### 4.3 Accumulation + aggregate cap
- Stitched accumulator: `Arc<Mutex<Vec<Brick>>>` reserved to `est_bricks`. Each tile meshes lock-free, builds its full `Vec<Brick>`, then takes the lock once to `extend` (the memmove). Contention negligible (one extend per tile).
- **Aggregate cap `MAX_GRID_BRICKS = 50_000_000`** (vs single-box `MAX_BRICKS = 2_000_000`, build.rs:41). The single-box `MAX_BRICKS` check (build.rs:342) MUST NOT gate the combined world — exceeding it is the whole point. After each tile's extend, if `acc.len() > MAX_GRID_BRICKS`, set cancel and return `GridError::TooManyBricks`. Each tile still respects `enforce_cell_budget` so no single tile OOMs the mesher. The pre-commit estimate predicts and blocks this before launch.

### 4.4 Build-time code reuse: factor `build_one_tile`
Factor the per-tile body of `run_build` into:
```rust
pub(crate) fn build_one_tile(
    request: &BuildRequest, raster: DemRaster,
    global_min_m: f32, offset: (i32,i32), base_override: Option<u32>,
    progress: ProgressFn, cancel: Arc<AtomicBool>,
) -> Result<Vec<brdb::Brick>, BuildError>;
```
`run_build` becomes the single-tile special case (`global_min = raster.min_m`, `forced_zoom = None`, `offset = -(width*size)`, `base_override = None`, then `MAX_BRICKS` check + `write_brdb` + `install_to_worlds`). This guarantees the existing path is preserved and keeps one mesher invocation site.

---

## 5. Component / file-change map (cite real anchors)

**`src/gui/grid.rs` (NEW)** — orchestrator + planner + estimate + output. Holds `TileId`, `PlannedTile`, `GridPlan`, `GridMode`, `AnchorKind`, `GridSettings`, `OutputOptions`, `GridProgress`, `GridPhase`, `GridEstimate`, `GridOutcome`, `GridError`. Functions: `partition(big, mode, dem_source, src_max_zoom, studs_per_meter, exaggeration, micro) -> GridPlan`; `tile_bbox(plan,r,c)`; `world_offset(plan,c,r,size) -> (i32,i32)`; `run_grid_build(...)`; `build_one_tile_raster(...)`; `mesh_and_accumulate(...)`; `estimate_grid(&GridPlan,&GridSettings, available_ram) -> GridEstimate` (pure); `available_ram_bytes() -> Option<u64>`; `adaptive_worker_count(...)`; `est_tile_mesh_bytes(cells)`; `write_outputs(...)`. Duplicate `haversine_km`+`EARTH_RADIUS_KM` here.

**`src/gui/grid_ui.rs` (NEW)** — all egui: `draw_grid_section`, `grid_auto_controls`, `grid_overlay_controls`, `grid_explicit_controls`, `draw_output_options`, `draw_grid_estimate`, `draw_grid_build_button`, `recompute_grid_plan`, `draw_grid_overlay` (paint lattice + clickable cells), `update_grid_pick`, `start_grid_build`, `poll_grid_promise`, `grid_disabled_reasons`, `source_max_zoom_for(dem)`. (Splitting UI from logic keeps `grid.rs` pure/testable; ponytail-acceptable since the UI surface is large.)

**`src/gui/build.rs`** (MODIFY):
- `build_heightmap` (773): add `global_min_m: f32` param, replace `raster.min_m` at 778. Single-box caller (run_build 308) passes `raster.min_m`.
- Factor `build_one_tile` (§4.4); `run_build` calls it.
- Widen to `pub(crate)`: `fetch_and_decode_dem` (393), `downsample` (701), `fetch_imagery_if_requested` (637), `generate_bricks` (808), `enforce_cell_budget` (282), `DemRaster` (682), `DemHeightmap` (988), `FlatColormap` (1006), `ImageColormap` (1024), `BlockType` (already pub(crate)), `bbox_area_km2` (already pub(crate)). Add `pub(crate) fn generate_bricks_styled(heightmap, colormap, BrickStyle, base_override, offset, progress, cancel)` (or extend `generate_bricks` signature with `base_override`/`offset`) so grid stamps style from `GridSettings` without a `BuildRequest`.
- Generalize install/path helpers: `brickadia_worlds_dir` (930) → `brickadia_saved_dir()` returning the `…/Saved` root + `saved_subdir(root, ext)` (Worlds/ for brdb, Prefabs/ for brz); `unique_world_path` (904) → `unique_save_path(dir, stem, ext)`; `install_to_worlds` (874) → `install_save(path, ext, overwrite)`. Expose `builds_dir` (922), `sanitize_name` (976), the install helpers as `pub(crate)`.
- `MAX_BRICKS` stays as the PER-TILE/single-box cap; do NOT apply to the combined accumulator.

**`src/gui/tiles.rs`** (MODIFY):
- `fetch_bbox` (208): add `forced_zoom: Option<u32>`; when `Some(z)` skip `pick_zoom`, use `z`, still assert `tile_count ≤ MAX_TILES_PER_FETCH`. Single-box callers pass `None`.
- Make `lat_lon_to_world_px` (180) `pub(crate)` (planner/tests project corners). `pick_zoom`, `approx_cell_count`, `tile_count`, `tile_range`, `BBoxLatLon`, `MAX_DEM_CELLS`, `MAX_TILES_PER_FETCH` already reachable/`pub(crate)`. `crop_window`/`lat_lon_to_tile` UNCHANGED.

**`src/opt/generate.rs`** (MODIFY): `gen_greedy_heightmap` (218), `gen_opt_heightmap` (32), `gen_quad_heightmap` (48) gain `base_height_override: Option<u32>` + `offset: Option<(i32,i32)>` forwarded through. `quads_to_bricks` (375) takes `offset_x:i32, offset_y:i32`, delete 388-389. `quadtree_to_bricks`/`gen_quad_heightmap` pass their existing centering to keep parity. `emit_column_bricks` UNCHANGED.

**`src/util.rs`** (MODIFY, minimal): `bricks_to_save` (66) UNCHANGED. Add `pub fn write_save_world(world: &World, out_file: &str) -> Result<(), String>` factoring the extension dispatch from `write_save` (169-183); `write_save` becomes a thin wrapper (`bricks_to_save → write_save_world`). The grid output layer calls `write_save_world` for both stitched and per-tile.

**`src/gui/mod.rs`** (MODIFY): add `mod grid;` and `mod grid_ui;` next to `mod build;` (line 14). (Submodules are private `mod` declarations; no `pub use` needed.)

**`src/gui/map_tab.rs`** (MODIFY): add grid state to `MapTabState` (170-210) + `MapTabState::new` (213): `grid_enabled: bool`, `grid_mode: GridMode`, `grid_excluded: HashSet<TileId>`, `grid_explicit: (u32,u32,AnchorKind)`, `grid_tile_m: f64`, `grid_output: OutputOptions`, `grid_plan: Option<GridPlan>`, `grid_estimate: Option<GridEstimate>`, `grid_promise: Option<Promise<Result<GridOutcome,GridError>>>`, `grid_progress: Arc<Mutex<GridProgress>>`, `grid_cancel: Arc<AtomicBool>`, `grid_outcome`, `grid_error`. `draw_controls` (310) calls `grid_ui::draw_grid_section` AFTER `draw_fetch_button` (678), behind a collapsing header + `grid_enabled` gate (single-box path untouched when off). `draw_map_area` (1077) calls `update_grid_pick` after `update_bbox_drag` when `grid_enabled && Overlay && !draw_mode`. `draw_bbox_overlay` (1192) gets an additive grid-paint branch. `draw`/poll (288-297) add `poll_grid_promise` + `request_repaint_after` while `grid_promise.is_some()`. `cancel_fetch` (266) also stores `grid_cancel`. `derive_scale` (1047) UNCHANGED, reused once per grid.

---

## 6. Pre-commit estimate (pure, unit-testable, the headline safety gate)

`estimate_grid(&GridPlan, &GridSettings, available_ram: u64) -> GridEstimate` (no I/O except an injectable `available_ram`):
```rust
pub(crate) struct GridEstimate {
    pub tile_count: u32, pub total_cells: u64, pub est_bricks: u64,
    pub peak_mesh_bytes: u64, pub est_brick_vec_bytes: u64,
    pub parallel_tiles: u32, pub est_seconds: f64,
    pub over_brick_cap: bool,   // any single tile > MAX_BRICKS (per-tile mesh must fit)
    pub over_cell_budget: bool, // any tile cells_per_tile > MAX_DEM_CELLS (pre-check, no mid-build GridTooLarge)
    pub fits_ram: bool,         // peak_mesh + brick_vec ≤ available - reserve at parallel_tiles≥1
}
```
Formulas (all grounded):
- `total_cells = Σ approx_cell_count(tile.bbox, plan.zoom)` (tiles.rs:195 — the same path the fetch walks; pin in tests).
- `est_bricks = total_cells` (conservative ceiling — greedy meshing only REDUCES count; fill-capped terrain ≤ ~1 brick/cell). A `~0.6` Horsetooth merge factor MAY be shown as the "likely" number, but the abort guard uses the ceiling.
- `peak_mesh_bytes = parallel_tiles * est_tile_mesh_bytes(max tile cells)` (NOT the sum — concurrency-bounded).
- `est_brick_vec_bytes = est_bricks * size_of::<Brick>()` (~120-160 B owned; **calibrate by RSS** — see open decisions) × a write-peak factor (~2-4×, since `to_unsaved` sorts ALL bricks via `itertools sorted_by`, save.rs:120, and `to_pending` holds all blobs).
- `parallel_tiles = adaptive_worker_count(...)`.
- `est_seconds = ceil(tiles/NET_WORKERS)*k_fetch + ceil(tiles/parallel_tiles)*k_mesh(max cells)`, `k_*` named consts seeded from Horsetooth (~70 k-brick build), refined live from the first tile. Labeled "~".

The estimate dialog (egui::Window) shows tiles, NxM @ zoom, m/cell, est bricks, est peak RAM across `parallel_tiles` of `free` GB, est time. Confirm button is DISABLED with remedy text when `over_brick_cap` ("a single tile exceeds the {MAX_BRICKS} cap — raise Density or lower exaggeration"), `over_cell_budget` ("a tile exceeds {MAX_DEM_CELLS} cells — smaller tile size or more Density"), or `!fits_ram` ("not enough free RAM — use keep-individual-only, fewer/finer tiles"). Mirrors `draw_output_estimate` warning style (map_tab.rs:980-1003).

`available_ram_bytes()`: read `/proc/meminfo`, find `MemAvailable:`, `value_kB * 1024`. **Zero-dep** (`std::fs::read_to_string` + line scan; do NOT add `sysinfo` for one number — ponytail; Linux-only is fine, the app targets Wayland/Arch). `None` on any failure → conservative fallback `W=2`, and the estimate uses a conservative free figure.

---

## 7. Output layer (.brdb world / .brz prefab × stitched / per-tile)

**brdb 0.4 write API (verified):** on `World` — `write_brdb(path)` (world.rs:30), `write_brz(path)` (world.rs:36), `to_brz_vec()` (world.rs:42). `bricks_to_save(Vec<Brick>) -> World` (util.rs:66) builds the World and injects exactly ONE `B_SPAWN_POINT` at the centroid above the peak over whatever set it's given.

**CRITICAL — `.brdb` is open-if-exists + append.** `write_brdb` → `Brdb::new(path)` which, if `path.exists()`, calls `Brdb::open` (NOT truncate) and `save` appends a revision (mod.rs:65-72, 99-102). **Therefore delete any stale destination file before each `.brdb` write** (mirror `examples/write_brz.rs:16-18`) so a re-run yields a clean single-revision world. `.brz` uses `File::create` (truncates, brz/mod.rs:227-228) — safe.

**Four sinks**, all from a `Vec<Brick>` via `bricks_to_save` then `write_save_world(&world, path)` (§5 util change) which dispatches by extension. The CLI's `write_save` (util.rs:169) proves both formats work; reuse it (no shelling out, no hand-rolled `to_brz_vec`+`fs::write` — `write_brz` already does file creation).

**Spawn-point rule:**
- **Stitched:** accumulate raw offset `Vec<Brick>` for ALL tiles, then `bricks_to_save` **ONCE** → one spawn at the global centroid. Never per-tile then merge Worlds (would give N spawns).
- **Per-tile:** `bricks_to_save` per tile → one spawn each (correct for a standalone tile you open to inspect).

**Per-tile coordinate convention (DECISION):** each per-tile file is written **pre-offset to its world position** (same `offset` as stitched), NOT origin-centered. Rationale: (1) a user who exports both gets tiles that drop into the exact same spot; (2) `.brz` prefabs paste-and-join reconstruct the seamless whole; (3) loading several tile worlds (if Brickadia append-loads at stored offset) re-forms the terrain in place. (See open decisions for the in-game `.brdb`-append confirmation.)

**Memory routing:**
- `individual && stitched`: per tile — accumulate the offset bricks (move into accumulator), AND write the per-tile file. To avoid a full clone, build the per-tile `World` from the same offset `Vec<Brick>` once (both `write_brdb` and `write_brz` take `&self`, so one World serves both formats), then move/extend into the accumulator. One bounded clone per tile at most.
- `individual && !stitched`: never accumulate — stream each tile straight to disk. **This is the bounded-RAM path that makes 28 km feasible** (peak = one tile's mesh, no 50 M-brick Vec).
- `stitched && !individual`: accumulate only; the single combined write is the RAM peak (gated by the estimate).

**Install:** write to `builds_dir()` always; if `install_to_brickadia`, `install_save(path, ext, overwrite)` → Worlds/ for brdb, Prefabs/ for brz, non-fatal per file (degrade to a warning, the file remains in `builds_dir`; mirror build.rs:359-376). `unique_save_path` gives `<stem>.<ext>` or `<stem>-N.<ext>` (bounded 2..=1000).

**Naming:** `width = digits(max grid index)`; `format!("{stem}_r{row:0w$}_c{col:0w$}")` (e.g. `_r00_c03`) so files sort in grid order. `sanitize_name` (build.rs:976) applied to the stem. Stitched uses `output_name`.

```rust
pub(crate) struct GridOutcome {
    pub written: Vec<PathBuf>, pub installed: Vec<PathBuf>,
    pub warnings: Vec<String>, pub brick_count: usize, pub tiles: u32,
}
```
`poll_grid_promise` + `draw_last_result` render the written/installed file LIST + per-file warnings (extend map_tab.rs:819).

---

## 8. UI — three grid modes (best-practice UX, additive)

A collapsing "Grid build (large worlds)" section under the single-box controls, behind `grid_enabled` (default off → single-box flow visually unchanged). All three modes reduce to a `GridPlan`; `recompute_grid_plan` runs each frame (pure, cheap) when inputs change.

- **Mode 1 — AutoSubdivide:** user draws one big box (existing `bbox`/drag), picks `tile_m` (DragValue, default 1000, 250..=4000). Live readout "N×M = T tiles @ z{zoom}, ~{cell_m} m/cell". One "Build grid" button → estimate dialog → worker.
- **Mode 2 — ClickMask (overlay pick):** draw box → paint the lattice (`draw_grid_overlay` projects each tile bbox via the existing `paint_bbox` Rect pattern, map_tab.rs:1247-1254; excluded cells dimmed/hatched, included get the thin stroke). Toggle membership by hit-testing the pointer against projected cell rects in `update_grid_pick`, which runs ONLY when `grid_enabled && Overlay && !draw_mode` so box-draw and tile-pick never fight for the pointer (honoring the file's single-Response warning at map_tab.rs:1090-1098 — reuse the map `Response`, add NO second interact widget; disable/auto-exit Draw Box when entering Overlay). "All / None / Invert" buttons + hover highlight; optional shift-drag range-select. Store EXCLUDED tiles so default = all-in.
- **Mode 3 — Explicit NxM:** numeric `tile_m` + cols (1..=32) + rows (1..=32) + anchor ComboBox {NW corner, Center}; anchor point from the box NW/centroid or the coord box. Derive the area bbox analytically (`dlon = tile_m/(111320*cos(lat))`, etc.), then the SAME partition path. Paint the resulting grid so the user sees where it lands. No big-box draw required.

A separate "Grid Build" button (distinct from the single-box "Fetch & Build") opens the estimate dialog, then `start_grid_build` spawns the worker (mirror `start_fetch` 781-793, sharing the Promise+thread+cancel machinery). Progress: `GridProgress { phase, tiles_done, tiles_total, current: TileId, stage: BuildStage, stage_fraction }` behind `Arc<Mutex<>>`, polled like `fetch_progress`; two-level bar (overall `(tiles_done + stage_fraction)/tiles_total` + per-tile `BuildStage::label`). Cancel sets `grid_cancel` (checked between fetches, at each tile job top, and inside `generate_bricks`' existing cancel closure, build.rs:844). On cancel: tile pool drains, partial output discarded → `GridError::Cancelled` (matches single-box contract).

---

## 9. Seam-irrelevant audited state (no other per-tile coupling)

- **Spawn point:** §7 (once for stitched, per-tile for individual). Touches nothing on the terrain grid.
- **Color/material:** `emit_column_bricks` (generate.rs:459-512) sets material from `options.glow` and color per-cell from the colormap — all per-cell; `glow`/`nocollide`/`block_type` come from shared `GridSettings`. No seam impact.
- **`base_height()`** (util.rs:34-43) depends only on `stud`/`micro` (from `block_type`) — uniform → the `position.z = base_height()-5 + (z - brick_height)` term is a shared constant offset.
- **`World::add_bricks`** (world.rs:98) only `extend`s the main `bricks` Vec — NO auto position shift (the `-CHUNK_HALF` shift at world.rs:104-110 is `add_brick_grid`, a different method for dynamic sub-grids we never use).
- **OpenTopography** is a geographic (non-Mercator) grid → aspect-distorted away from the equator (`predicted_cell_m` geometric-mean, map_tab.rs:1025). Tiling aligns seams (same datum + abutment) but SHAPE is stretched. **Recommend/force AWS Terrarium or Mapbox (isotropic Mercator) for grid mode; warn if OpenTopography is selected.** Also: each tile is its own OpenTopography REST request (per-request 450 000 km² cap is fine per tile, but the per-day quota could bite a large grid — warn in the estimate). Antimeridian spans are already rejected by `BBox::from_corners` (map_tab.rs:109-111) — out of scope, inherited.

---

## 10. Test strategy (all runnable as `#[cfg(test)]`)

**Alignment (pillar A/B/C):**
- `partition_shares_exact_edges`: a 3×2 partition has `tile(r,c).east == tile(r,c+1).west` and `tile(r,c).south == tile(r+1,c).north` as **exact f64 ==** for every interior edge.
- `crop_contiguity_world_px`: for two tiles sharing a meridian at a fixed zoom, `round(lat_lon_to_world_px(edge,z).0)` from A's east == from B's west, and A's last abs column +1 == B's first abs column. (May make `lat_lon_to_world_px` `pub(crate)`.)
- `seam_boundary_columns_equal_in_z`: two rasters whose shared edge column has identical heights but different per-raster `min_m` (left valley / right ridge) → equal edge Z **only** with `global_min_m`; assert they DIFFER with per-raster `min_m` (regression lock for build.rs:778).
- `single_box_offset_unchanged`: `quads_to_bricks` with `offset = -(width*size), -(height*size)` over a fixed quad set is byte-identical to pre-change output.
- `build_heightmap_single_box_identity`: `build_heightmap(r, v, r.min_m)` equals the existing `build_heightmap_normalizes_to_zero_min` fixture (build.rs:1088-1100).
- `world_offset_abutment`: tile(0,0) and tile(0,1) at their prefix-sum offsets place tile1's min-X brick exactly `2*size` past tile0's max-X (shared edge world-x coincides).
- `uniform_scale_across_tiles`: `derive_scale` for every tile's lat band yields ONE `(hscale, vertical)` → all tiles share `size`.

**Orchestrator/estimate (pure):**
- `estimate_grid_deterministic`: a 2×2 of 1 km tiles → `tile_count==4`, `total_cells == 4*approx_cell_count(one tile)` (pinned integer), `est_bricks==total_cells`, `fits_ram==true` at 60 GB.
- `adaptive_worker_count_clamps`: `(60e9, 831e6, 12e9)→4`; `(4e9, 831e6, 2e9)→2`; `(1e9, 2e9, 0)→1`; free==reserve→1.
- `est_tile_mesh_bytes`: `(75625)` within 1% of `75625^1.5*40`; flat branch `(75625)==75625*64`, >100× smaller.
- `fits_ram_gate_flips`: a 200-tile satellite grid at `available=8e9` → `fits_ram==false`.
- `over_cell_budget_precheck`: `tile_m=4000 @ z15` → `over_cell_budget==true` (UI blocks before launch, no mid-build `GridTooLarge`).
- `combined_exceeds_max_bricks_allowed`: a plan implying >2 M bricks does NOT return `TooManyBricks` from `run_grid_build`'s combined path, while a single tile over its own budget still errors (`enforce_cell_budget`).
- `available_ram_bytes_parses_fixture`: `"MemAvailable:   12345678 kB" → 12345678*1024`; missing line / garbage → None.

**Output:**
- `write_save_world_roundtrip_brdb`: 3 bricks → temp `.brdb`, reopen via `brdb::Brdb::open_readonly`, assert 3+1 (spawn) bricks + a known position.
- `write_save_world_roundtrip_brz`: 3 bricks → temp `.brz`, reopen via `brdb::Brz::open(path)?.into_reader()` (brz/mod.rs:107,149), assert a brick's color/position.
- `stale_brdb_delete_guard`: write a `.brdb`, then write the SAME path again (via the orchestrator's delete-then-write) → reopened world has the expected single-revision brick count (proves no revision pile-up).
- `combined_one_spawn_many_individual`: stitched output has exactly one `B_SPAWN_POINT` over the union; per-tile has one per tile; stitched tile-B bricks all have `x ≥ W`.
- `keep_individual_offset_parity`: a 1×2 grid — reopen r0c0 and r0c1, assert `c1.min_x == c0.min_x + W`.
- `output_options_validate`: `Err` on empty formats / empty layout; `Ok` on each non-empty combo; `Default == legacy` (brdb+stitched+install) and a 1×1 grid with Default yields exactly one `<name>.brdb`.
- `naming_zero_padded_sorts`: 2×10 grid → `_c00.._c09`, lexicographic == grid order.

---

## 11. Definition of Done
- `cargo build` (gui feature) + `cargo test` green, including the new alignment/estimate/output tests AND the existing suites (single-box identity tests must pass unchanged).
- Single-box "Fetch & Build" produces a byte-identical `.brdb` to pre-change for a fixed bbox/settings (offset extraction + `global_min=raster.min_m` are no-ops on that path).
- A 2×2 auto-grid over Horsetooth (AWS Terrarium) stitches into one `.brdb` that loads in Brickadia with NO visible step at any seam (visual confirmation on a 2×2 before trusting large grids).
- The estimate dialog blocks an over-RAM / over-cell-budget / over-per-tile-brick-cap plan with the correct remedy text, before any heavy fetch.


---

## CROSS-CUTTING RISKS

- Float drift in the partition is the #1 seam regression: any tile edge recomputed independently (e.g. west + tile_m/111320 per tile) instead of read from the shared lon_edges/lat_edges Vec can differ in the last f64 bit → round(world_px) differs by 1 → a duplicated/missing DEM column = faint seam. MUST build the edge lattice once and index it; enforce with partition_shares_exact_edges (exact f64 ==).
- Per-tile min normalization (build.rs:778) and per-tile fill-floor (generate.rs:245) are the dominant vertical-seam bugs. Both MUST become global (global_min_m + base_override=Some(0)) or adjacent tiles step at the seam. Regression-locked by seam_boundary_columns_equal_in_z with a per-raster-min negative control.
- Per-tile pick_zoom drift: a smaller remainder tile could pick a higher zoom → finer cells → different size → seam. Forcing one zoom (GridPlan.zoom) for every fetch is mandatory; uniform_scale_across_tiles guards the scale half.
- .brdb is open-if-exists + append (Brdb::new mod.rs:65-72, save mod.rs:99-102) — re-running an output name onto a leftover file piles revisions. The tiled path writes brdb directly to multiple names and CAN hit this; delete the stale destination before every .brdb write (stale_brdb_delete_guard). .brz truncates (safe).
- Two-phase requires ALL tiles fetched before ANY meshing (to know global_min_m), so peak fetch state holds all rasters (cheap: cells*4B, ~186 MB for a 28x22km/616-tile grid). The estimate's RAM model assumes only `parallel_tiles` rasters/meshes are resident — a naive 'hold all meshes' executor would blow it; only rasters are cached, meshes are the bounded W-concurrent hog.
- MAX_BRICKS (build.rs:41) must gate only the per-tile/single-box path, never the combined accumulator (exceeding it is the feature). MAX_GRID_BRICKS=50M is the combined guard. Forgetting the bypass would falsely reject every large grid; forgetting per-tile enforce_cell_budget would let one tile OOM the mesher.
- Nested rayon: meshing tiles on the global pool while mesh_planes also uses the global pool (generate.rs:354) causes cache-thrash + memory spikes if W is large. Dedicated tile pool sized W<=4; do NOT raise MAX_TILE_WORKERS without measuring under per-pixel-unique imagery.
- The combined .brdb write is single-threaded + all-in-RAM: to_unsaved sorts ALL bricks (itertools sorted_by, save.rs:120), to_pending holds all blobs (~3-4x the Vec<Brick> footprint). A 50M-brick stitch can OOM even though each tile meshes fine and Brickadia loads it. Mitigate via the pre-commit RAM gate for stitched + always-available keep-individual streaming; add a WritingCombined progress phase so it doesn't look hung.
- Spawn-point duplication: bricks_to_save injects one spawn per call (util.rs:74). Stitched MUST merge raw Vec<Brick> and call bricks_to_save exactly once; per-tile calls per tile. Reusing per-tile Worlds for the stitch would litter N spawns.
- OpenTopography (geographic, non-Mercator) yields aspect-distorted square bricks for non-square cells; tiling aligns seams but not SHAPE. Recommend/force AWS Terrarium or Mapbox for grid mode + warn; also surface the per-day quota for many per-tile REST requests.
- egui pointer contention (Mode 2): tile-pick and box-draw both read the single map Response (the file warns at map_tab.rs:1090-1098 that two interact widgets over one rect both claim the drag). Gate pick on !draw_mode and disable/auto-exit Draw Box in Overlay; add NO second interact widget.
- Module/visibility churn: widening ~10 build.rs items + lat_lon_to_world_px to pub(crate) and adding two params across the generate.rs call chain touches several signatures. Keep the diff additive and default-preserving; the single-box identity tests are the safety net that the refactor changed nothing observable.
