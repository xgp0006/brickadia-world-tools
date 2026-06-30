# Sculpt Grid Sections, Selection Layers & Multi-Resolution Aligned Export — Design Spec

**Date:** 2026-06-30 · **Branch (proposed):** `feat/sculpt-layers` (stacks on `feat/shape-stamps` @ `04ccf1e`) · **Project:** `~/Projects/brickadia/heightmap2brz` · **Status:** Binding decisions (D1–D4) + Open Questions §10 all RESOLVED — spec finalized, MVP cleared to build.

**One-line summary:** A Photoshop-style export-layer system on the sculpt tab — each layer is a colored selection of base-grid boxes + lasso/polygon regions with its own export settings — that exports each layer as its own pre-positioned save written at true absolute world coordinates, so loading every part in Brickadia auto-assembles one seamless world; detail layers keep the fine pitch while background layers may use a coarser integer-multiple pitch, watertight by construction.

## How to use this doc

The four BINDING DECISIONS (§1) are locked — do not relitigate. All `symbol` / `file:line` citations are ground-truth as of the synthesis groundings; **grep the cited symbol, do not trust the exact line number** (the codebase moves). Where this doc and the code disagree, the binding decisions and the named invariants in §5/§9 win. Anything listed as deferred for a phase (§7) is out of scope for that phase.

---

## 1. Binding decisions (restated crisply)

- **D1 — Shared base grid.** Every region's resolution is an integer multiple (1×, 2×, 4×, 8×, …) of ONE base cell size, placed on a common world grid, so coarse brick edges always land exactly on fine brick edges. Watertight by construction — the same uniform-pitch rule that makes `tile_bounds` seams abut after the cuts fix.
- **D2 — Pre-positioned overlay assembly.** Each layer exports as its own save whose bricks are written at TRUE absolute world coordinates, so loading every part in Brickadia drops them into place automatically (zero manual alignment). Bounded by Brickadia's i16 `ChunkIndex` world-extent limit (`MAX_CHUNK_INDEX_UNITS`, `grid.rs:510`).
- **D3 — New export-layer system; existing Mask stays as-is.** A layer = a colored set of selected grid boxes + lasso regions + its OWN export settings, producing one exported part. The existing `zones::ZoneMode::{Omit,Include}` Mask keeps shaping a single export. Separate concepts, separate UI surfaces.
- **D4 — Adjustable box grid + per-cell/lasso selection into layers.** The user sets the grid divisions; a small control pops up at a grid box's top-left to pick boxes; a layer holds any mix of picked boxes AND lasso/polygon regions.

**Hard constraint:** scaling to massive worlds must NOT degrade top-level quality. Detail regions keep the fine base pitch; only coarse/background regions use a coarser (integer-multiple) pitch. Every error path steers the user to *coarsen the background*, never to coarsen the detail.

---

## 2. The unifying invariant (read this before §3–§5)

Both D1 (watertight resolution) and D2 (auto-overlay) reduce to ONE rule:

> **Every part is meshed against ONE shared per-cell pitch `size_base` and ONE shared global-centering datum (`global_cells_w/h` = the full field's base-cell extent), and every cell of every part is placed by `tile_world_offset` on the same `2·size_base` world lattice.**

This is identical to the uniform-pitch mechanism that the recent tiling/cuts fix repaired (`derive_scale` called ONCE, stamped into every tile — `grid.rs:371`, `convert.rs:291-302`). We are reusing the proven mechanism, not inventing a new one. The blast radius of getting this wrong is the seam/coincident-brick bug; §8 lists the explicit guards.

---

## 3. Data model

New module `src/gui/sculpt/layers.rs` (pure data + selection geometry; reuses `zones.rs` for polygons, `paint.rs` palette conventions for color). Lives entirely in new `SculptState` fields — it never touches `state.zones` / `state.paint`, keeping D3 separation structural rather than by convention.

```rust
// src/gui/sculpt/layers.rs  — NEW

pub struct LayerStack {
    pub layers: Vec<Layer>,    // z-order: index 0 = BOTTOM, last = TOP = highest precedence
    pub active: usize,         // selected layer index; exactly one active at all times
    pub grid_div: (u32, u32),  // box-grid divisions over the field (D4, user-adjustable, clamp 1..=64)
}

pub struct Layer {
    pub id: LayerId,           // stable monotonic id — claim map + deterministic ordering
    pub name: String,          // default "Layer N"; the first layer is the un-deletable "Base"
    pub color: [u8; 4],        // panel swatch + canvas overlay tint (right-click to set); IDENTITY only, never exported brick color
    pub visible: bool,         // eye icon; hidden layers excluded from export AND from the claim pass AND from canvas tint
    pub box_mask: Vec<bool>,   // row-major base-cell ownership from box picks (len = gw*gh). Stored as CELLS, not box indices, so changing grid_div never loses picks.
    pub regions: Vec<zones::Zone>, // lasso/polygon picks — REUSE zones.rs verbatim; INCLUDE-semantics within the layer
    pub export: LayerExport,
}

pub struct LayerExport {
    pub res_mult: u8,          // 1 | 2 | 4 | 8 — the resolution multiplier knob (D1). size_part = res_mult * size_base
    pub reduce: ReduceOp,      // downsample op for coarse layers (Mean | Max | Min), default Mean
    pub reconcile_seams: bool, // opt-in flatten fine→coarse at LOD boundary (§5.5), default false
    pub sea_level: Option<f32>,// None = inherit global sculpt sea level
    pub surface: Surface,      // reuse existing enum
    pub output: convert::OutputOptions, // reuse: brdb/brz/install/overwrite/skip_floor (convert.rs:36-50)
    pub advanced: AdvancedFill,// floor_h, omit_below_h (reuse existing semantics)
}

pub enum ReduceOp { Mean, Max, Min }
```

**On `SculptState`** (`sculpt_tab.rs`): one field `layers: LayerStack`. No other globals change.

**Invariants the model upholds** (debug-asserted): `active < layers.len()`; ids unique; `layers.len() >= 1` always (the default "Base" layer can be renamed/recolored but not deleted). `res_mult ∈ {1,2,4,8}`. `block_type` / `micro` / surface base scale / `vertical_scale` / base `cell_m` are **NOT per-layer** — they must stay constant for alignment (§5.2) and live on the shared sculpt/export state.

**Selection mask of a layer** (base-cell bool, len `gw·gh`):

```
layer_selection(L) = L.box_mask  OR  zones::rasterize(&L.regions, gw, gh)   // zones.rs:73
```

Box-cells and lasso-cells both contribute "included" cells (union). Per-layer regions are **Include-only** — the Omit/Include vocabulary stays exclusively in the Mask tool (D3).

**Separation from Mask/zones (D3):** layers never read or mutate `state.zones`. The global Zone Mask continues to shape the single legacy Convert path only; `Export All Parts` ignores it (each part is fully defined by its own layer selection). This is stated in the UI (§4) to prevent surprise.

**Undo:** extend the one unified timeline. Add `UndoEntry::Layers(LayerStack)` (`sculpt_tab.rs:444`), a `record_layer_edit` recorder mirroring `record_zone_edit` (`:736` — whole-stack clone, clears redo, bounded to `UNDO_CAP=32`), and an `invert_entry` arm doing `std::mem::replace(&mut state.layers, prev)` (`:2796`). Box/region geometry edits do NOT `mark_dirty` (overlay is vector-drawn each frame, like zones); `res_mult` / visibility changes mark nothing but re-trigger the live estimate. `box_mask` is `Vec<bool>` (1 byte/cell) and the stack clones only on actual mutation — same O(W·H) ceiling already accepted for paint undo (`:453`).

**Persistence:** in-memory only, dropped on field load (`set_field` clears zones/paint at `:723-730`; the layer stack resets to one default "Base" layer). The struct is serde-ready by design; `.h2bproj` serialization lands with the zones/paint Phase-1b project-file work (§7 Phase 6).

---

## 4. Grid + selection UX

### 4.1 Where it lives — one new mode

The mode bar today is `[Shape · Stamp · Paint · Zone]` (`SculptMode::ALL`, `draw_mode_bar` `:1054-1072`). Add exactly one:

```
[ Shape ][ Stamp ][ Paint ][ Zone ][ Layers ]
```

- New `SculptMode::Layers`, glyph `egui_phosphor::regular::STACK` (stacked squares; fall back to `SQUARES_FOUR` if absent in the phosphor subset). `help()`: `"Layers — split your terrain into parts that export separately and snap together in-game"`.
- Selecting Layers mode swaps the right-side per-mode control block to the **Layers panel** (§4.4) and turns on the **box-grid overlay** (§4.2). Leaving the mode hides the overlay/popup but keeps all layer data (persists across mode switches, like zones/paint). Zone/Mask UI is entirely unchanged (D3, no clutter).

### 4.2 Adjustable box grid overlay (D4)

- `Columns` / `Rows` `DragValue`s in the panel (default 8×6, clamp 1..=64). Box `(i,j)` covers cells `x ∈ [i·fw/cols, (i+1)·fw/cols)`, `y ∈ […]`; boundaries round to integer base-cell indices so a box edge is always a valid D1 seam. Changing divisions does NOT clear picks (picks are stored as a per-layer cell mask, not box indices) — avoids the "I re-divided and lost my work" trap.
- **Rendering is rotation-correct, NOT axis-aligned** (load-bearing). Transform each box's 4 cell-space corners through `cell_to_screen` (`:2358`) and stroke the resulting quad — exactly the pattern `paint_terrain` uses for its 4-corner mesh (`:2371-2389`) and `paint_zone_overlay` for polygon verts (`:2241`). Grid lines: 1px `STROKE_DIM` at `gamma_multiply(0.6)`. Active-layer boxes fill with the layer color at the `BBOX_FILL` low-alpha precedent (`theme.rs:90`, `from_rgba_premultiplied(...,0x30)`); other visible layers' boxes tint in their own colors at half that alpha; hidden layers contribute no tint. Partial boxes (a lasso clipped through them) get a cheap single diagonal hatch + dashed border in the layer color.
- **Hover feel:** box under the cursor gets the `icon_button` hover-glow treatment byte-for-byte — `animate_bool_with_time(id.with("hover_glow"), hovered, 0.16)`, 1.5px `ACCENT.gamma_multiply(0.65·t)`, `StrokeKind::Outside` (`:1007-1018`).

### 4.3 Box pick control — the top-left popup (D4)

- The discoverable affordance: on hover-still over a box (respect `show_tooltips_only_when_still`, fade via the 0.16s `animate_bool_with_time`), a small floating control anchored at the box's **top-left screen corner** = `cell_to_screen(state, box_x0, box_y0)`, hit-tested in screen space using the same `cell_to_screen` → distance pattern as the polygon-close test (`:2160-2162`). It follows `view_rot` automatically. Contents: three mini `icon_button`s (22px glyphs) — `+` (`PLUS`, add box to active layer), `−` (`MINUS`, remove), toggle (`SELECTION`, add-if-absent/remove-if-present). Tooltips name the active layer: `"Add box to {layer}"`.
- **Fast path (the snappy flow):** left-click a box (when not over the popup) = toggle it into/out of the active layer. Left-drag across boxes = paint-select a swath into the active layer; hold **Alt** while dragging = erase. One `record_layer_edit` at drag start, not per-box.

### 4.4 The layers panel (Photoshop-style)

Rendered in `SidePanel::right("sculpt_controls")` (`:870`, default 280px) when `state.tool == Layers`, using `section_header` (`:1025`) + `ui.separator()` conventions. ASCII mock:

```
Layers
┌─────────────────────────────────────┐
│ 👁  ■  Background          4x    ⋮  │  ← selected = ember fill, TEXT_SELECTED
│ 👁  ■  Town detail         1x    ⋮  │
│ 👁  ■  Mountains           2x    ⋮  │
│ 👁  ■  Base (italic)       1x    ⋮  │  ← default layer, un-deletable
└─────────────────────────────────────┘
  [ + Add layer ]   [ 👁 All ]  [ 🚫 None ]
──────────────────────────────────────
Selected layer: Town detail
  Sub-tool   [ ▣ Boxes ][ ⬚ Lasso ][ ⬡ Polygon ]
  Detail     [ 1x | 2x | 4x | 8x ]   1x (finest)
  Sea level  [💧 Set]  12 N 3 f   ☑ inherit
  Surface    (Smooth ▾)
  ▸ Advanced
──────────────────────────────────────
Grid   Columns [ 8 ]   Rows [ 6 ]
──────────────────────────────────────
  Bricks (this part)   ~412 k   ✓
  Bricks (all parts)   ~1.8 M   ✓
  World span           1.2 km ✓ fits
      [  Export All Parts  ]
```

**Row anatomy** (one `ui.horizontal`, ~28px tall):
1. **Eye toggle** — compact 20×20 glyph (`EYE` / `EYE_SLASH` dimmed to `STROKE_DIM`); click toggles `layer.visible`. Hidden rows dim to `gamma_multiply(0.5)` AND drop their canvas tint (legible in two places). Tooltip: `"Included — this part will export"` / `"Skipped — won't export"`.
2. **Color swatch** — 14×14 filled rounded rect (`CornerRadius::same(3)`) in `layer.color`, 1px `STROKE_DIM` border. Left-click the swatch OR right-click anywhere on the row opens the recolor menu (§4.5).
3. **Name** — single-click selects (makes active); double-click → in-place `TextEdit` (Enter/focus-loss commits, Esc cancels). The default layer renders italic and cannot be deleted.
4. **Multiplier badge** — `ui.small` chip `1x/2x/4x/8x`, `TEXT_PRIMARY` for 1×, `STATUS_WARN_FG` amber for ≥2× (quiet "this part is coarser" hint).
5. **Overflow `⋮`** (`DOTS_THREE_VERTICAL`) — Rename · Color ▸ · Duplicate · Merge down · Delete (disabled on default) · Move up · Move down.

**Active highlight:** selected row paints `selection.bg_fill` (ACCENT_EMBER) + `TEXT_SELECTED` (`theme.rs:61`).

**Controls row:** `+ Add layer` (primary `egui::Button`; new layer gets next color from a fixed 8-color set distinct from the paint palette, name `"Layer N"`, `res_mult=1`, empty selection, `visible=true`, inserted above active and made active). `👁 All` / `🚫 None` icon_buttons set all `visible`.

**Reordering** carries NO compositing meaning (parts tile space, they don't blend). Order decides only canvas tint draw-order and overlap precedence (§5.4). v1 uses Move up/down menu items; drag-reorder deferred.

### 4.5 Recolor flow

Right-click row (or swatch / `⋮`→Color ▸) opens a small popup: the 8 preset layer colors as a `horizontal_wrapped` of 18×18 swatch buttons + a "Custom…" entry revealing `egui::color_picker::color_edit_button_srgba`. Selecting recolors the canvas box-tints and that layer's lasso overlay immediately. Undoable. Copy clarifies once (in Advanced): `"Layer color marks the part on the canvas — it doesn't paint bricks."`

### 4.6 Lasso / polygon into the active layer (D3 generalization)

Layers mode reuses the **entire** zone capture pipeline verbatim (grounding B), retargeted from `state.zones` to `active_layer.regions`:

- Sub-tool row (`horizontal_wrapped` of `icon_button`s, mirroring `draw_shape_tools`): `[ ▣ Boxes ][ ⬚ Lasso ][ ⬡ Polygon ]`. Boxes is §4.3; Lasso/Polygon run `handle_zone_lasso` (`:2117`) / `handle_zone_polygon` (`:2145`) exactly as today.
- `zone_draft`, `commit_zone_draft` (`:2173`), decimation const, double-click/close-vertex commit, `paint_zone_overlay` / `paint_closed_outline` / `is_convex` — all reused. The only change: commit pushes into the active layer's `regions` and renders in the layer's color instead of omit/include red/green.
- **Erase:** an eraser sub-tool or Alt+lasso subtracts a polygon from the active layer's mask.

This is the "selection-layer idea should also apply to lasso" ask: each layer owns its regions, so the same lasso machinery now produces per-layer selections rather than one global mask.

---

## 5. Export pipeline & alignment math

### 5.1 The resolution-multiplier knob

The per-part multiplier is the integer LOD factor **`res_mult ∈ {1,2,4,8}`** on `LayerExport`. It is **NOT** `studs_per_meter`, **NOT** `cell_m` directly, and **NOT** a per-part `derive_scale` call. Powers of two are recommended for clean nesting; the math holds for any positive integer.

Per part with multiplier `m`:
- `size_base = cell_size_units(hscale_base, micro) = hscale_base · upf` (`build.rs:313-315`) — derived ONCE via `derive_scale(cell_m_base, spm, exag, micro)` (`map_tab.rs:1312-1334`).
- `size_part = m · size_base`. Compute the multiple **explicitly** — never re-run `derive_scale` with a coarser `cell_m` (rounding could diverge and break exact integer alignment).
- A coarse layer downsamples its heightfield by `m` (each coarse cell = `reduce` of its `m×m` base cells) and emits bricks whose footprint = `m` base cells (`hscale_part = m · hscale_base`, fed to `generate_bricks_skip_floor` so `GenOptions.size` is computed internally at `build.rs:978`). `GenOptions.scale = 1` always — vertical exaggeration stays baked into the heightmap (`build.rs:983-985`).

### 5.2 Invariants constant across ALL parts (the D1 invariant set)

Hold these identical and stamp into every part:
1. **`size_base`** — the base per-cell pitch divisor; every `size_part = m·size_base` derives from it. Nothing else sets size. (Pillar A / uniform pitch.)
2. **`upf` / `micro` / `block_type`** — sets `upf` (1 or 5) and the brick asset.
3. **The centering datum** `global_cells_w/h` = the FULL field's base-cell extent. Every part centers on the SAME extent (`center_x = -(global_cells_w · size_base)`, `grid.rs:482`). **The centering term must use `size_base` for ALL parts, never the per-part coarse size** — this is the single most dangerous line in the feature. Parts that self-center on their own sub-region (the `-(dem_width·size)` path, `convert.rs:137-138`) will NOT abut.
4. **`vertical` (units/m)** — invariant under `m` automatically: `vertical = (2·hscale·upf / cell_m)·exaggeration` (`map_tab.rs:1332`); substituting `hscale_part = m·hscale_base` and `cell_m_part = m·cell_m_base` cancels `m`, yielding identical `vertical_base`. Equal real elevations → equal brick-Z across LODs with no extra work. (Pillar B.)
5. **Floor datum** (`global_min_m`, `base_override`/`floor_h`). Floor-relative sculpt fields each use `global_min = 0.0`; must match across parts for cross-part Z continuity.

### 5.3 Why coarse and fine abut with NO gap and NO overlap (integer identity)

Base lattice: base-cell `f` spans world-X `[center_x + 2·size_base·f, center_x + 2·size_base·(f+1)]` (pitch `2·size_base`; `BrickSize` is a half-extent — `map_tab.rs:1223-1229`). A part's NW corner is placed at base-cell index `nw_base_x`, **required to be a multiple of `m`** (`nw_base_x = m·q`). Offset computed on the BASE lattice; bricks step at the COARSE pitch:

```
offset_x = tile_world_offset(nw_base_x, nw_base_y, global_cells_w, global_cells_h, size_base).0   // grid.rs:472
coarse cell c → edges [center_x + 2·size_base·m·(q+c), center_x + 2·size_base·m·(q+c+1)]
             = base-lattice edges at base indices m·(q+c) and m·(q+c+1)
```

So a coarse cell of multiplier `m` covers exactly base cells `[m(q+c), m(q+c)+m)` and its edges land precisely on base-lattice boundaries. A fine part (`m=1`) sharing the same `center_x` puts edges on every integer base index. **A coarse edge therefore always coincides with a fine edge — no gap, no overlap, by integer identity**, the same reasoning as `tile_bounds` after the cuts fix (`convert.rs:209-224`, `grid.rs:457-471`). The shared `center_x` is the load-bearing term: identical for every part, so the lattice is global.

**Single-part reduction (INV-REGRESSION):** one layer, `m=1`, full field, `nw_base=0`, `global_cells = field dims` → `offset = -(field_w·size_base)`, byte-identical to legacy `convert_heightfield` centering (`build.rs:337`, `convert.rs:137-138`).

**Coarse-cell snapping rule:** before export, a part's region rect is snapped to its own `m`-sub-lattice against the global base origin:
```
nw_base_x = floor(min_claimed_x / m) · m      // snap DOWN
se_base_x = ceil (max_claimed_x / m) · m      // snap UP
part_grid_w = (se_base_x - nw_base_x) / m     // exact integer by construction
```
Cells inside the snapped rect but not actually claimed are dropped at the mesher via the `keep_mask: Option<&[bool]>` path (`generate.rs:276/423`), so a part exports only claimed cells on an `m`-aligned grid.

### 5.4 Overlap / coverage rules (strict partition, higher layer wins)

**Default and only shipped mode: strict partition** — every base cell belongs to at most one part's solid column, so no coincident bricks, no Z-fight, watertight overlay. Selections will overlap (the user paints freely); resolved deterministically by **z-order precedence = panel order, top layer wins**, via a single global claim pass over visible layers:

```
claimed: Vec<Option<LayerId>>      // one slot per base cell, len = gw*gh
for layer in stack.layers.iter().rev() {            // top (highest precedence) first
    let sel = layer_selection(layer);
    for cell where sel[cell] && claimed[cell].is_none() { claimed[cell] = Some(layer.id) }
}
```
Each part's export mask = `{ cell : claimed[cell] == Some(layer.id) }`. Lower layers get overlapping cells subtracted → parts are disjoint by construction.

**Multi-resolution overlap is coarse-cell-atomic:** a coarse cell is emitted iff ALL `m×m` of its base cells are claimed by that coarse layer (none stolen by a higher, finer layer); otherwise the whole coarse cell is omitted (keep-mask false). This prevents a coarse brick overlapping a finer part. Authoring guard: snap a coarse layer's grid-box divisions to multiples of `m` so its boxes never straddle a coarse-cell boundary.

**Coverage contract (INV-COVERAGE + INV-NOCOINCIDE):** `union(emitted base cells over visible parts) == union(visible layer selections)`, no base cell double-claimed. UI surfaces a non-blocking warn chip when overlaps exist (`"{n} cells claimed by two parts — higher layer wins"`) and a conflict-stripe on conflicted boxes; unassigned cells get an info chip (`"{n} cells aren't in any part — they'll be left out"`).

### 5.5 Fine-vs-coarse LOD boundary handling

At a seam a coarse cell (one height `H_c`) abuts `m` fine cells. Horizontal alignment is exact (§5.3); heights may differ → a vertical step. **The decisive fact:** every brick column is solid from the shared floor datum up to its surface (`grid.rs:928-932`), so two adjacent columns of different heights form a **solid staircase, not a hole** — watertight by default, the step is cosmetic and never see-through (INV-SEAM default arm).

Ship two arms:
1. **Accept the staircase (DEFAULT).** Requires the shared floor datum (§5.2 item 5). Cheapest, always watertight.
2. **Reconcile seams (OPT-IN, `reconcile_seams`).** Flatten the fine part's single boundary cell-row adjacent to a coarse neighbor to the covering coarse cell's `H_c` (exact watertight match; loses fine detail only in one boundary row). Uses the global `claimed` map for cross-part edge knowledge.

The per-layer `reduce` op (`Mean`/`Max`/`Min`, default `Mean`) is the middle ground: `Max` makes the coarse surface never sit below the fine surface, eliminating the worst "sliver of floor through the step" artifact without per-edge stitching. Skirt-brick LOD walls are explicitly NOT shipped (unnecessary given solid columns).

### 5.6 World-extent budget (giant-world validation)

Per-axis hard cap: `MAX_CHUNK_INDEX_UNITS = CHUNK_SIZE_UNITS(2048) · i16::MAX(32767) = 67,106,816` units (`grid.rs:505-510`). `Position` is i32 but `to_relative` maps it to an i16 `ChunkIndex`; exceed the bound and `tile_world_offset`'s saturating i32 math **silently clamps** in release (the guard is only `debug_assert!`, `grid.rs:494-499`) — which would shear a pre-positioned part off the lattice.

**Change:** promote `offset_fits_chunk_index` (`grid.rs:517-521`, all-i64) from a debug assert into a **hard pre-export gate** returning `Result`, run for EVERY part before any brick is written:
```rust
fn validate_world_extent(parts: &[PartPlacement]) -> Result<(), Vec<ExtentViolation>>
// per part, per axis: extent = global_cells*size_base*2 (i64); far_edge = offset + extent;
// require |offset| <= MAX && |far_edge| <= MAX
```
On violation, abort with a per-part report — never clamp. This is additive; it does not change existing tiling behavior.

**Capacity:** at `size_base=5` (normal, `hscale=1`), pitch 10 units/cell → ~6.71M base cells/axis is the single-overlay ceiling. The integer-LOD system is exactly what keeps a giant world under the brick-count cap (`MAX_GRID_BRICKS=50M`, `build.rs:49`) without degrading detail: a background layer at `m=8` emits 64× fewer bricks over the same footprint. When the mosaic exceeds the cap, the UI steers the user to coarsen background (`Auto-coarsen background` one-click bumps the largest-area layer to the next multiplier and re-estimates, leaving `m=1` layers alone) or to a sector-split (independent re-centered worlds, no longer auto-overlay — reported, not auto-assembled).

### 5.7 Export functions (new vs reused)

`convert_heightfield` (single, self-centering), `convert_heightfield_tiled` (single-resolution stitched), and geographic-grid `write_tile_outputs` (`grid.rs:1077`) stay **untouched** — the regression baseline. **New in `convert.rs`:**

1. `plan_layer_parts(stack, field, base_scale) -> Result<Vec<PartPlacement>, ExportError>` — builds the global `claimed` map (§5.4, visible layers only); per layer derives claimed cells → bbox → snap to `m` (§5.3) → `PartPlacement { layer_id, m, nw_base, part_grid_dims, keep_mask, offset = tile_world_offset(nw_base_x, nw_base_y, gw, gh, size_base) }`; runs `validate_world_extent`.
2. `build_part_bricks(part, field, base_scale) -> Vec<brdb::Brick>` — `downsample(field, part, m, reduce)` → coarse heightfield; paint indices downsample by **majority / nearest** (categorical, never averaged — same rule as `PaintGrid::rotated`, `paint.rs:88`); if `reconcile_seams`, flatten boundary rows; `generate_bricks_skip_floor(coarse_field, hscale_part = m·hscale_base, vertical, part.offset, part.keep_mask, …)`; cap-check `MAX_GRID_BRICKS`.
3. `export_layer_parts(stack, field, base_scale) -> Vec<PartResult>` — ONE `derive_scale` (shared); loop parts → `build_part_bricks` → `bricks_to_save` → `write_and_install` (`convert.rs:462-530`) with stem `{sanitize_name(source_name)}_{sanitize_name(layer.name)}` (`build.rs:1180-1189`), e.g. `myterrain_Background.brdb`. `.brdb`→`Worlds/`, `.brz`→`Prefabs/` (`build.rs:1136-1144`); remove-before-write avoids brdb revision pile-up; install failure non-fatal (`build.rs:1066-1086`).

**Reused verbatim:** `derive_scale`, `cell_size_units`, `tile_world_offset`, `offset_fits_chunk_index` (now a real gate), `generate_bricks_skip_floor` + `GenOptions`, the `keep_mask` mesher path, `zones::rasterize`/`rotate_zones`, `PaintGrid::sub_window`/`rotated`, `bricks_to_save`, `write_and_install`/`install_save`/`sanitize_name`.

**view_rot bake:** the layer stack rotates at convert exactly like zones/paint today (`start_convert`, `:3252`): when `view_rot != 0`, rotate field + each layer's `regions` (`zones::rotate_zones`) + box-mask (cell-rects → corners, same corner-frame transform as `HeightField::rotated` / `PaintGrid::rotated`) onto the SAME `rotated_dims` with the SAME `theta`; originals untouched (`theta==0` byte-identical). Assert dims match before meshing.

---

## 6. Per-layer settings

Surfaced in the "Selected layer:" sub-panel, updating when a different row is clicked. The anti-confusion rule: **show only the few settings that legitimately vary per part, reuse the exact existing widgets/copy, and label everything shared "shared by all parts."**

| Setting | Widget (reused) | Notes |
|---|---|---|
| **Detail (`res_mult`)** | 4-stop segmented `[1x \| 2x \| 4x \| 8x]` (`Button::selectable`, mode-bar style), NOT a free DragValue | The headline. `1x` annotated `(finest)`. Live conversion line `ui.small("1 brick ≈ {studs_per_cell·m} studs · ~{est} bricks")`. Clamped so `m·hscale_base ≤ max_hscale`. |
| **Sea level** | `height_drag_pickable` (`:1245`) eyedropper chip | `Option<f32>`; "inherit" checkbox defaults on. |
| **Surface** | existing Surface dropdown | per-layer. |
| **Reduce op** | dropdown `Mean/Max/Min` | coarse layers only; default Mean. |
| **Reconcile seams** | checkbox | opt-in §5.5; default off. |
| **Output formats** | reuse `OutputOptions` block (`draw_export_section` `:1709`) | brdb/brz/install/overwrite/skip_floor per layer. |
| **▸ Advanced** | nested `ui.collapsing` | floor level (`height_drag_pickable`), omit-below-height. |

**NOT per-layer (shared, shown once, locked):** base cell size / studs-per-meter / micro-vs-normal / vertical exaggeration / block type. Per §5.2 these MUST be identical for alignment. The layer sub-panel simply does not offer these controls (footgun removed by omission); they live on the global Export section with copy `"Base grid: 1 brick = {base} studs · shared by all parts"`. If a future UI ever exposes them per-layer, they are greyed + tooltip `"alignment-critical — set globally"`.

---

## 7. Phasing (MVP → full, with a ground-truth acceptance test per phase)

Each phase's acceptance test is a **real ground-truth observation** (in-game overlay and/or REAL coordinate comparison), never a proxy.

### MVP (v1) — adjustable box grid + whole-box layers → single-resolution auto-overlay parts
**In:** box-grid overlay (§4.2, rotation-correct render); direct click-to-toggle box pick (popup deferred to Phase 4); `LayerStack` + minimal panel (list, `+`, click-to-activate, auto-assigned color; `visible` field present but eye UI deferred); box selection → keep-mask; pre-positioned multi-part export with shared `size_base` + shared `global_cells` from ONE `derive_scale`, `offset = tile_world_offset(nw_base, 0, gw, gh, size_base)`; `offset_fits_chunk_index` promoted to a checked error.
**Out:** any `res_mult ≠ 1` (Phase 1); lasso-in-layer (Phase 2); color picker / eye / recolor / reorder UI (Phase 3); box popup (Phase 4); per-layer settings (Phase 5); undo + persistence (Phase 6).
**Acceptance:** carve a 2×1 box grid into 2 layers, export. In Brickadia: both `.brdb` parts load into one world forming a seamless terrain, no gap/overlap at the seam. Offline REAL: west part's max world-X and east part's min world-X differ by exactly `2·size_base` (computed from written brick `Position`s). Full-field single-export with zero layers is byte-identical to today's output (INV-REGRESSION).
**Riskiest assumption:** that meshing each part with a per-part keep-mask but shared offset/size yields the same absolute positions a single full-field mesh would. **Verify by direct coordinate comparison** (mesh full field vs mesh layer-subset; assert identical positions for overlapping cells) before trusting the overlay.

### Phase 1 — Multi-resolution (integer-multiple base grid)
**In:** `res_mult ∈ {1,2,4,8}` per layer; coarse downsample (`reduce`); `size_part = m·size_base` computed explicitly; coarse `nw_base` a multiple of `m`; the §5.2 invariant set enforced.
**Acceptance:** a `m=1` detail layer abutting a `m=4` background layer. In-game: coarse edges land on every 4th fine edge, no T-junction. Offline REAL: every coarse edge-X ∈ the set of fine edge-Xs; `size_coarse == 4·size_base`; `coarse_off_cells % 4 == 0`; the detail layer's bricks are byte-identical to MVP (hard-constraint check).
**Riskiest assumption:** the centering term using `size_base` (not the coarse size) for all parts — verify with a 3-resolution overlay before declaring D1 done.

### Phase 2 — Lasso/polygon regions inside layers
**In:** `Layer.regions: Vec<Zone>`; reuse capture pipeline retargeted to active layer (§4.6); keep-mask = boxes ∪ `rasterize(regions)`; `rotate_zones` in the view_rot bake.
**Constraint:** lasso regions allowed only on `res_mult=1` layers (a lasso edge mid-coarse-cell has no `m`-aligned boundary); documented, not silently failed.
**Acceptance:** a layer of one lasso blob + two boxes exports a single part whose brick footprint (projected to cell space) equals `(boxes ∪ rasterize(regions))` cell-for-cell, and overlays correctly vs a box-only layer. `theta≠0` still registers.

### Phase 3 — Per-layer color + visibility + recolor (the Photoshop panel)
**In:** full panel — colored squares, eye toggle wired, color swatch + right-click color picker, hide-all/show-all, active-layer ember highlight, Move up/down.
**Acceptance:** add 3 layers, recolor the middle via right-click, hide it via its eye → canvas shows 2 colored selections, hidden boxes vanish; export produces exactly 2 parts. State survives mode switches.

### Phase 4 — Box top-left popup pick control + grid ergonomics
**In:** the D4-literal popup at `cell_to_screen(box_x0, box_y0)` (§4.3); marquee drag-select; corner readout reusing the 8px/28px stacking convention (`paint_pick_readout` `:2273`).
**Acceptance:** hovering a box shows the popup at its true (rotated) top-left at `view_rot=45°`; click assigns in the active layer's color; marquee assigns a rectangle. Pure-UI, no export change.

### Phase 5 — Per-layer export settings
**In:** §6 `LayerExport` overrides (surface, output formats, floor, omit-below, sea level, reduce, reconcile) with hard guards keeping `size_base`/`vertical_scale`/`micro`/datum identical; live per-layer + total estimate (`ExportEstimate` `:1609`) capped at `MAX_GRID_BRICKS`.
**Acceptance:** two layers with different `skip_floor` and formats export correctly and still overlay aligned. Offline REAL: assert all parts share identical `size` and `vertical_scale`; combined brick count enforced against 50M.

### Phase 6 — Assembly QA + layer undo/persistence
**In:** assembly-QA readout (total extent vs `MAX_CHUNK_INDEX_UNITS`, per-axis margin, seam-coincidence self-check across all part pairs, per-part + total brick count), reusing `draw_estimate_readout` `:1925`; `UndoEntry::Layers` + `record_layer_edit` + `invert_entry`; `.h2bproj` serialization of layers alongside zones/paint (the pending Phase-1b serde work, done once for all three).
**Acceptance:** QA flags a deliberately oversized mosaic before export instead of clamping; on a valid export the seam-coincidence self-check passes for every part pair (programmatic version of the in-game overlay test); undo reverses a box-pick; reload restores all layers.

---

## 8. Edge cases & failure modes (and how NOT to reintroduce the cuts bug)

The MVP and Phase 1 reuse the **exact** mechanism the recent tiling/cuts fix repaired — a feature (proven code) and the blast radius. Guards:

1. **Shared-`size` invariant.** Compute `size_base` and `global_cells` once per export, pass by value to every part; debug-assert + unit-test that every part carries byte-identical `size`/`vertical_scale`/centering-datum. Never let a layer re-derive its own base `size`. (Counters the seam bug at its root.)
2. **`offset_fits_chunk_index` as a release error** (§5.6) — multi-part absolute-coordinate export is far likelier to exceed 67.1M than a single centered field; refuse with a clear message rather than emit silently-clamped (misaligned) parts.
3. **Keep-mask reuse.** Layers feed the SAME `keep_mask: Option<&[bool]>` contract; the empty/none fast-path (`zones.rs:80`, `generate.rs:1091`) must stay byte-identical — a no-layers export equals today's output bit-for-bit.
4. **Mask vs layer separation (D3).** Layers live in `state.layers`, never mutate `state.zones`; the single-export-with-Mask path is untouched.
5. **view_rot bake.** Rotate boxes (cell-rects → corners) and regions with the SAME `theta`/`rotated_dims` as field/zones/paint; assert dims match before meshing.

Other failure modes (UI states drive the Export button + a `draw_estimate_readout` chip):

| Condition | Button | Chip | Copy |
|---|---|---|---|
| No field loaded | disabled | — | standard "load a heightmap first" empty state |
| Only default layer, nothing picked | disabled | warn | `"Pick some boxes or lasso a region to make your first part."` |
| Active layer empty (others have content) | enabled | warn | `"{layer} is empty — it won't export."` |
| All layers hidden | disabled | warn | `"All parts hidden — show at least one to export."` |
| Cells unassigned to any part | enabled | info | `"{n} cells aren't in any part — they'll be left out."` |
| Cell claimed by two layers | enabled | warn | `"{n} cells claimed by two parts — higher layer wins."` |
| One part > `MAX_GRID_BRICKS` | disabled | error | `"{layer} is too big ({est} bricks, max 50M). Raise its Detail to 2x or split it."` |
| Part offset+extent > `MAX_CHUNK_INDEX_UNITS` | disabled | error | `"This world is too large to assemble (max ~{km} km/side). Use coarser Detail on background parts."` + `Auto-coarsen background` |
| Total huge but each fits | enabled | warn | `"~{total} bricks across {n} parts — large but OK."` |
| Lasso on a coarse (`res_mult>1`) layer | blocked at pick | warn | `"Lasso is only available on full-detail (1x) parts."` |

Live `World span {km}×{km} ✓ fits` / `✗ too large` readout above the button, from `offset_fits_chunk_index` over the union of parts.

---

## 9. Testing strategy (named invariants + unit tests)

`#[cfg(test)]` in `layers.rs`, `convert.rs`, `grid.rs`; property tests fuzz random `m`, offsets, regions. REAL coordinate tests, not in-game proxies.

- **INV-REGRESSION (critical gate).** One layer, `m=1`, full-field, no regions, no paint → byte-identical bricks to legacy `convert_heightfield` (leverages the `keep_mask=None` byte-identical guarantee, `generate.rs:1091`). Any drift fails the build.
- **INV-ALIGN.** For `m ∈ {1,2,4,8}`, `nw_base` a multiple of `m`: each coarse cell edge `== center_x + 2·size_base·(integer)`; property-test that an abutting fine-part edge equals the coarse-part edge bit-for-bit (i64 math). Also `size_coarse == m·size_base`, `coarse_off_cells % m == 0`.
- **INV-POSITION (the D2 crux).** Mesh full field vs mesh a layer-subset with shared offset/size → identical `Position` for every overlapping cell.
- **INV-NOCOINCIDE.** After the z-order claim pass each base cell is claimed ≤ once; enumerate all emitted brick footprints into an integer-keyed set, assert pairwise-disjoint; includes a coarse-over-fine case asserting the coarse cell is omitted when any covered base cell is stolen.
- **INV-COVERAGE.** `union(emitted) == union(selections)`; no footprint cell uncovered or outside.
- **INV-DETERMINISM.** Same `LayerStack` + field → byte-identical `Vec<brdb::Brick>` per part and identical file digest across two runs; stable part order; stable z-order subtraction by `LayerId`.
- **INV-EXTENT.** `validate_world_extent` → `Err` for `|offset|+extent > MAX_CHUNK_INDEX_UNITS` on either axis, `Ok` just under; i64 math doesn't overflow at i32-extreme inputs (±1 boundary cases).
- **INV-VERTICAL.** `vertical` from `derive_scale(cell_m_base,…)` equals the implied vertical at `(m·cell_m_base, m·hscale_base)` for `m ∈ {1,2,4,8}` (algebraic cancellation).
- **INV-SEAM.** Default arm: every boundary column's min-Z equals the shared floor datum (no see-through). Reconcile arm: along a fine/coarse shared edge each fine boundary cell height == its covering coarse cell height.
- **INV-SNAP.** `nw_base` snapped DOWN, `se_base` UP, `part_grid_w = (se−nw)/m` exact integer for all random regions/`m`.
- **INV-MASK-SURVIVES.** Changing `grid_div` preserves a layer's owned cells (`box_mask` stored in cells, not box indices).
- **INV-UNDO.** Every mutator (add/delete/rename/recolor/eye/pick-box/commit-region/change-`res_mult`/reorder) round-trips through `record_layer_edit`/`invert_entry` bit-exactly (parity with zone/paint undo tests).

**Regression gate before any "done" claim:** `cargo test` exit 0 with the above classified REAL (not tautological); plus the per-phase in-game ground-truth observation (load all parts of a 2+-layer, 2+-resolution export into one Brickadia world; confirm seamless overlay, coarse edges on fine edges) before any "works" claim.

---

## 10. Open questions / decisions for review

**RESOLVED 2026-06-30 (user):**
- **Q1 — base cell size:** **(b) Explicit global "base detail"** set in the Export header, independent of layers; every layer's `res_mult` is relative to it and it never shifts when a layer's multiplier changes.
- **Q3 — overlap policy:** **Higher layer wins, non-blocking** (strict partition via the z-order claim pass) with a visible warn chip + canvas conflict-stripe. No hard-block.
- **Q5 — oversize policy:** **Report + one-click "auto-coarsen background" only** in v1. Sector-split is deferred (out of scope until a later phase).
- **Build order:** **Build the MVP exactly as specced (single-resolution first)**; multi-resolution is Phase 1 immediately after.

The remaining items (Q2 palette, Q4 part-naming, Q6 Mask-never-applies, Q7 `reduce`=`Mean` default, Q8 menu-reorder, Q9 per-stroke undo) **default to the recommendation stated below** unless revisited during build.

Original second-order gaps (recommendations stand for the unresolved ones):

1. **How is the base cell size chosen?** Three candidates: (a) the finest layer's `cell_m` defines the base and coarser layers are `≥1×` (clean, but a user who sets every layer coarse has no `1×` anchor); (b) a global project-level "base detail" the user sets explicitly, independent of any layer; (c) auto = the smallest `res_mult` present. **Recommendation:** (b) — an explicit shared base in the global Export header, so the base never shifts when a layer's multiplier changes. Needs confirmation.

2. **Default layer color palette.** Need a fixed 8-color set, visually distinct from the paint palette (so users don't confuse layer-identity color with paint color) and legible at `BBOX_FILL` low alpha over terrain. Proposed: high-saturation hues spaced ~45° on the wheel, but the exact list and add-order cycling is unspecified.

3. **May layers overlap, and is "higher wins" the right policy?** §5.4 assumes strict partition with top-layer precedence. Alternative: warn-and-block on any overlap (force the user to resolve it). **Recommendation:** ship higher-wins (deterministic, non-blocking) with a visible warn chip — but confirm the user doesn't want hard-block instead.

4. **Save naming for parts.** Proposed `{source_name}_{layer_name}` (e.g. `myterrain_Background.brdb`). Open: collision policy when two layers share a name (append `_2`?), whether to also emit a `_manifest.txt` listing parts + shared grid origin, and whether the default "Base" layer's part should be named just `{source_name}` (no suffix) for the common single-part case.

5. **Max-world policy when the mosaic exceeds `MAX_CHUNK_INDEX_UNITS`.** §5.6 offers `Auto-coarsen background` and sector-split. Open: should sector-split be implemented in v1 (it breaks the auto-overlay promise — the user manually positions sectors in-game), or just reported as "too large, coarsen background" until a later phase? **Recommendation:** report-only in v1, sector-split deferred.

6. **Does `Export All Parts` ever respect the global Mask?** §3/D3 say no (each part is fully its own selection). Confirm the user doesn't want an opt-in "apply the Mask to every part too" — a clean future extension but explicitly out of scope now.

7. **Coarse-layer reduce default + whether `Max` should be the default** (it best avoids see-through slivers vs `Mean`'s faithfulness). Proposed default `Mean`; flag for review.

8. **Drag-reorder vs Move up/down.** v1 ships menu-item reordering (drag in a custom egui list is fiddly). Confirm that's acceptable for the Photoshop-feel, or whether drag-reorder is a v1 must-have.

9. **Undo granularity for drag-select.** One `record_layer_edit` per drag-stroke (proposed) vs per-box. Per-stroke matches the height-stroke `commit_stroke` model and is recommended; confirm.

---

**Relevant files:** `src/gui/sculpt/sculpt_tab.rs` (mode, panel, overlay, capture, undo, view_rot bake), `src/gui/sculpt/convert.rs` (multi-part export loop, `write_and_install`), `src/gui/sculpt/heightfield.rs`, `src/gui/sculpt/paint.rs` (downsample/palette conventions), `src/gui/zones.rs` (reused region geometry), `src/gui/grid.rs` (`tile_world_offset`, `offset_fits_chunk_index`, `MAX_CHUNK_INDEX_UNITS`), `src/gui/build.rs` (`cell_size_units`, `generate_bricks_skip_floor`, `sanitize_name`, `install_save`, `MAX_GRID_BRICKS`), `src/gui/map_tab.rs` (`derive_scale`), `src/opt/generate.rs` (`keep_mask` path), `src/gui/theme.rs` (`BBOX_FILL`, `ACCENT`, status colors); **new** `src/gui/sculpt/layers.rs`. Suggested spec home: `docs/superpowers/specs/2026-06-30-sculpt-layers-design.md`.