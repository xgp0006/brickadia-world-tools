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

**Goal:** Lift cell budget honestly by removing per-(height,color) plane explosion in `opt/generate.rs`.

**Acceptance:** same visual for small maps; ≥1M cells path without OOM on 32 GB; tests for byte-identity on small fixture optional.

Ticket BWT-F3. Design first (spec), then impl.

## F4 — Upstream harvest

Selective: verify HD Map end-to-end docs; optional wedge surface behind flag. **No** full Meshiest merge.

Ticket BWT-F4.

## F5 — In-game FLATS

User confirms 1 brick = 3 flats per `docs/IN_GAME_TEST.md`. Ticket BWT-F5.
