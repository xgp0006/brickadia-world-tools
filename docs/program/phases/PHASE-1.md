# Phase 1 — API seam + workspace + Tauri scaffold

**Status:** Done (2026-08-11)  
**Tickets:** BWT-1.*

## Goal

UI-agnostic Rust command surface + empty (then minimal) Tauri app so Phase 2+ never re-implements mesh logic in JS.

## Architecture delivered

```
heightmap (lib)
  api::convert_heightmap
  api::predict_dem_cells
  opt / map / util
apps/desktop (Tauri + Svelte + Deno)
  commands: core_version, convert_build, dem_predict
```

## Outcomes

- [x] `src/api/` free of egui  
- [x] egui Convert worker calls `api::convert_heightmap`  
- [x] Workspace members: `.` + `apps/desktop/src-tauri`  
- [x] Deno: `deno task install|dev|build|tauri:dev`  
- [x] Convert fixture test writes `.brdb`  

## Exit gates (met)

- [x] `cargo test --lib` includes `api::` tests  
- [x] `cargo check -p brickadia-world-tools`  
- [x] `deno task build`  

## Non-goals (deferred)

- Physical `crates/heightmap-core` split (workspace root package is enough for now)
- Map/Sculpt UI
