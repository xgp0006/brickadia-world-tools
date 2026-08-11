# F3 — Streaming / columnar mesher (design)

**Ticket:** BWT-F3  
**Status:** Design recorded 2026-08-11 — implementation not started  
**Goal:** Lift `MAX_DEM_CELLS` honestly by removing per-(height,color) plane explosion in `opt/generate.rs`.

## Problem

Greedy path builds one BitMask plane per unique `(height, color)` pair (`build_planes`). With satellite imagery colors nearly unique per cell, plane count ≈ cells → RAM ~ `cells^1.5`. Soft cap `MAX_DEM_CELLS = 400_000` is a guard, not a product limit.

## Approach (phased)

### Phase A — column streaming (ponytail path)

1. Emit bricks **per DEM column** (or band of columns) without materializing all planes at once.
2. Keep greedy merge **within** a column strip; accept slightly more bricks vs global planes.
3. Acceptance: same silhouette for blank/paint-less terrains; ≤2× brick count vs today on monochrome tests.

### Phase B — height-band planes

1. Quantize height to flats for plane keys; color bucket to N palette slots (splat already does palette).
2. Cap unique plane keys to e.g. 4k; spill to column path.

### Phase C — out-of-core

1. Spill brick chunks to temp files / append-only brdb writer if available.

## Non-goals

- GPU meshing for export
- Changing brdb format

## Exit

- Spec + unit tests for strip greedy
- Benchmark: 1M cells on 32 GB host without OOM
- Document new default `MAX_DEM_CELLS` if raised

## Related

`src/opt/generate.rs` `// ponytail:` note · `tiles.rs` MAX_DEM_CELLS · Grid path already tiles area.
