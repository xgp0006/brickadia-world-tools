# Sculpt Terrain Editor (Brickadia-World-Tools) — Design Spec

Project: `~/Projects/brickadia/heightmap2brz` (app being rebranded **Brickadia-World-Tools**) · Status: **design approved, ready for writing-plans → ultracode** · Approach **A** (Sculpt tab + shared `HeightField`, real seams for future features).

## Goal

Add a **Sculpt** workspace: a WorldPainter-style brush-based terrain height editor, rebuilt in Rust inside the existing app. You sculpt a heightmap (from a blank canvas, an imported real-world DEM, or a loaded heightmap image), then feed it into the existing brick-conversion pipeline. This is MVP **#1 (height sculpting)** of a planned arc; #2 (color painting), #3 (live brick preview + GPU), and prefab-scatter are explicitly out of this spec but seamed for.

## Non-goals (each a later spec)
- Color/material painting (#2)
- Live brick / 3D preview, GPU acceleration (#3)
- Prefab-scatter "layers" (categorized `.brz` placement)
- Full WorldPainter layers: biomes, water, trees, caves, snow, procedural generation

---

## 1. Workflow & entry points

Three ways to get a `HeightField` into the Sculpt tab:
1. **Send to Sculpt** — a button on the Map tab. Reuses `fetch_and_decode_dem` on a worker thread (the existing fetch machinery, `start_fetch` pattern), stops at the `DemRaster`, builds a `HeightField`, switches to the Sculpt tab with it loaded. Carries bbox + scale metadata for convert.
2. **New blank canvas** — set grid size (cells W×H, bounded by the existing `MAX_DEM_CELLS = 200_000`) and cell size (ground m/cell). All cells start at the floor (0). Author from scratch.
3. **Load heightmap image** — open a grayscale/16-bit PNG as a `HeightField` (reuse the existing heightmap decode path where possible).

Then: brush to shape the terrain → **Convert to bricks** runs the existing `build_heightmap` → `generate_bricks` → write/install pipeline unchanged (single-box semantics: `.brdb` → Worlds/, optional `.brz` → Prefabs/).

---

## 2. Floor model (native Brickadia ground) — a first-class invariant

- Height `0.0` **is** the native Brickadia flat ground plane. `FLOOR_M = 0.0`.
- **Every brush clamps cells to `>= FLOOR_M`.** You can only build *up*; the lowest point is always the floor; nothing carves beneath it. Lower/Flatten/Set bottom out at 0.
- On convert: the global minimum is the floor (0), so `build_heightmap` normalizes against 0 and the watertight fill-to-base fills each column from its surface **down to the floor**, base flush with the native ground.
- **Floor (zero-height) columns emit no bricks** — the native world floor serves as the floor, so a blank canvas converts to the plain Brickadia world until terrain is raised. This is an **additive `GenOptions.skip_floor` flag, default `false`** (existing single-box/grid output stays byte-identical — those paths never skip), set **`true` by the sculpt/blank-canvas convert** so flat areas reveal the native floor instead of a redundant plate. Mechanically: when `skip_floor`, a column whose normalized height is `0` emits nothing; non-zero columns fill exactly as today.
- A DEM sent to Sculpt and converted **with the same options as a direct map build** (`skip_floor = false`) is byte-identical to it — the identity guard (§10) proves the `HeightField` round-trip is lossless independent of the floor-skip cosmetic.
- Alignment to the native ground plane Z is confirmed by in-game check on first build (DoD).

---

## 3. Data model — `HeightField` (`src/gui/sculpt/heightfield.rs`)

```
pub(crate) struct HeightField {
    pub width: u32,
    pub height: u32,
    pub cells: Vec<f32>,        // meters above floor, row-major, length width*height, all >= FLOOR_M
    pub meta: FieldMeta,        // convert + scale metadata
}

pub(crate) struct FieldMeta {
    pub cell_m: f64,            // ground meters per cell (pitch)
    pub studs_per_meter: f32,
    pub vertical_exaggeration: f32,
    pub micro: bool,
    pub centroid_lat: f64,      // for derive_scale (lat-correct pitch); blank canvas uses 0.0 / a default
    pub source_name: String,    // for the output filename stem
}
```

- `FLOOR_M: f32 = 0.0` constant; the model's hard lower bound.
- Constructors: `from_dem(&DemRaster, FieldMeta)`, `flat(w, h, FieldMeta)` (all FLOOR_M), `from_image(&GrayImage, FieldMeta)`.
- Accessors: `at(x,y)`, `set(x,y, v)` (clamps `>= FLOOR_M`), `sample_bilinear(fx,fy)`, `min_max() -> (f32,f32)`.
- `apply(&mut self, op, brush, center)` — dispatches a `Tool` over the affected rect (see §4).
- `to_dem_raster(&self) -> DemRaster` — produces the converter's input so `build_heightmap`/`generate_bricks` consume it **unchanged**. This is the Sculpt→Convert seam (where #3's incremental re-mesh taps).
- This is "layer 0"; a future color layer is a sibling buffer with the same dimensions (the #2 seam). Nothing here blocks it.

---

## 4. Tools & brushes (`src/gui/sculpt/brush.rs`, `tools.rs`)

```
pub(crate) trait Tool {                      // seam: color/scatter tools are future impls
    fn apply(&self, f: &mut HeightField, brush: &Brush, center: (f32, f32));
}

pub(crate) struct Brush {
    pub shape: BrushShape,                    // Circle now; enum prepped for Square/custom
    pub radius_cells: f32,
    pub strength: f32,                        // per-dab magnitude (meters for raise/lower; blend factor 0..1 for smooth)
    pub falloff: Falloff,                     // Smoothstep | Linear | Constant -> weight(0..1) by normalized distance
}

pub(crate) enum BrushShape { Circle }
pub(crate) enum Falloff { Smoothstep, Linear, Constant }
```

MVP height tools (all clamp `>= FLOOR_M`):
- **Raise** — `cell += strength * weight`.
- **Lower** — `cell -= strength * weight`, clamped at floor.
- **Smooth** — blend each cell toward its local neighborhood mean by `strength * weight` (box/gaussian kernel).
- **Flatten / Level** — pull toward a `target` height (sampled at stroke start, or a UI value) by `strength * weight`.
- **Set-height** — drive toward an absolute `target` by `strength * weight`.

Application:
- A stroke is a sequence of **dabs** along the pointer path (spaced ~`radius * spacing`). Each dab touches only the bounded `[cx-r, cx+r] × [cy-r, cy+r]` sub-rect → O(radius²); microseconds on one CPU core at MVP sizes. `rayon` is available if a large canvas ever needs it, but not required for MVP.
- Determinism: a given (op, brush, center, field) always yields the same result.

---

## 5. Rendering & feel (`src/gui/sculpt/sculpt_tab.rs`)

- The `HeightField` renders to an `egui` texture: a hypsometric colormap + hillshade (so relief is visible). The `ColorImage` is regenerated **only when cells change** (dirty flag), then uploaded; steady frames reuse the texture.
- Native DEM-grid resolution — what you sculpt is what converts (no resample round-trip).
- **Brush cursor is an egui overlay** (a circle following the pointer), fully decoupled from the grid, so radius/shape **animate smoothly at 60fps regardless of canvas size** — this is the "smoothly animate brush sizing/shapes" requirement.
- Pointer drag applies dabs; `request_repaint` while interacting; pan/zoom of the canvas view.
- Controls panel: tool selector (Raise/Lower/Smooth/Flatten/Set), radius slider, strength slider, falloff, target-height (for Flatten/Set), undo/redo, Convert to bricks, reset, new-blank / load-image, output options (mirror single-box: `.brdb`→Worlds, `.brz`→Prefabs, install toggle).

---

## 6. Undo / redo

- Bounded per-stroke history: on stroke start, snapshot the affected bounding rect (union of the stroke's dabs) **before** editing; push to a deque capped at ~32 entries. Undo restores the rect; redo re-applies. Rect-sized `f32` snapshots → small memory. Exact restoration (test-guarded).

---

## 7. Integration with the existing app

- **`app.rs`** — add a `Sculpt` tab to the existing tab enum/switcher beside Map/Convert; hold `SculptState`.
- **`map_tab.rs`** — add a **Send to Sculpt** button; on click, fetch+decode the DEM on a worker thread (reuse `fetch_and_decode_dem`, already `pub(crate)`), build a `HeightField`, store it, switch tab.
- **Convert reuse** — Sculpt's Convert calls the existing `build_heightmap` → `generate_bricks` → write/install path (factor a shared `convert_heightfield(&HeightField, OutputOptions) -> BuildOutcome` that mirrors the single-box `run_build` tail; reuse `build_one_tile` where it fits). No change to the mesher core.
- **`GenOptions.skip_floor` (additive, default `false`)** threaded into `quads_to_bricks`/`emit_column_bricks`: when set, a normalized-zero column emits no bricks (§2). Default-off keeps every existing single-box/grid output byte-identical (guarded by the existing identity tests); the sculpt convert sets it `true`.

### Sculpt→Convert seam for #3 (future, documented only)
The live brick preview will subscribe to dirty rects from the editor and incrementally re-mesh only changed regions through the same `to_dem_raster` boundary. Nothing in the MVP precludes it.

---

## 8. Rename — user-facing (internals deferred)

- Window title + in-app title → **"Brickadia-World-Tools"** (in `app.rs` / eframe `NativeOptions` / title bar).
- `~/.local/share/applications/heightmap2brz.desktop` `Name=` → "Brickadia-World-Tools" (keep the file/exec; optionally add a `brickadia-world-tools` launcher symlink in `~/.local/bin` aliasing the binary).
- Cargo package name (`heightmap`), binary (`heightmap_gui`), repo dir, and existing symlinks stay as-is for now (zero-risk, reversible). Full internal rename is a later, separate task.

---

## 9. Module layout

New `src/gui/sculpt/`:
- `mod.rs` — `SculptState`, tab entry, re-exports.
- `heightfield.rs` — `HeightField`, `FieldMeta`, `FLOOR_M`, constructors, `to_dem_raster`.
- `brush.rs` — `Brush`, `BrushShape`, `Falloff`, weight kernel.
- `tools.rs` — `Tool` trait + the five height tools, dab application.
- `sculpt_tab.rs` — egui UI, rendering, brush overlay, undo/redo, convert wiring.

Plus edits: `app.rs` (tab + state), `map_tab.rs` (Send to Sculpt), `mod.rs` (`mod sculpt;`), small `build.rs`/`generate.rs` touch for the floor-skip + a shared convert entry. Files kept focused.

---

## 10. Test strategy (NASA JPL + honesty contract; real, non-tautological)

**Identity / floor:**
- `sculpt_passthrough_identity` — `HeightField::from_dem(raster)` → `to_dem_raster()` with **no edits**, converted with `skip_floor = false`, reproduces a `DemRaster` that converts to **byte-identical bricks** vs a direct map build (the headline guard, mirrors the grid single-box identity; proves the round-trip is lossless independent of floor-skip).
- `brush_never_below_floor` — Lower/Flatten/Set on a near-floor field never produce a cell `< FLOOR_M`.
- `blank_canvas_floor_emits_no_bricks` — an all-floor `HeightField` converted with `skip_floor = true` yields zero terrain bricks (native floor only); raising one region emits bricks only there.
- `skip_floor_default_off_is_byte_identical` — `skip_floor = false` over a fixed quad set equals pre-change `emit_column_bricks` output (locks the additive default).

**Tools:**
- `raise_bounded_within_radius` — raise adds within the brush radius, **zero change outside**.
- `smooth_reduces_local_variance` — smoothing a noisy patch lowers its variance.
- `flatten_moves_monotonically_toward_target` — each flatten dab moves cells toward target, never past it.
- `falloff_weights_center_and_edge` — Smoothstep weight ≈1 at center, ≈0 at radius edge; Constant is flat.
- `dab_touches_only_affected_rect` — a dab leaves all cells outside its rect untouched.

**Undo/state:**
- `undo_restores_exact_prior_state` — snapshot→edit→undo == original, bit-exact.
- `apply_is_deterministic` — same inputs → same field.

**HeightField:**
- `from_dem_roundtrips_dims_and_values`, `min_max_correct`, `sample_bilinear_interpolates`.

---

## 11. Definition of Done
- `cargo build` (gui) + `cargo test` + `cargo clippy --all-targets -- -D warnings` all green, including the new sculpt tests AND the existing single-box / grid identity suites unchanged.
- Send to Sculpt → no edits → Convert (`skip_floor = false`, same options) produces a `.brdb` byte-identical to a direct map build of the same area/settings.
- A blank canvas converts to the plain Brickadia world (no redundant floor); raising terrain builds **up** from the native ground, watertight, with nothing below the floor (in-game visual confirmation of base-on-ground).
- Window/launcher reads "Brickadia-World-Tools".

---

## Implementation note
Execute via the ultracode adversarial-loop workflow (sequential build stages → parallel adversarial audit → fix → gate), as used for the grid feature. Audit dimensions to carry over: correctness/identity (floor + passthrough), performance/no-crawl (60fps brush feel, texture-regen only on dirty), memory (bounded undo history), and honest CPU/GPU framing (GPU deferred to #3, not faked here).
