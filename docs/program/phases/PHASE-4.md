# Phase 4 — Tauri Sculpt

**Status:** Not started  
**Tickets:** BWT-4.1–4.5  
**Depends on:** Phase 3 Map happy path (can load field from DEM or PNG)

## Goal

Sculpt workspace in Tauri: edit heightfield, export to same mesh path as today.

## Outcomes

1. **Document model** — height grid + meta (cell_m, studs) + optional paint/zones  
2. **Preview** — WebGL or canvas height shading  
3. **MVP tools** — Raise, Lower, Smooth, Flatten, Set (+ strength, brush size)  
4. **Export** — `convert`/greedy path → `.brdb` / install  
5. **Parity later** — stamps, splat, zones, layers (may stay egui longer if needed)

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

## Non-goals (first cut)

Full layers multi-save, free-angle rotation bake (port after MVP), GPU live brick preview.
