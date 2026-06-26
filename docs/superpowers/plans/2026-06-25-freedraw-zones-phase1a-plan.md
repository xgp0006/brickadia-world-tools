# Implementation plan — Freedraw zones, Phase 1a

Spec: [`../specs/2026-06-25-freedraw-zones-design.md`](../specs/2026-06-25-freedraw-zones-design.md)
Scope: zone masking core + sculpt-canvas freedraw + undo. **Persistence (`.h2bproj`)
is Phase 1b, NOT in this plan.** Map-tab freedraw is Phase 2.

Discipline: TDD (test first per step), NASA-JPL Power-of-10, no TODO/placeholder,
doc-truth. Every step ends green (`cargo test --features gui` + `cargo clippy
--features gui -- -D warnings`). Identity guards must stay green at every step.

---

## Precondition (BLOCKER — resolve before any code)

The working tree on `feat/sculpt-terrain-editor` has **uncommitted, unrelated work**
(563 insertions across 12 files: `MAX_BRICK_UNITS` change in `opt/mod.rs`/`quad.rs`,
plus `build.rs`/`map_tab.rs`/`tiles.rs`/`generate.rs`). None of it is zone-related.

Do **one** of these first, so zone work starts from a clean, known base:
1. Commit it on `feat/sculpt-terrain-editor` if it's finished, **or**
2. `git stash` it if it's mid-flight, **or**
3. Branch zones off the last clean commit (`3c33df7`) and leave the dirty tree where it is.

Then: `git switch -c feat/freedraw-zones` from the chosen base.
**Do not** start zones on top of an unexplained dirty tree — a failing identity
guard later would be unattributable.

---

## Step 1 — Rasterizer core (`src/gui/zones.rs`, new, pure geometry)

Surface-agnostic, zero egui/field/map deps so Phase 2 reuses it verbatim.

**Test first** (`#[cfg(test)]` in `zones.rs`):
- omit zone → covered cells `false`, rest `true`
- include zone → only inside `true`
- include + overlapping omit → overlap `false` (omit wins)
- concave polygon → notch excluded (correct even-odd)
- no zones → all `true` (the byte-identical default)
- degenerate (<3 pts) → contains nothing
- two abutting zones sharing an edge → every cell kept/dropped exactly once
  (half-open edge convention, no double-count or gap)

**Then implement:**
```rust
pub(crate) enum ZoneMode { Omit, Include }      // Clone+Copy+PartialEq+Eq
pub(crate) struct Zone { mode: ZoneMode, polygon: Vec<(f32,f32)> }   // Clone+PartialEq
pub(crate) fn rasterize(zones: &[Zone], width: u32, height: u32) -> Vec<bool>;
```
Per cell tested at center `(x+0.5, y+0.5)`: ray-cast point-in-polygon (closed,
last→first edge, half-open convention). `keep = (in_include || !has_include) && !in_omit`.

Wire `mod zones;` into the gui module tree (`src/gui/mod.rs` or wherever `sculpt`
is declared). **No serde derives** (Phase 1b adds them).

---

## Step 2 — Mesher keep-mask (`src/opt/generate.rs`, `build.rs`, `quad.rs`)

Thread an optional cell mask through the greedy path, mirroring the existing
`cull`/`skip_floor` per-cell skip.

**Test first** (`generate.rs` tests, beside `skip_floor_default_off_is_byte_identical`):
- **identity guard:** `keep_mask = None` (and an all-`true` mask) is byte-identical
  to today's mesh — same fixture as the skip_floor identity test.
- mask with a hole → exactly those cells emit no brick.
- mask length ≠ `width*height` → checked `Err`, no panic.

**Then implement:**
- `generate_bricks_skip_floor` gains `keep_mask: Option<&[bool]>` (last data param).
- Thread into `gen_greedy_heightmap` → the per-cell skip in `collect_height_color_pairs`
  / `build_planes` (lines ~367, ~403): a cell with `keep_mask[y*width+x] == false`
  is treated like a culled cell (contributes no occupancy → no brick).
- Length check at the `gen_greedy_heightmap` entry: `Some(m)` with `m.len() != width*height`
  → `Err` (defense-in-depth, like the heightmap/colormap dim check).
- `quad.rs` quadtree path: same skip for parity (mask consulted identically).
- Test-only `generate_bricks` wrapper passes `None`.

`None` everywhere = zero behavior change; the identity guard proves it.

---

## Step 3 — Zones in state + unified undo (`src/gui/sculpt/sculpt_tab.rs`)

**Test first:**
- add a zone, `do_undo` → zones list restored to prior; `do_redo` → re-added.
- delete one / clear-all → undoable; redo re-applies.
- interleaved height-edit then zone-edit → undo pops in LIFO order (zone first).

**Then implement:**
- `SculptState.zones: Vec<Zone>` (default empty).
- Generalize history:
  ```rust
  enum UndoEntry { Height(RectSnapshot), Zones(Vec<Zone>) }
  ```
  `undo`/`redo` become `VecDeque<UndoEntry>` (cap unchanged, `UNDO_CAP`).
  `active_stroke: Vec<RectSnapshot>` stays height-only (mid-drag buffer).
- `commit_stroke`: wrap the collapsed snapshot as `UndoEntry::Height(..)`.
- A zone mutation pushes `UndoEntry::Zones(prev_zones.clone())`, clears `redo`.
- `do_undo`/`do_redo`: `match` the entry — `Height` runs `restore_into` (today's
  path) and re-wraps the inverse; `Zones` swaps `state.zones` with the snapshot and
  pushes the now-current list to the other deque. A `Zones` op touches no cells →
  no heightfield re-raster (don't call `mark_dirty_all` for it; only repaint the overlay).

---

## Step 4 — Capture UX + overlay (`src/gui/sculpt/sculpt_tab.rs`)

No tests (interactive egui); correctness proven by Steps 1–3 + 5 and manual in-game check.

- `SculptTool::Zone` added to the enum + `ALL` + `label()`. Guard: `strength_is_blend`
  and `apply_dab` are height-tool concepts — Zone must not reach `apply_dab` (the
  zone tool branches before brush-dab dispatch).
- Tool sub-panel when Zone active: Mode toggle (Omit red / Include green), Style
  toggle (Lasso / Polygon), zone list (mode + vertex count + per-row delete),
  "Clear all zones" button.
- Pointer → cell via the existing canvas transform:
  - **Lasso:** press-drag records points; on release decimate (drop points <~0.75
    cell apart), auto-close, push `Zone`; <3 surviving points discarded.
  - **Polygon:** click appends vertex; close on click within small px radius of
    first vertex OR double-click; `Esc` cancels in-progress (never enters undo).
- Overlay render (in/near `paint_brush_overlay` sibling): committed zones as
  translucent filled outlines (omit red / include green); in-progress polygon shows
  placed verts + rubber-band to cursor.
- Every committed zone add/delete/clear routes through the Step-3 `UndoEntry::Zones` push.

---

## Step 5 — Apply at convert (`src/gui/sculpt/convert.rs`, `sculpt_tab.rs`)

**Test first** (`convert.rs` tests):
- field + one omit zone → fewer bricks than no-zones, and no brick inside the omit footprint.
- include zone → bricks ONLY inside the zone.
- tiled convert with a seam-spanning zone → watertight + equal to the single-mesh
  masked result.
- convert does **not** clear `state.zones` (they persist for re-export).

**Then implement:**
- `convert_heightfield`: `rasterize(&zones, w, h)` once → pass as `keep_mask`.
- `convert_heightfield_tiled`: rasterize the FULL field once, slice each sub-field's
  `[x0,x1)×[y0,y1)` window out of the full mask per tile (seam-consistent).
- `start_convert`: clone `state.zones` into the worker thread alongside the field
  (exactly as it stamps `FieldMeta`); never mutate/clear them.

---

## Step 6 — Gate + ship

- `cargo build --features gui` clean.
- `cargo test --features gui` — all green, incl. every identity guard.
- `cargo clippy --features gui -- -D warnings` clean.
- Manual in-game: draw an omit loop over a hill → Convert → hole is cut, terrain
  outside intact; draw an include loop → only that footprint exports; both survive
  a second Convert.
- Then (optional, your call) run the **ultracode** adversarial-loop workflow like
  the grid/sculpt features for an independent correctness pass before merge.

---

## Build order rationale

1→2 are independent pure-logic cores (no UI) and fully testable headless — land them
first, fully guarded. 3 generalizes undo with zones still invisible. 4 is the only
untested (interactive) step, deliberately last among the UI work and resting on
tested foundations. 5 wires the tested core to the tested mesher. Each step is a
clean commit; a bisect lands on a single concern.

## Out of scope (Phase 1a)

Project save/load (`.h2bproj`, Phase 1b) · Map-tab freedraw (Phase 2) · per-zone
height/scale · sub-cell raster · post-close vertex editing · named/layered masks.
