# Roadmap — Brickadia World Tools

**Program board (phases + tickets):** [docs/program/PROGRAM.md](./docs/program/PROGRAM.md) · [docs/program/TICKETS.md](./docs/program/TICKETS.md)

## Done (on `master`)

- [x] Map-tab DEM pipeline (Terrarium / Mapbox / OpenTopo / **USGS 3DEP**)
- [x] Horizontal scale + predicted output size; downsample clarity
- [x] Grid / tiled large-world generation + `.brz` prefabs
- [x] Sculpt terrain editor + export tooling
- [x] Freedraw zones Phase 1a; stamps; splat paint; terrace; rotation bake; layers MVP
- [x] GitHub `xgp0006/brickadia-world-tools`; feature train on `master`
- [x] Fidelity P0 (window, tooltips); MAX_DEM_CELLS 400k
- [x] Tauri Phase 1–2 start: `api::convert` + `dem_predict`; Deno shell; dialogs + progress

## Phase order (shell)

| Phase | Status | PRD |
|-------|--------|-----|
| 0 House + egui baseline | **Done** | [PHASE-0](./docs/program/phases/PHASE-0.md) |
| 1 API + Tauri scaffold | **Done** | [PHASE-1](./docs/program/phases/PHASE-1.md) |
| 2 Convert polish + install | **Done** | [PHASE-2](./docs/program/phases/PHASE-2.md) |
| 3 MapLibre Map + dem_fetch_build | **Core done** (Grid deferred) | [PHASE-3](./docs/program/phases/PHASE-3.md) |
| 4 Sculpt Tauri | **Next** | [PHASE-4](./docs/program/phases/PHASE-4.md) |
| 5 Launcher cutover | Planned | [PHASE-5](./docs/program/phases/PHASE-5.md) |

## Fidelity track

| | Status |
|--|--------|
| F0 clarity | Done |
| F1 3DEP + cell budget | Done |
| F2 OpenTopo COP30 | Planned |
| F3 streaming mesher | Planned |
| F4 upstream harvest | Planned |
| F5 in-game FLATS | User |

## Spec index (sculpt/grid era)

See `docs/superpowers/specs/`.
