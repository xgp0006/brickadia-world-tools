# In-game test checklist (Brickadia)

**Full procedures:**  
- Phase 3 exit + FLATS calibration → [`program/fidelity/F5-FLATS-AND-IN-GAME.md`](./program/fidelity/F5-FLATS-AND-IN-GAME.md)  
- Streaming mesher plan → [`program/fidelity/F3-STREAMING-MESHER.md`](./program/fidelity/F3-STREAMING-MESHER.md)

Run after `cargo build --release --features gui` (or Tauri Map build).  
Game: Steam **2199420** (Proton).

**Worlds:**  
`~/.steam/steam/steamapps/compatdata/2199420/pfx/drive_c/users/steamuser/AppData/Local/Brickadia/Saved/Worlds/`

---

## BWT-3.9 — Phase 3 exit (load worlds)

Tooling smoke (no game): `~/.local/share/heightmap2brz/builds/smoke-terrarium.brdb`.

| # | Check | ☐ |
|---|--------|---|
| 3A | Terrarium small box → install → load; terrain not “only spawn” | |
| 3B | USGS 3DEP CONUS small box → load; looks finer than Terrarium (or N/A) | |
| 3C | Convert `example_maps/gradient.png` → load | |

**Signed:** date ________  commit ________  shell egui/Tauri ________

---

## BWT-F5 — FLATS_PER_BRICK

Code contract (`src/brick_units.rs`):

- **1 flat** = 4 heightmap units of `h` (mesh-derived)  
- **1 brick (UI)** = **3 flats** (game convention — measure this)

Pre-flight:

```bash
cargo test --lib brick_units -- --nocapture
```

| UI | Expected plates if ratio=3 | Observed | ☐ |
|----|----------------------------|----------|---|
| `1f` | 1 | | |
| `1b` | 3 | | |
| `1b 1f` | 4 | | |
| `2b` | 6 | | |

**Verdict:** ☐ confirmed 3 · ☐ other: ____ → change `FLATS_PER_BRICK` only in `brick_units.rs`

Full step-by-step: **F5-FLATS-AND-IN-GAME.md** §4.

---

## Smoke (every release)

- [ ] Map: Terrarium + overwrite → walkable  
- [ ] Convert sample PNG  
- [ ] Scale 1 vs 4 studs/m changes world size  
- [ ] Micro matches normal physical span  

## Sculpt

- [ ] Raise/lower; undo  
- [ ] Zone omit/include  
- [ ] Rotation export orientation  
- [ ] Splat colors  
- [ ] Layers multi-save  

## Grid / tiles

- [ ] Grid 2×2 or large bbox: no seam holes  
- [ ] Max brick size: plains don’t vanish  

## Gotchas

- Overwrite or open `-2`/`-3`  
- Map basemap ≠ DEM colors  
- PNG re-import loses studs/m metadata  
