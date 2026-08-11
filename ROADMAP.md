# Roadmap — Brickadia World Tools

**Program board:** [docs/program/PROGRAM.md](./docs/program/PROGRAM.md) · [docs/program/TICKETS.md](./docs/program/TICKETS.md)

## Product

| | |
|--|--|
| **Primary app** | **Tauri** — `brickadia-world-tools` |
| **Legacy** | egui `heightmap_gui` — **deprecated** (`--features gui`) |

## Phase order

| Phase | Status | PRD |
|-------|--------|-----|
| 0 House + baseline | **Done** | [PHASE-0](./docs/program/phases/PHASE-0.md) |
| 1 API + Tauri scaffold | **Done** | [PHASE-1](./docs/program/phases/PHASE-1.md) |
| 2 Convert polish + install | **Done** | [PHASE-2](./docs/program/phases/PHASE-2.md) |
| 3 MapLibre + dem_fetch + Grid | **Done** | [PHASE-3](./docs/program/phases/PHASE-3.md) |
| 4 Sculpt Tauri | **Done** (MVP + 4.5 parity) | [PHASE-4](./docs/program/phases/PHASE-4.md) |
| 5 Launcher cutover | **Done — Tauri primary** | [PHASE-5](./docs/program/phases/PHASE-5.md) |

## Fidelity track

| | Status |
|--|--------|
| F0 clarity | Done |
| F1 3DEP + cell budget | Done |
| F2 OpenTopo COP30 | Done |
| F3 streaming mesher (Phase A) | Done |
| F4 upstream harvest notes | Done |
| F5 FLATS in-game | Fixture installed — user confirm |

## Rebuild

```bash
cd apps/desktop && deno task tauri:build
# binary: target/release/brickadia-world-tools
```
