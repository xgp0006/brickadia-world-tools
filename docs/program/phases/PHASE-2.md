# Phase 2 — Tauri Convert polished

**Status:** ~95% (BWT-2.7 human exit sign-off remaining)  
**Tickets:** BWT-2.1–2.7  
**Depends on:** Phase 1 done

## Goal

Convert is **production-usable** in Tauri: pick files, convert with progress, optionally **install into Brickadia Worlds** — same bytes as egui Convert.

## Done

- Dialog open/save for heightmap, colormap, out  
- `convert:progress` events + progress bar  
- Greedy / HD Map / brick mode / scales  
- Deno-only toolchain  
- **BWT-2.4** `src/api/install.rs` — shared `install_save` / `builds_dir` / Worlds path; egui `gui/build.rs` thin wrappers  
- **BWT-2.3** ConvertRequest `install` + `overwrite`; soft-fail `install_warning`; Tauri checkboxes  
- **BWT-2.5** Empty out → `builds_dir/<stem>.brdb`; HD Map Stage-1 tip; status shows installed path  
- **BWT-2.6** `api::convert` lib test on `example_maps/gradient.png`; install unit tests under `cargo test --lib`  

### Verify one-liners

```bash
cargo test --lib
cargo check -p brickadia-world-tools
cd apps/desktop && deno task build
# manual: deno task tauri:dev → Convert example_maps/gradient.png (+ Install checkbox)
```

## Exit gates (Phase 2 complete when all true)

1. Dialog + progress (done)  
2. Install path works or clear “prefix missing” warning (done in API/UI)  
3. `cargo test --lib` green; `deno task build` green (done)  
4. Docs: PROGRAM + this PRD status → Done — **BWT-2.7 human sign-off**  

## Out of scope

Map basemap, DEM fetch, Sculpt, Grid.
