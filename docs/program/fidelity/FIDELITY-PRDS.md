# Fidelity track PRDs

Backend/quality work that improves worlds regardless of shell.  
Parent analysis: [../../FIDELITY_AND_PRODUCT_PLAN.md](../../FIDELITY_AND_PRODUCT_PLAN.md) (some lines stale — 3DEP **is** wired; MAX_DEM_CELLS **400k**).

## F0 — Clarity (done)

Downsample rename, window defaults, game-linked tooltips. Tickets BWT-F0, BWT-0.7.

## F1 — Budget + 3DEP (done)

- `MAX_DEM_CELLS = 400_000`  
- USGS 3DEP National Map ImageServer → GeoTIFF decode  
- Map-tab predict + source selectable  
Tickets BWT-F1.

**Still user-facing:** large boxes coarsen; Grid for area×detail; in-game proof.

## F2 — OpenTopo extra products

**Goal:** Optional demtype COP30 / better global mid-res where key allows.

**Acceptance:** picker or advanced dropdown; area caps from OpenTopo docs; tests for demtype string wiring.

Ticket BWT-F2.

## F3 — Streaming / columnar mesher

**Full spec + implementation plan:** [F3-STREAMING-MESHER.md](./F3-STREAMING-MESHER.md)

Strip/column streaming first (`GenOptions.streaming_mesh`); flag off = byte-identical; then Auto + raise `MAX_DEM_CELLS`. Ticket BWT-F3.

## F4 — Upstream harvest

[F4-UPSTREAM-HARVEST.md](./F4-UPSTREAM-HARVEST.md) — HD Map already wired; no full merge.

## F5 — In-game FLATS + Phase 3 exit

**Full procedure:** [F5-FLATS-AND-IN-GAME.md](./F5-FLATS-AND-IN-GAME.md)  
**Constants:** `src/brick_units.rs`  
**Short checklist:** `docs/IN_GAME_TEST.md`  
Tickets BWT-F5 + BWT-3.9 — human measurement required.
