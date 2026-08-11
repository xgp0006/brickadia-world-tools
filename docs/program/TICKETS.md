# Ticket board — Brickadia World Tools

Status legend: `todo` · `doing` · `done` · `blocked`

## Phase 0 — baseline

| ID | Title | Status |
|----|-------|--------|
| BWT-0.1 | GitHub xgp0006 + remotes + master tip | done |
| BWT-0.2 | Layers MVP commit + feature train merge | done |
| BWT-0.3 | Release rebuild + launcher symlink | done |
| BWT-0.4 | Product README / ROADMAP / IN_GAME_TEST | done |
| BWT-0.5 | Parent legacy/ + builds ignore | done |
| BWT-0.6 | Vault project + memory status | done |
| BWT-0.7 | Fidelity P0: window, downsample label, tooltips | done |

## Phase 1 — API + scaffold

| ID | Title | Status |
|----|-------|--------|
| BWT-1.1 | `heightmap::api::convert_heightmap` + egui worker wire | done |
| BWT-1.2 | Cargo workspace + `apps/desktop` Tauri | done |
| BWT-1.3 | Deno tasks (not npm) for frontend | done |
| BWT-1.4 | `dem_predict` pure API + Tauri command | done |
| BWT-1.5 | Lib tests green (≥260) | done |

## Phase 2 — Convert polish

| ID | Title | Status |
|----|-------|--------|
| BWT-2.1 | File dialogs (open/save) | done |
| BWT-2.2 | `convert:progress` events + UI bar | done |
| BWT-2.3 | **Install to Brickadia Worlds** from Tauri Convert | done |
| BWT-2.4 | `api::install_save` extracted (shared egui/Tauri) | done |
| BWT-2.5 | Convert UX: defaults, HD Map tip, error mapping | done |
| BWT-2.6 | Smoke: example_maps → .brdb via Tauri command test/CLI | done |
| BWT-2.7 | Phase 2 exit checklist in PRD signed off | todo |

## Phase 3 — Map (Tauri)

| ID | Title | Status |
|----|-------|--------|
| BWT-3.1 | MapLibre GL (or Leaflet) basemap in Svelte | done |
| BWT-3.2 | Bbox draw + edit + lat/lon readout | done |
| BWT-3.3 | Live `dem_predict` panel (m/cell, cells, notes) | done |
| BWT-3.4 | Feature `dem` / expose fetch+build without full egui | done |
| BWT-3.5 | `dem_fetch_build` command + `build:progress` events | done |
| BWT-3.6 | DEM source picker (Terrarium/Mapbox/OpenTopo/3DEP) + keys | done |
| BWT-3.7 | Install + overwrite + output name | done |
| BWT-3.8 | Grid/tile path (or link “use Grid in egui until…”) | todo |
| BWT-3.9 | Phase 3 exit: one US 3DEP + one Terrarium world installed | todo |

## Phase 4 — Sculpt

| ID | Title | Status |
|----|-------|--------|
| BWT-4.1 | Heightfield document model API (load DEM/PNG, ops, export) | done |
| BWT-4.2 | WebGL/canvas height preview | done |
| BWT-4.3 | Raise/Lower/Smooth/Flatten/Set | done |
| BWT-4.4 | Export via convert path | done |
| BWT-4.5 | Stamps, paint, zones, layers parity | todo |

## Phase 5 — Cutover

| ID | Title | Status |
|----|-------|--------|
| BWT-5.1 | Desktop entry → Tauri binary | todo |
| BWT-5.2 | Symlink `brickadia-world-tools-gui` dual-run period | todo |
| BWT-5.3 | Archive egui bin behind feature or docs-only | todo |
| BWT-5.4 | README single-path user docs | todo |

## Fidelity track

| ID | Title | Status |
|----|-------|--------|
| BWT-F0 | Downsample rename + window + tooltips | done |
| BWT-F1 | MAX_DEM_CELLS 400k + USGS 3DEP wire | done |
| BWT-F2 | OpenTopo COP30 (or extra demtype) | todo |
| BWT-F3 | Streaming/columnar mesher design + impl | todo |
| BWT-F4 | Upstream wedge/hdmap selective harvest | todo |
| BWT-F5 | In-game FLATS_PER_BRICK confirm | todo |

## House / process

| ID | Title | Status |
|----|-------|--------|
| BWT-H1 | Clippy -D warnings CI | todo |
| BWT-H2 | Program PRDs fleshed (this tree) | doing |
| BWT-H3 | Vault tickets mirrored | todo |
