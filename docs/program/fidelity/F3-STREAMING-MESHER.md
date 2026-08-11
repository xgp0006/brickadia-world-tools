# F3 — Streaming / columnar mesher

**Ticket:** BWT-F3  
**Status:** Spec + implementation plan fleshed 2026-08-11 — **code not started**  
**Priority:** After Grid (shipped). Unlocks multi-M cells in a **single** box without auto-coarsen.  
**Related:** `MAX_DEM_CELLS` in `src/gui/tiles.rs` · `build_planes` / `mesh_planes` in `src/opt/generate.rs` · Grid already tiles **area** (orthogonal)

---

## 1. Problem (ground truth)

### What the greedy path does today

```
heightmap + colormap
  → unique (height, color) pairs
  → build_planes: one Vec<BitMask> per pair, one BitMask per image column
  → mesh_planes: greedy_mesh_binary_plane per plane (parallel)
  → emit_column_bricks per quad
```

With **per-pixel unique satellite colors**, plane count ≈ cell count. Peak RAM scales roughly **O(cells × planes)** for mask storage and worse under mesh temps — comment in `tiles.rs` approximates **~cells^1.5**.

### Soft limit (not a product feature)

| Constant | Value | Role |
|----------|-------|------|
| `MAX_DEM_CELLS` | 400_000 | `pick_zoom` / OpenTopo / 3DEP coarsen so mesh doesn't OOM |
| `MAX_TILES_PER_FETCH` | 64 | Network tile budget (separate) |

Users experience this as “big box = low zoom = soft terrain” even on a 60 GB host.

### What Grid does *not* fix

Grid keeps **per-tile** cells under budget. It does **not** let one contiguous 2 km × 2 km CONUS LiDAR patch mesh as **one** high-res world without tiling. F3 is the single-box unlock; Grid remains the large-area product tool.

---

## 2. Goals / non-goals

### Goals

1. Mesh **≥ 1_000_000 cells** monochrome (no imagery) on a 32 GB host without OOM.  
2. Mesh **≥ 400_000 cells** with dense unique colors (worst case) without OOM — or degrade to strip path automatically.  
3. **Silhouette parity** for monochrome / low-color terrains vs current greedy (byte-identical preferred; if not, max relative brick-count delta documented).  
4. Keep `GenOptions` surface stable; new mode is opt-in or auto behind a flag.  
5. Progress callback + cancel still work.

### Non-goals

- GPU export meshing  
- Changing `.brdb` format  
- Replacing Grid for continent-scale  
- Perfect global greedy under unique colors (may accept more bricks)

---

## 3. Approach — three phases

### Phase A — Column / strip streaming (**implement first**)

**Idea:** Never hold all planes for the full image. Process **vertical strips** of columns (e.g. 64–256 columns wide).

#### Algorithm (A1 — strip greedy)

```
strip_w = min(W, STRIP_COLS)   // e.g. 128
for x0 in 0..W step strip_w:
  x1 = min(x0 + strip_w, W)
  sub = heightmap/colormap view [x0, x1) × [0, H)
  planes = build_planes(sub)           // pairs only within strip
  quads  = mesh_planes(planes)
  for quad in quads:
    // shift quad.x by x0 into full-world coords
    emit_column_bricks(..., offset_x adjusted)
  drop planes/quads
```

**Merge quality:** Merges **cannot** cross strip boundaries → more bricks on flat monochrome than global greedy. Target **≤ 1.5×** brick count vs global on monochrome 512² plateau (measure; if worse, widen strips or add A2).

#### Algorithm (A2 — overlapping strips, optional)

Strips of width `S` with overlap `O` (e.g. 32). Emit only quads whose **center x** falls in the exclusive core `[x0+O, x1-O)` to avoid double bricks. More complex; only if A1 brick inflation is unacceptable.

#### Algorithm (A3 — height-run columns, optional ponytail)

Per column x, scan y runs of constant `(h, color)`, emit as 1×N quads without BitMask planes. Worst for XY merge, best for RAM. Fallback when unique pairs in a strip still explode.

### Phase B — Color / height key compression

Apply **before** strip build when imagery is dense:

1. **Height:** already discrete `u32` from heightmap; optional quantize to flats for plane key only (render height still full).  
2. **Color:** bucket RGBA to palette of size P (8–256), matching splat paint spirit.  
3. Cap unique keys per strip at `MAX_PLANE_KEYS` (e.g. 4096); excess → A3 column runs for those cells.

### Phase C — Out-of-core brick list

If brick `Vec` itself is the limit:

1. Stream bricks to a temp file / chunked writer.  
2. Or install path that writes brdb incrementally (depends on `brdb` crate capabilities — research before coding).

---

## 4. API surface (proposed)

```rust
// GenOptions or parallel flag
pub streaming_mesh: bool,  // default false for byte-identity
// or
pub mesh_mode: MeshMode::Global | Strip { cols: u32 } | Auto,
```

**Auto rule (recommended default later):**

```
if cells > MAX_DEM_CELLS / 2 || unique_pairs_estimate > 50_000:
  MeshMode::Strip { cols: 128 }
else:
  MeshMode::Global
```

`unique_pairs_estimate`: reservoir sample of N cells or exact count if cells < 100k.

Public entry: still `gen_greedy_heightmap` / `gen_opt_heightmap` — branch inside.

---

## 5. File / module plan

| Path | Change |
|------|--------|
| `src/opt/generate.rs` | Extract `build_planes` / `mesh_planes` to take width range; add `gen_greedy_heightmap_strips` |
| `src/opt/greedy.rs` | Unchanged if plane API same |
| `src/util.rs` `GenOptions` | New field + default `false` |
| `src/gui/build.rs` / Map / Sculpt convert | Pass flag or Auto |
| `src/gui/tiles.rs` | Raise or tier `MAX_DEM_CELLS` when streaming on |
| `src/api/dem_build.rs` | Optional request field `streaming_mesh` |
| Tests | See §7 |

---

## 6. Implementation stages (agent checklist)

| Stage | Work | Exit gate |
|-------|------|-----------|
| **A0** | Unit tests documenting **current** global greedy brick count on monochrome 64×64 + 256×256 plateau fixtures | Baseline numbers recorded in test names/comments |
| **A1** | Implement strip path behind `streaming_mesh: true` only | Compiles; flag off = **byte-identical** to today (existing tests) |
| **A2** | Strip path monochrome: silhouette coverage tests (every cell with h>0 has a brick top at expected Z band) | Green |
| **A3** | Benchmark script or `#[ignore]` test: 1024² monochrome, report peak RSS + brick count | Numbers in PR description |
| **A4** | Auto mode + Map-tab / dem_build expose “Streaming mesh” or Auto | UX tooltip: when it engages |
| **A5** | Raise `MAX_DEM_CELLS` to 1e6 **only when** streaming/Auto on | pick_zoom uses effective budget |
| **B1** | Color palette bucket for strip path when unique pairs > threshold | Test with random RGB colormap |
| **C0** | Spike: can brdb write stream? | Spike note only |

**Do not** raise global `MAX_DEM_CELLS` for legacy path until A1–A3 green.

---

## 7. Acceptance tests (concrete)

### Identity (flag off)

- All existing `opt::generate` + sculpt convert + grid tests stay green.  
- Explicit: `streaming_mesh: false` byte-identical brick dump for fixture X (hash or exact vec).

### Strip correctness (flag on)

1. **Coverage:** monochrome ramp; every non-culled cell contributes to at least one brick footprint (or skip_floor rules).  
2. **No double solid:** no two bricks same (x,y) full column overlap (harder — at least no identical position+size duplicates).  
3. **Height:** plateau of constant h → top Z matches global path within parity rounding.  
4. **Brick count inflation:** monochrome 256² plateau, strip_cols=128:  
   `count_strip / count_global ≤ 1.5` (tune threshold if needed; document if raised).

### Stress

| Fixture | Cells | Color | Pass |
|---------|-------|-------|------|
| plateau_mono_1024 | 1M | 1 | Completes &lt; 60s, RSS peak &lt; 12 GB |
| noise_rgb_512 | 262k | unique | Completes without OOM (may use B1) |
| real_terrarium_crop | ~200k | imagery | Visual gate (user) optional |

---

## 8. Risks

| Risk | Mitigation |
|------|------------|
| Seam brick seams at strip edges | Overlap (A2) or accept count inflation |
| Identity tests too strict | Hash only positions+sizes of sorted bricks |
| Auto engages too often | Thresholds + status line “Streaming mesh ON” |
| Grid + streaming interaction | Grid tiles already small — keep streaming off for grid tiles by default |

---

## 9. Effort estimate

| Stage | Size |
|-------|------|
| A0–A2 | M (1–2 focused sessions) |
| A3–A5 | M |
| B1 | S–M |
| C0 | S spike |

---

## 10. Decision log

| Date | Decision |
|------|----------|
| 2026-08-11 | Spec only; Grid ships first for area×detail |
| 2026-08-11 | Prefer strip streaming (A) before full rewrite of plane model |
| 2026-08-11 | Default remains global for byte-identity until Auto proven |

---

## 11. Handoff prompt (for implementer agent)

```
Implement F3 Phase A1–A2 per docs/program/fidelity/F3-STREAMING-MESHER.md.
- GenOptions.streaming_mesh default false; flag off = byte-identical.
- Strip width 128; shift quads into world X.
- Tests: identity off; monochrome strip coverage + brick-count ratio ≤1.5 on 256².
- Do not raise MAX_DEM_CELLS yet.
cargo test --lib must stay green.
```
