# In-game test checklist (Brickadia)

Run after `cargo build --release --features gui`. Game: Steam **2199420** (Proton).

Install path (typical):  
`~/.steam/steam/steamapps/compatdata/2199420/pfx/drive_c/users/steamuser/AppData/Local/Brickadia/Saved/Worlds/`  
Prefabs: `…/Saved/Prefabs/`

## Smoke (every release) — BWT-3.9 / BWT-F5

**Agent/tooling smoke (no game):** Terrarium write  
`~/.local/share/heightmap2brz/builds/smoke-terrarium.brdb` (2026-08-11, ~15k bricks).

**Your eyes (required for ticket close):**

- [ ] Map (egui or Tauri): AWS Terrarium + small bbox → generate → **Overwrite** if re-testing same name
- [ ] World loads; terrain visible (not “only spawn”)
- [ ] Walkable surface; spawn above peak
- [ ] Convert: `example_maps/gradient.png` → loads in-game
- [ ] **USGS 3DEP** (US box only): finer relief than Terrarium for same km² when budget allows
- [ ] **Confirm FLATS_PER_BRICK:** plateau of known height vs Brickadia UI  
  - Code: **1 brick = 3 flats**, **1 flat = 4 height units** after vscale  
  - Result: ________ (date/build)

## Smoke (continued)

- [ ] Map tab: AWS Terrarium + default bbox → generate → **Overwrite** if re-testing same name
- [ ] World loads; terrain visible (not “only spawn”)
- [ ] Walkable surface; spawn above peak
- [ ] Convert tab: sample PNG from `example_maps/` → loads in-game

## Scale / units

- [ ] Horizontal scale 1 vs 4: world size changes as expected (studs readout)
- [ ] **Micro** mode: same physical span as normal at matched settings (post-2026-06-25)
- [ ] **Confirm FLATS_PER_BRICK:** measure a known height (e.g. set plateau) vs Brickadia UI  
  - Code assumes **1 brick = 3 flats**, **1 flat = 4 height units** after `vscale`  
  - Record result here / vault if different

## Sculpt

- [ ] Brush raise/lower; undo/redo
- [ ] Zone omit: hole through tall terrain; zone include: only island kept
- [ ] Rotation 45° then export: ridge orientation matches preview (no mirror)
- [ ] Splat paint colors visible on bricks
- [ ] Layers: two layers → two saves → both load and assemble

## Tiled export

- [ ] Force tile / large bbox: multiple `.brdb` or stitched; no seam holes at joins
- [ ] Max brick size slider: flat plains don’t vanish (lower max if holes)

## Gotchas

- Stale world: enable **Overwrite existing world** or open the newest name (`-2`, `-3`…)
- Map preview is always OSM — DEM/imagery pickers do not recolor the basemap
- PNG heightmap re-import loses studs/m metadata — re-set panel values after import
