# Phase 2 — Tauri Convert polished

**Status:** In progress (~80%)  
**Tickets:** BWT-2.1–2.7  
**Depends on:** Phase 1 done

## Goal

Convert is **production-usable** in Tauri: pick files, convert with progress, optionally **install into Brickadia Worlds** — same bytes as egui Convert.

## Done already

- Dialog open/save for heightmap, colormap, out  
- `convert:progress` events + progress bar  
- Greedy / HD Map / brick mode / scales  
- Deno-only toolchain  

## Remaining work (agents)

### BWT-2.4 · `api::install_save`

Extract Proton prefix + Worlds/Prefabs install from `gui/build.rs` into `src/api/install.rs` (or `util`) so both shells call one function.

**Acceptance:** unit test finds or mocks prefix path; no egui types; `cargo test --lib` green.

### BWT-2.3 · Install from Tauri Convert

- Checkbox “Install into Brickadia Worlds”  
- After successful convert, call install  
- Show installed path or soft-fail warning (like egui: .brdb still kept)

**Acceptance:** with Steam/Brickadia prefix present, `.brdb` appears under `…/2199420/…/Saved/Worlds/`.

### BWT-2.5 · Convert UX polish

- Default out path: `~/Projects/brickadia/builds/<name>.brdb` or last-used  
- HD Map helper text (Stage-1 RGBA)  
- Map error strings to user-facing copy  
- Link to in-game load steps (one line)

### BWT-2.6 · Automated smoke

- Rust test or `#[tauri::test]` / bin that runs convert on `example_maps/gradient.png`  
- Document manual: `deno task tauri:dev` → convert gradient  

## Exit gates (Phase 2 complete when all true)

1. Dialog + progress (done)  
2. Install path works or clear “prefix missing” warning  
3. `cargo test --lib` green; `deno task build` green  
4. Docs: PROGRAM + this PRD status → Done  

## Out of scope

Map basemap, DEM fetch, Sculpt, Grid.
