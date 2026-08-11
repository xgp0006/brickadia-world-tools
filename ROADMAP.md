# Roadmap — Brickadia World Tools

## Done (on `master`, 2026-08-11)

- [x] Map-tab DEM pipeline (Terrarium / Mapbox / OpenTopo)
- [x] Horizontal scale + predicted output size
- [x] Grid / tiled large-world generation + `.brz` prefabs
- [x] Sculpt terrain editor + export tooling
- [x] Freedraw omit/include zones (Phase 1a)
- [x] Shape stamps, splat paint (color), terrace
- [x] Free-angle rotation baked into export; bricks+flats height UI
- [x] Export **layers** (box selection + claim pass + multi-save) — MVP
- [x] GitHub: `xgp0006/brickadia-world-tools` (upstream = Meshiest)
- [x] Merge feature train → `master`; tidy parent `legacy/`

## Next (feature PRDs)

Full analysis: [docs/FIDELITY_AND_PRODUCT_PLAN.md](./docs/FIDELITY_AND_PRODUCT_PLAN.md).

| Priority | Item | Notes |
|----------|------|--------|
| P0 | **Fidelity clarity + window + tooltips** | Density rename (downsample≠detail); always-on-top desync; game-linked help |
| P1 | **Higher DEM fidelity** | Raise cell budget (host-aware); wire USGS 3DEP 1m; Grid at max zoom |
| P1 | **Layers Phase 2+** | Lasso regions, per-layer settings, resolution multiplier (see sculpt-layers-design) |
| P1 | **Project save** `.h2bproj` | Phase 1b zones/sculpt state — specs under freedraw-zones |
| P2 | Streaming/columnar mesher | Lifts `MAX_DEM_CELLS` honestly (`opt/generate.rs`) |
| P2 | Upstream harvest | `hdmap` verify, optional wedge surface — **not** full merge |
| P2 | Per-cell **material** paint | Widen greedy `(height,color)` key in `opt/generate.rs` |
| P2 | Heightmap PNG **scale metadata** | Avoid studs/m loss on re-import |
| P3 | Live brick preview (optional GPU) | Preview only; meshing stays CPU |
| P3 | Prefab scatter / WorldPainter-style layers | Roadmap #3 from design era |
| — | **Tauri + Svelte** | Deferred — rewrite cost ≫ gain while egui pipeline works |

## Housekeeping backlog

- [ ] In-game confirm **1 brick = 3 flats** (`FLATS_PER_BRICK`)
- [ ] Clippy `-D warnings` clean on CI
- [ ] Optional: rename user-facing desktop file only (crate names stay `heightmap*`)

## Spec index

| Spec | Topic |
|------|--------|
| `docs/superpowers/specs/2026-06-24-grid-tiled-world-generation.md` | Grid / tiles |
| `docs/superpowers/specs/2026-06-24-sculpt-terrain-editor-design.md` | Sculpt MVP |
| `docs/superpowers/specs/2026-06-24-sculpt-export-tooling-design.md` | Export tooling |
| `docs/superpowers/specs/2026-06-25-freedraw-zones-design.md` | Zones |
| `docs/superpowers/plans/2026-06-25-freedraw-zones-phase1a-plan.md` | Zones plan |
| `docs/superpowers/specs/2026-06-29-sculpt-player-facing-ux-design.md` | UX / rotation |
| `docs/superpowers/specs/2026-06-30-sculpt-layers-design.md` | Layers |

Superseded claims live in git history + vault `[[brickadia-heightmap-toolkit]]` changelog sections.
