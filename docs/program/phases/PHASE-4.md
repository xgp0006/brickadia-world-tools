# Phase 4 — Tauri Sculpt

**Status:** BWT-4.1–4.5 landed (Tauri sculpt parity)  
**Tickets:** BWT-4.1–4.5  
**Depends on:** Phase 3 Map happy path (can load field from DEM or PNG)

## Goal

Sculpt workspace in Tauri: edit heightfield, export to same mesh path as today.

## Outcomes

1. **Document model** — height grid + meta (cell_m, studs) + paint/zones/layers on session  
2. **Preview** — canvas height shading (+ paint tint)  
3. **MVP tools** — Raise, Lower, Smooth, Flatten, Set (+ strength, brush size)  
4. **Export** — `convert`/greedy path → `.brdb` / install  
5. **Parity (BWT-4.5)** — stamps, paint splat, rect zones, multi-layer export

## Architecture sketch

```
api::sculpt::{
  SessionId or document bytes,
  apply_stroke(tool, brush),
  export(GenOptions) -> path
}
Frontend: map of tools → IPC strokes (debounced)
```

Prefer **server-side heightfield** in Rust (authoritative) with preview texture updates, not dual logic in JS.

## Exit gates

- Raise/lower on loaded DEM or blank canvas → export loads in Brickadia  
- Undo stack at least 1-level or stroke-based  
- Performance OK for ≥512² fields on desktop  

## MVP landed (2026-08-11)

| Ticket | Deliverable |
|--------|-------------|
| BWT-4.1 | `api::sculpt` session store: blank / load PNG / stroke / undo / preview / export / close |
| BWT-4.2 | `/sculpt` Canvas 2D greyscale height preview (min→max normalize) |
| BWT-4.3 | Raise, Lower, Smooth, Flatten, Set + radius/strength (+ target for Flatten/Set) |
| BWT-4.4 | Export: `to_dem_raster` → greedy mesh → `.brdb` (+ optional install); event `sculpt:progress` |
| BWT-4.5 | Stamp (cone/mesa/crater/ramp); paint splat + color export; rect zones keep-mask; multi-layer part export |

Pure engine under `feature = "dem"`: heightfield / brush / tools / paint / layers + `gui::zones` rasterize.  
egui-only remains: freehand/polygon zone capture UI, flood-fill paint bucket, splatmap import, tiled convert seam, full convert terrace knobs, rotation bake UI.

## Non-goals (still open)

Free-angle rotation bake UI, GPU live brick preview, freehand zone lasso in Tauri, per-part paint on layer export.
