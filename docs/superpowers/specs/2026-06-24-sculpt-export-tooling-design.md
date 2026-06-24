# Sculpt Export Tooling + Fixes — Design Spec

Project: `~/Projects/brickadia/heightmap2brz` (Brickadia-World-Tools) · Branch `feat/sculpt-terrain-editor` · Follows the Sculpt MVP. Status: **approved, ready for ultracode loop.**

## Why (root-caused from the live convert path)
The Sculpt convert (`src/gui/sculpt/convert.rs`) has **no user-facing export controls** and **does not reuse the grid pipeline**. Every reported symptom traces to that:

| Symptom | Root cause (code) |
|---|---|
| "not using the gridding system" | `convert_heightfield` single-meshes the whole field (`convert.rs:116`); never calls `run_grid_build`. |
| "not using microbricks" | mesh honors `meta.micro` (`convert.rs:99`) but `blank_meta` hardcodes `micro:false` (`sculpt_tab.rs:314`) and there is no UI toggle. |
| "heights low / not 1:1" | `blank_meta` hardcodes `studs_per_meter:4.0, vertical_exaggeration:1.0`; the Sculpt tab exposes no scale controls, so a blank canvas can't be tuned. |
| "lots of gaps" | NOT hollow-shell (`fill_to_base:true`, `build.rs:961`). Consistent with low vertical resolution: at low scale `build_heightmap` rounds near-floor cells to brick-Z 0 and `skip_floor` drops them → feathered holes. **The loop's first stage MUST confirm this on real bricks before fixing.** |

## Scope
Add a **Sculpt Export panel** + fix the scale/micro/gap path + add a manual grid-tiled export. All additive; the passthrough-identity and `skip_floor`-default-off guards stay green.

---

## 1. Export panel (collapsing "Export" section in the Sculpt tab)
One panel, best-practice grouping:
- **Scale:** `studs_per_meter` + `vertical_exaggeration` DragValues (drive `FieldMeta`/convert; let a blank canvas reach true 1:1 and crank relief). Live "≈ N studs/m · relief 1:1 · ~M m/cell" readout mirroring the Map tab.
- **Brick type:** `micro` toggle (SmoothTile ↔ Micro).
- **Formats:** `.brdb`→Worlds, `.brz`→Prefabs, install toggle, overwrite (existing `OutputOptions`).
- **Tiling (manual):** "Tile this export" checkbox (default off). Off = single mesh (today). On = grid-tiled export with a `tile_cells` DragValue (sub-field edge length in cells).
- **Floor / omit:** `floor_level_m` + `omit_below_m` DragValues, each with an eyedropper button (see §3).
- **Live estimate:** bricks / peak RAM / tile count (reuse the grid `estimate_grid` math when tiling; a cheap single-mesh estimate otherwise), with a disable + remedy when over budget — gate the Export button on it.

## 2. Modifier scrolling (F2) — one reusable helper
`modifier_drag(ui, value, base_speed)`: a wrapper around `egui::DragValue` that, while hovered, reads keyboard modifiers and scales the step — **Ctrl = fine (×0.1)**, **Alt = coarse (×10)**, none = base. Applied to every numeric DragValue in the Sculpt/Export UI (brush radius/strength/target, scale, tile size, floor/omit). Pure helper in `sculpt_tab.rs` (or a small `ui.rs`); unit-test the step-selection logic.

## 3. Eyedropper height-picker (F3)
A pick mode toggled from the Export panel (and/or a hotkey). While active, clicking the canvas samples the cell's height (meters). The active modifier routes the sample:
- **plain click → target height** (the Flatten/Set target, existing field)
- **Alt-click → floor level** (`floor_level_m`)
- **Ctrl-click → omit-below level** (`omit_below_m`)
Reuses the single map `Response` hit-test (no second interact widget), gated so it never fights brush painting (active only in pick mode). A small on-canvas readout shows the sampled value. Test the cell→meters sampling + modifier routing.

## 4. Floor & omit-below model (F4) — meter-space, fixes the gaps
Two meter-space levels drive the convert, evaluated **before** vertical quantization so low scale can't silently drop terrain:
- **`floor_level_m`** (default `FLOOR_M`=0): the base plane terrain fills down to. Convert maps it through `base_override`.
- **`omit_below_m`** (default = floor): a column whose **source height (m) ≤ `omit_below_m`** emits no bricks (native floor / void) — this is "omit water". Threshold converted to brick-Z as `h_omit = round(omit_below_m * vertical_scale)`; skip iff `h ≤ h_omit`.

Replace the current brick-Z `skip_floor` predicate (`(h - min_height) == 0`) with this meter-derived threshold. **Default-preserving:** `skip_floor=false` still never skips (passthrough identity intact); `skip_floor=true` with `omit_below_m=0` skips only true-floor columns. At a proper (user-set) scale, near-floor cells map to `h>0` and survive → the feathering gaps close. The loop confirms before/after on real bricks.

## 5. Grid-tiled sculpt export (manual toggle)
When "Tile this export" is on, route through a **heightfield tiling** path (NOT the geographic grid fetch):
- Subdivide the `HeightField` into `ceil(w/tile_cells) × ceil(h/tile_cells)` sub-fields by **shared integer cell ranges** (adjacent sub-fields share the exact edge column/row — seams are trivially exact, no projection/zoom drift like the geo grid).
- Mesh each sub-field via the existing convert mesh (uniform `size`, `base_override=Some(0)`, `global_min=0`, the meter-space omit), with a per-tile **world offset** computed from cumulative cells + global centering (reuse the grid `world_offset` algebra).
- Accumulate into one `Vec<Brick>` (or stream per-tile), `bricks_to_save` once → one stitched `.brdb`/`.brz`. Aggregate cap = `MAX_GRID_BRICKS`; per-tile cap = `enforce_cell_budget`.
- Worker thread + progress + cancel, mirroring `convert_heightfield` and `run_grid_build`.

Factor shared seam/offset helpers from `grid.rs` rather than duplicating where clean; otherwise reuse its math.

---

## 6. Files
- `src/gui/sculpt/convert.rs` — `convert_heightfield` gains `floor_level_m`/`omit_below_m`; add `convert_heightfield_tiled` (the §5 path). Skip predicate → meter-space.
- `src/gui/sculpt/sculpt_tab.rs` — the Export panel, `modifier_drag`, eyedropper pick mode + level state, tile toggle/size, estimate, gating.
- `src/gui/sculpt/heightfield.rs` — `FieldMeta` already carries `studs_per_meter`/`vertical_exaggeration`/`micro`; add a `sub_field(cell_rect)` helper for tiling; cell→meters sampler.
- `src/opt/generate.rs` / `src/gui/build.rs` — thread the meter-derived omit threshold into the skip decision (additive; default-off byte-identical). Reuse `generate_bricks_skip_floor`.
- `src/gui/grid.rs` — expose `world_offset` (or a tiling helper) `pub(crate)` for the sculpt tiler if not already.

## 7. Test strategy (real, NASA-JPL/honesty)
- `skip_floor_default_off_byte_identical` + `sculpt_passthrough_identity` stay green (unchanged).
- `omit_below_drops_cells_at_or_under_level` (meter-space) + `near_floor_cells_survive_at_proper_scale` (the gap fix: at vscale where 0.3 m → h≥1, a 0.3 m cell emits, with `omit_below=0`).
- `floor_level_raises_base_plane`.
- `modifier_drag_step_selection` (ctrl→×0.1, alt→×10, none→×1).
- `eyedropper_samples_cell_meters` + modifier routing.
- `sub_field_shares_exact_edge_cells` + `tiled_export_world_offset_abutment` (a 2×2 sculpt tiling abuts exactly; stitched brick set == single-mesh of the same field, modulo the spawn).
- `tiled_vs_single_mesh_equivalence` (no seam: tiled stitch geometry == single mesh for a field under one tile's budget).
- `export_estimate_gates_over_budget`.
- micro path: `micro_export_uses_micro_block_and_size`.

## 8. Definition of Done
- Build + tests + `clippy --all-targets -D warnings` green; existing identity/grid/sculpt suites unchanged.
- Blank canvas: scale controls reach true 1:1; cranking exaggeration visibly raises relief; micro toggle produces micro bricks.
- A sculpted hill exports **watertight** (no feathered gaps) at a proper scale; `omit_below` leaves water/lowland as native floor; `floor_level` sets the base.
- "Tile this export" on a large canvas produces one seamless stitched `.brdb` (visual seam check in-game).
- Every numeric slider honors Ctrl(fine)/Alt(coarse).

## 9. Loop note
First stage = **diagnosis**: build real bricks at low vs proper scale, measure dropped near-floor columns, and confirm the gap mechanism before changing the skip predicate. Then implement; then the 5-dimension adversarial audit (identity/floor/omit · perf/60fps · memory · integration/UX · standards) → fix → gate.
