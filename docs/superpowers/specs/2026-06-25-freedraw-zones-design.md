# Freedraw omit/include zones — design (Phase 1a: core + sculpt canvas + undo)

## Goal

Let the user draw closed loops ("zones") that act as a spatial XY cookie-cutter
on the exported terrain, independent of height:

- **Omit zone** — no bricks anywhere inside the loop (cut a hole), even under
  tall terrain.
- **Include zone** — keep ONLY what falls inside include zones; drop everything
  outside them.

This is distinct from the existing height-based `omit_below_m` (which drops
columns at/below an elevation). Zones are a footprint mask; the two are
orthogonal and both apply.

**Phase 1a (this spec)** delivers the shared masking core, freedraw on the
**sculpt canvas**, and **undo/redo** for zones. **Phase 1b (deferred)** adds the
saveable `.h2bproj` project file — separable because zones draw, mask, and export
entirely from memory without it. **Phase 2 (separate spec)** adds freedraw on the
**Map tab**, reusing the identical core with a lat/lon → cell transform.

## Decisions (settled in brainstorming)

1. **Zone purpose:** spatial XY mask, height-independent (cookie-cutter).
2. **Surfaces:** both sculpt canvas and Map — but split into two phases; this
   spec is sculpt only.
3. **Rasterization:** classify EXISTING cells via point-in-polygon. No new/finer
   cells. Loop edges are stair-stepped at the current cell resolution.
4. **Combine rule:** `keep = (inside_any_include OR no_include_zones) AND NOT
   inside_any_omit`. Omit wins on overlap; no zones = everything exports
   (byte-identical to today). Order-independent.
5. **Draw style:** both freehand lasso AND polygon-click.
6. **Undoable:** zone add / delete / clear go on the same undo/redo stack as
   height edits (revision change after the original spec).
7. **Persisted / saveable project:** the sculpt session (field + meta + zones +
   export settings) saves to and loads from a project file (revision change).
   **Deferred to Phase 1b** — designed below but not built in this pass.
8. **Masks survive generation:** generating an export does NOT clear zones; the
   user explicitly clears all or keeps drawing new loops (revision change).

## Data model

```rust
// src/gui/zones.rs (surface-agnostic; Phase 2 reuses verbatim)
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ZoneMode { Omit, Include }

#[derive(Clone, PartialEq)]
pub(crate) struct Zone {
    pub mode: ZoneMode,
    /// Closed polygon, points in CELL space (fractional cell coords). The
    /// closing edge (last -> first) is implicit. Requires >= 3 points to be a
    /// real area; a degenerate (<3) zone rasterizes to "contains nothing".
    pub polygon: Vec<(f32, f32)>,
}
```

`SculptState.zones: Vec<Zone>` — lives in memory alongside the field. No
serialization in this phase: project save/load is deferred (see Persistence).
`Serialize`/`Deserialize` get added only when the project file lands.

## Rasterizer (the shared core)

```rust
/// Row-major (width*height) keep-mask: true = export this cell.
pub(crate) fn rasterize(zones: &[Zone], width: u32, height: u32) -> Vec<bool>;
```

Algorithm, per cell `(x, y)` tested at its CENTER `(x as f32 + 0.5, y as f32 +
0.5)`:

```
has_include   = zones has any Include
in_include    = any Include zone contains center
in_omit       = any Omit  zone contains center
keep(x,y)     = (in_include || !has_include) && !in_omit
```

- **Point-in-polygon:** standard ray-casting / even-odd crossing count, polygon
  treated as closed (last→first edge). A point exactly on an edge resolves
  consistently (half-open edge convention) so adjacent zones tile without
  double-counting or gaps.
- **Empty `zones`:** returns all-`true` (every cell exports) — the byte-identical
  default.
- Degenerate polygons (<3 points) contain no cells.

This module has NO dependency on egui, the field, or the map — pure geometry over
`(width, height)` so both surfaces share it.

## Mesher integration

`generate_bricks_skip_floor` gains one optional parameter:

```rust
keep_mask: Option<&[bool]>   // row-major width*height; None = no masking
```

Threaded into `gen_greedy_heightmap` (and, for parity, the quadtree path): a cell
with `keep_mask[y*width + x] == false` is excluded from the greedy occupancy
planes — exactly the existing per-cell skip the `cull` flag uses — so it emits no
brick. `None` (and an all-`true` mask) is byte-identical to today, guarded by a
test. The convenience wrapper `generate_bricks` (test-only) passes `None`.

The mask length must equal `width*height`; a mismatch is a checked error
(defense-in-depth, like the heightmap/colormap dimension check).

## Capture UX (sculpt canvas)

New `SculptTool::Zone`. When active, the tool sub-panel shows:

- **Mode** toggle: Omit (red) / Include (green).
- **Style** toggle: Lasso / Polygon.
- **Zone list:** one row per zone (mode + vertex count) with a delete button, plus
  a "Clear all zones" button.

Drawing (pointer coords mapped to cell space via the existing canvas transform):

- **Lasso:** press-drag records points; on release, decimate (drop points closer
  than a min cell-distance, e.g. ~0.75 cell) and auto-close → push `Zone`. A
  release with <3 surviving points is discarded.
- **Polygon:** each click appends a vertex; clicking within a small pixel radius
  of the first vertex, or double-click, closes the loop → push `Zone`; `Esc`
  cancels the in-progress polygon.

Rendering on the canvas overlay: committed zones as translucent filled outlines
(omit red, include green); the in-progress polygon shows placed vertices + a
rubber-band segment to the cursor.

The in-progress polygon's `Esc` cancels a mid-draw loop (before it is committed,
so it never enters undo history).

## Undo / redo

Zone add (lasso release / polygon close), delete, and clear-all are undoable on
the SAME history as height edits. The undo/redo deques generalize from
`RectSnapshot` to an enum:

```rust
enum UndoEntry {
    Height(RectSnapshot),   // existing per-stroke cell snapshot
    Zones(Vec<Zone>),       // the zones list BEFORE the mutation
}
```

- A committed zone mutation pushes `UndoEntry::Zones(prev_zones)` onto `undo` and
  clears `redo` (same discipline as a height stroke).
- `do_undo`: pop the last entry; `Height` restores cells (today's path), `Zones`
  swaps the current `zones` with the snapshot and pushes the now-current list to
  `redo`. `do_redo` is the mirror.
- The existing ~32-entry cap applies to the unified deque. Height and zone edits
  interleave in one timeline (undo walks back through whatever was done last).
- A `Zones` undo touches the export mask only (no cells), so it marks the canvas
  texture clean / no re-raster of the heightfield is needed.

## Apply at convert

`SculptState.zones` is rasterized to a keep-mask and passed to the mesher:

- **Single mesh (`convert_heightfield`):** `rasterize(&zones, w, h)` once →
  `keep_mask` → `generate_bricks_skip_floor`.
- **Tiled (`convert_heightfield_tiled`):** rasterize against the FULL field once,
  then slice the per-sub-field window out of the full mask for each tile (the
  sub-field's `[x0,x1) × [y0,y1)`), so a zone spanning a seam is consistent across
  tiles.

`start_convert` forwards `state.zones` into the convert calls (cloned into the
worker thread alongside the field), exactly as it already stamps `FieldMeta`.

**Masks survive generation.** Converting reads `state.zones` but never mutates or
clears them — after an export the zones remain so the user can re-export, refine,
or start over. The zone panel's **"Clear all zones"** wipes them (undoable);
drawing simply appends new loops. ("Create a new mask" = clear all, then draw —
there is no separate named-mask concept in Phase 1.)

## Persistence — saveable sculpt project (Phase 1b — DEFERRED, not built in 1a)

> Designed here for continuity, but **out of scope for the 1a build**. Phase 1a
> keeps zones in memory only; nothing below is implemented yet. When 1b lands,
> `Zone`/`ZoneMode`/`FieldMeta`/export-settings gain feature-gated serde derives.

The whole sculpt session round-trips through a project file via the native
save/load dialogs (mirrors the existing heightmap-PNG export + image-load dialog
pattern).

```rust
// src/gui/sculpt/project.rs
#[derive(Serialize, Deserialize)]
struct SculptProject {
    format: String,        // "heightmap2brz-sculpt-project" marker, checked on load
    version: u32,          // schema version (1), so future loads can migrate
    width: u32,
    height: u32,
    cells: Vec<f32>,       // row-major heightfield, EXACT (no PNG quantization)
    meta: FieldMeta,       // cell_m, studs_per_meter, vertical_exaggeration, micro, centroid_lat, source_name
    zones: Vec<Zone>,
    // Export-panel settings so a resumed project rebuilds identically:
    omit_below_m: f32,
    floor_level_m: f32,
    // (output formats / install / overwrite / tile settings as a small serde struct)
}
```

- **Format:** JSON via `serde_json` (already a dep). Extension `.h2bproj` with a
  dialog filter; the `format`/`version` fields guard against loading a foreign or
  newer file (clear error, never a panic).
- **Save:** serialize `SculptProject` from `SculptState` (field cells + meta +
  zones + export settings) → write to the chosen path. Errors surface in
  `last_error` like the PNG export.
- **Load:** parse → validate `format`/`version` and `cells.len() == width*height`
  → rebuild the `HeightField` and replace the tab's field + zones + export
  settings; clears undo/redo (a load is a non-local change, like today's
  field-load path).
- **`FieldMeta` and the export-settings struct gain `Serialize`/`Deserialize`**
  (feature-gated), alongside `Zone`.
- **Known limitation (size):** `cells` is exact `f32`, so the JSON scales with
  cell count (~3 MB at 512², large at 4096²). Acceptable for typical canvases;
  a compact binary format is a future option, noted not built.

## Testing (TDD)

**Rasterizer (`zones.rs`):**
- Omit zone over a region → those cells `false`, rest `true`.
- Include zone → only inside `true`, outside `false`.
- Include + overlapping omit → overlap is `false` (omit wins).
- Concave polygon classified correctly (a notch is excluded).
- No zones → all `true`.
- Degenerate (<3 pts) → contains nothing (no effect for omit; for include it
  contributes no kept cells).
- Edge convention: two abutting zones sharing an edge tile every cell exactly
  once (no double-keep/gap).

**Mesher:**
- A keep-mask with a hole drops exactly those cells (brick footprint reflects the
  mask).
- `None` / all-`true` mask is byte-identical to the pre-change mesh (identity
  guard).
- Mask length mismatch → checked error.

**Convert:**
- A field with one omit zone emits fewer bricks than the same field with no zones,
  and no brick falls inside the omit footprint.
- An include zone emits bricks ONLY inside the zone.
- Tiled convert with a seam-spanning zone matches the single-mesh masked result
  (watertight + consistent).
- Converting does not clear `state.zones` (they persist for re-export).

**Undo/redo:**
- Adding a zone then undo restores the prior zones list; redo re-adds it.
- Delete / clear-all are undoable; redo re-applies.
- Interleaved height-edit and zone-edit undo in correct last-in-first-out order.

**Persistence (Phase 1b — deferred, tests written when built):**
- `SculptProject` round-trips: save a field+zones+settings, load it, and the
  rebuilt field cells, meta, zones, and export settings are equal.
- Load rejects a wrong `format`/`version` and a `cells.len()` mismatch with a
  checked error (no panic).

## Scope boundaries (YAGNI)

- **Sculpt canvas only.** Map-tab freedraw is Phase 2 (separate spec), reusing
  `zones::rasterize` with a lat/lon→cell transform.
- **Omit/Include mode only** — no per-zone height/floor/scale settings.
- **Current cell resolution** — no finer-resolution subdivide inside zones.
- **Draw + list + delete + clear** — no post-close vertex dragging/editing.
- **Project save/load is Phase 1b (deferred)** — 1a draws/masks/exports zones
  from memory only; no `.h2bproj` file yet. (When built: JSON with exact `f32`
  cells, large canvases make large files, compact/binary deferred further.)
- **One mask set** — no named/layered masks; "new mask" = clear + redraw.

## Phase 2 (not in this spec)

Map-tab freedraw: capture loops on the OSM map in screen space → convert to
lat/lon → at build time convert to cell indices of the fetched raster (bbox +
dims) → `zones::rasterize` → thread the same `keep_mask` into `build_one_tile`.
The rasterizer and mesher changes from Phase 1 are reused unchanged.
