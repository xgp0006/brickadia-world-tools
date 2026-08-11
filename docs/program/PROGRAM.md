# Brickadia World Tools — Program Board

**Updated:** 2026-08-11  
**Product:** DEM/heightmap → Brickadia `.brdb`/`.brz` (desktop)  
**Shell path:** egui (shipping) → Tauri 2 + Svelte 5 + **Deno** (migration)  
**Core:** Rust crate `heightmap` (`src/api`, `opt`, `map`, `util`; DEM under `gui` until dem-feature split)

## North star outcomes

1. **Fidelity-clear:** user always knows m/cell, zoom cap, why detail is limited.
2. **US max detail:** 3DEP path works for small CONUS boxes end-to-end in-game.
3. **Shell modern:** Tauri Convert + Map parity; Sculpt after Map; single launcher cutover.
4. **Docs as contracts:** every phase has PRD + exit gates + tickets; agents implement against tickets.

## Phase status

| Phase | Name | PRD | Status |
|-------|------|-----|--------|
| **0** | egui baseline + fidelity P0 | [phases/PHASE-0.md](./phases/PHASE-0.md) | **Done** |
| **1** | API seam + workspace + Tauri scaffold | [phases/PHASE-1.md](./phases/PHASE-1.md) | **Done** |
| **2** | Tauri Convert polished (dialog, progress, install) | [phases/PHASE-2.md](./phases/PHASE-2.md) | **Done** (exit BWT-2.7 human sign-off optional) |
| **3** | Tauri Map (MapLibre + predict + fetch/build/install) | [phases/PHASE-3.md](./phases/PHASE-3.md) | **Core done** (3.8 Grid deferred; 3.9 user in-game) |
| **4** | Tauri Sculpt MVP → parity | [phases/PHASE-4.md](./phases/PHASE-4.md) | **Next** |
| **5** | Launcher cutover + archive egui | [phases/PHASE-5.md](./phases/PHASE-5.md) | **Not started** |

## Parallel fidelity track (backend; does not wait for shell)

| ID | PRD | Status |
|----|-----|--------|
| **F0** | Clarity + window + tooltips | **Done** |
| **F1** | Cell budget + 3DEP wire | **Done** (400k cells; 3DEP ImageServer) |
| **F2** | OpenTopo COP30 / extra products | Planned |
| **F3** | Streaming/columnar mesher | Planned |
| **F4** | Upstream wedge/hdmap harvest | Planned |

Details: [fidelity/FIDELITY-PRDS.md](./fidelity/FIDELITY-PRDS.md) · parent analysis [../FIDELITY_AND_PRODUCT_PLAN.md](../FIDELITY_AND_PRODUCT_PLAN.md)

## Ticket index

All work items: [TICKETS.md](./TICKETS.md)

## Execution rules

1. **Phase order for shell** — do not start Phase 4 UI before Phase 3 Map happy path.
2. **Fidelity backend** may land on egui anytime (already shipping).
3. **Deno only** for `apps/desktop` (`deno task *` — never npm as primary).
4. **Acceptance** = ground truth (tests exit 0, real `.brdb`, screenshot/viewport for UI, in-game for FLATS).
5. **egui freeze** — bugfix + DEM only; new chrome → Tauri.

## Agent handoff

Spawn implementers with: PRD path + ticket IDs + acceptance tests from that phase file.  
Orchestrator verifies gates before marking tickets done.
