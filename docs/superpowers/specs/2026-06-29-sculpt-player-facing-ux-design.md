# Sculpt tab — player-facing UX redesign

**Date:** 2026-06-29 · **Branch:** `feat/sculpt-ui-rotation-studs`

## Goal

Make the Sculpt panel speak to a Brickadia *player*, not a GIS/internals power user.
Express everything in **studs** (the 1×1 stud square is the atomic unit), replace
mode-checkboxes with click-modifiers, flatten the hidden collapsibles into one clean
panel, and make the advanced export knobs either smart-automatic or plainly labeled.

Scope is the Sculpt tab control panel + canvas height-pick interaction. It does **not**
change the sculpt/convert/mesh math, only how scale is expressed and which controls show.

## Locked decisions

1. **Units: studs everywhere; 1×1 stud = the base unit.** No size presets, no brick-count units.
2. **Brush size** slider reads/writes in studs ("Ø N studs"). Cells stay internal.
3. **World size** shown in studs; set via a **World width (studs)** field that back-solves the scale.
4. "Vertical exaggeration" → **Terrain height** (1× = realistic). "Micro bricks" → **Fine detail**.
5. **Height pick:** hold **`E`** + click samples the hovered cell's height into the active height value;
   normal click/drag applies. (E, not Alt — window managers grab Alt+drag to move the window, so the
   pick silently failed cross-platform.) The Eyedropper checkbox + collapsible + Pick-into selector are removed.
6. **One flat panel**, grouped Surface · Build size · Output. Exactly **one** `▸ Advanced` expander remains.
7. **Tiling: fully automatic** — auto-split when over the brick/RAM cap with a one-line notice; manual override in Advanced.
8. **Brick cap visible, plain:** "Max brick size: N studs — lower if flat areas show holes," edited in studs.

## Panel layout (after)

```
[ Shape  Stamp  Paint  Zone ]            (mode bar, iconified — unchanged)

Brush:  tool · size [Ø N studs] · strength · falloff · shape
        ℹ Hold E + click to sample a height

Surface:  Smooth ⇄ Stepped (+ step size) · Sea level [N studs] · Fill flat ground ☐

Build size:  World width [____ studs]  → 2,600 × 2,600 studs · ~82k bricks
             Terrain height [1.0×]   Fine detail ☐

Output:  name [____] · World(.brdb) ☑ · Prefab(.brz) ☐ · Install ☑ · Overwrite ☐
         (auto-tiling notice appears here when the world is too big)

[ ⬇ Convert to bricks ]   [ 🖼 Export heightmap PNG ]

▸ Advanced:  Floor level · exact Max-brick override · manual tiling + tile size
```

## Removed from the main panel

- "Studs / meter" control (now derived from World width).
- "cells" as a user-facing unit anywhere.
- Eyedropper collapsible, `pick_mode` checkbox, Pick-into Target/Floor/Omit selector → replaced by `Alt`+click.
- "Terrain shaping" / "Bricks (fix holes)" / "Tiling (big worlds)" collapsibles → flattened or automated.

## Conversions (the truth the UI must show)

- `studs_per_cell = 2·hscale·upf / 5` (existing helper; achieved integer hscale).
- **Brush:** `brush_studs = 2·radius_cells·studs_per_cell` ⇒ `radius_cells = brush_studs / (2·studs_per_cell)`.
- **World width:** `world_studs = field.width · studs_per_cell`. Setting a target width back-solves
  `studs_per_meter ≈ world_studs / (field.width · cell_m)`, then re-derives hscale and shows the *achieved* width
  (may differ slightly after integer rounding — show the achieved value, never a fake exact one).
- **Brick cap:** `studs = max_brick_units / 5` (1 stud = 5 units). Edit in studs, store units.
- **Height values are VERTICAL studs**, distinct from horizontal: a height of `m` meters is
  `vertical_studs = m · vertical_scale / 5` (`vertical_scale` from `derive_scale`, units/m). "Sea level"
  (omit-below) and "Floor level" edit in vertical studs, store meters. "Fill flat ground" is the inverse
  of the internal `skip_floor` (on = build a base plate; default off, native floor shows through).

## Smart behaviors

- **Auto-tiling:** reuse the existing `tiled_estimate`/over-cap detection; when the single-mesh estimate
  exceeds the brick/RAM cap, set the export to tile automatically and show "Big world — split into N tiles."
  No silent truncation: the notice always states the tile count.
- **Hold-E sample:** the canvas pointer path checks `eyedropper_active` (the `E` key held) → sample-into-
  active-height, retiring the `pick_mode`/`pick_into` state. (Was Alt; moved to E because WMs grab Alt+drag.)

## Out of scope (separate backlog, still pending)

- Free-angle rotation (feature 2) + slice-direction overlay (feature 3). Not part of this spec.
- Theme palette reconciliation / font ramp (deeper restyle polish).

## Testing

- Unit: `brush_studs ↔ radius_cells` round-trip at a few scales.
- Unit: World-width back-solve produces the requested width within integer-rounding tolerance.
- Unit: `max_brick studs ↔ units` conversion.
- Unit/logic: holding `E` maps to sample-into-active-height (`eyedropper_active` → action mapping).
- Regression gate: `cargo test` + `clippy -D warnings` + release build green; existing 238 tests still pass.
