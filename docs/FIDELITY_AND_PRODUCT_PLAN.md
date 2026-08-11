# Fidelity, window scale, tooltips, Tauri — plan (2026-08-11)

Status: research + decision record. Current app **works** (fetch → mesh → install). Goal: higher terrain fidelity, clearer tools, correct window behaviour — without throwing away the working pipeline.

## 1. What limits fidelity today

Fidelity has **four independent axes**. Raising one without the others feels “low res.”

| Axis | What it is | Ceiling in our code | Notes |
|------|------------|---------------------|--------|
| **A. DEM source ground pitch** | Real metres per elevation sample | AWS Terrarium / Mapbox RGB: **z15** (~4–5 m/cell mid-lat). OpenTopo SRTMGL1: **~30 m**. USGS 3DEP: **1 m** (US) — **picker only, not wired** | You cannot invent metres the DEM never had. |
| **B. Fetch zoom / cell budget** | How many DEM cells we keep for a bbox | `pick_zoom` + `MAX_DEM_CELLS = 200_000` + `MAX_TILES_PER_FETCH = 64` (`src/gui/tiles.rs`) | Larger bbox → auto lower zoom → coarser. By design (mesher RAM). |
| **C. Density knob** | Post-fetch grid size | `density_factor` 1–8 = **downsample** (`build::downsample`) | **Larger density = less detail.** Misnamed for “increase fidelity.” 1 = full. |
| **D. Brick playable size** | Studs per real metre | `studs_per_meter` + micro | Free map size at **same** cell count — does **not** add DEM detail. |

### Mesher memory (the hard wall)

Comment in `tiles.rs`: greedy mesher with per-pixel satellite colors → peak RAM scales roughly `cells^1.5`. `MAX_DEM_CELLS=200k` targets ~safe on 16 GB hosts; this desktop has **60 GB** → room to raise for local builds, but not unlimited.

**Ponytail note (already in tree):** streaming/columnar mesher in `opt/generate.rs` would lift the cell budget honestly. Until then: grid/tiled export is the correct path for large high-res areas.

### Density UX trap

Label: “Density (terrain resolution)” + tooltip correctly says downsample, but the **word density** reads as “more detail.” Players turn it **up** and get **worse** terrain. Fix: rename to **Downsample / Coarser grid**, default 1, warn when >1.

## 2. Paths to higher fidelity (ranked)

| Priority | Work | Effort | Payoff |
|----------|------|--------|--------|
| **P0** | **Rename + clarify Map controls** (density, scale, DEM m/cell, predicted studs) so “fidelity” is understandable | S | Stops self-sabotage |
| **P0** | **Window scale fix** (see §4) | S | Usability |
| **P0** | **Game-linked tooltips** pass (Map + Sculpt + Convert) | M | Toolset clarity |
| **P1** | **Raise `MAX_DEM_CELLS` (config or tier)** + expose “max detail” preference; keep hard backstop | S–M | More cells / higher zoom before step-down |
| **P1** | **Wire USGS 3DEP** (1 m LiDAR, CONUS) via National Map / elevation API — same GeoTIFF path as OpenTopo | M–L | Real fidelity leap (US only) |
| **P1** | **Grid Build as default for large high-res boxes** — force tile so each tile hits source max zoom | M | Area × fidelity |
| **P2** | **OpenTopo extra products** (e.g. COP30, higher tiers where key allows) | M | Global mid-res options |
| **P2** | **Streaming / columnar mesher** — remove plane-per-color explosion | L | Structural unlock for multi-M cells |
| **P2** | **Upstream algorithm harvest** (see §3) | M | Better surfaces, not more samples |
| **P3** | Super-sample / bicubic when *down*sampling only | S | Cosmetic when density>1 |

### Practical recipes (today, no code)

| Goal | Settings |
|------|----------|
| Finest free global DEM | AWS Terrarium, **density 1**, small box, watch status `~m/cell` |
| Playable big map, same detail | Raise **studs/m** (or micro), not density |
| Large area, keep detail | **Grid Build** / tile — not one giant box |
| US ultra-detail | Need 3DEP wired (or offline GeoTIFF → Convert/Sculpt) |

## 3. Upstream (Meshiest) — what still applies

Upstream `master` has **diverged hard**: heightmap + **img2brick, text, audio, video, MIDI**, WASM (`trunk`), wedge/terrain surface modes. Our product is **DEM world tools** (Map/Convert/Sculpt). Most of their modes are **out of scope** unless we deliberately re-expand.

### Worth learning / cherry-picking

| Upstream idea | Relevance |
|---------------|-----------|
| `--hdmap` RGB high-detail heightmap | We still have `opt_hdmap` in Convert state — verify wired + document for Stage-1 PNGs |
| `--wedge` / terraced wedge / micro-wedge terrain | Alternate surface (not more DEM res) — interesting Sculpt/export style |
| `--greedy` / surface ramps | Meshing quality |
| WASM + trunk web GUI | Proves browser shell; **not** our current product path |
| Theme / sandbox gallery | UX polish reference |

### Not compatible / not useful for our fork

- Audio/MIDI/video pipelines (different product)
- Assuming their Map/DEM path is “more advanced” — **our Map tab is the advanced fork**; upstream is broader media→bricks
- Blind merge of upstream `master` — would reintroduce huge surface area and fight our sculpt/grid stack

**Do:** selective read of their wedge/greedy/hdmap commits; **don’t:** full merge.

## 4. Window scaling

Current (`gui_main.rs`):

- Default size **960×720**, min **720×540**, resizable
- **`.with_always_on_top()`** at launch — checkbox in UI starts `always_on_top: false` → **state desync** (window on top until toggled once)
- No explicit `with_maximized`, no DPI policy, no persist size/pos

Likely issues (to verify in-session):

1. Always-on-top + multi-monitor HiDPI → feels “stuck” / wrong size
2. Fixed logical min size may be tight on small or fractional-scale displays
3. Map side panels use fixed widths (`desired_width(260)` etc.) — usually OK with egui scale, but canvas may not reclaim space
4. Convert tab hyperlink still points at **brickadia-community** not xgp0006

**Fix package (small):**

1. Remove default always-on-top; only apply via checkbox
2. Sync initial checkbox with viewport
3. Persist window size/pos in config.toml
4. Optional: default larger (1280×800), max unconstrained
5. Respect system `pixels_per_point` (eframe default — confirm no override)

## 5. Tauri + Svelte migration

| | Stay egui | Move Tauri + Svelte |
|--|-----------|---------------------|
| Map | walkers already works | Rewrite map (Leaflet/MapLibre) + all DEM UI |
| Sculpt | Immediate-mode canvas + GPU-ish paint | WebGL/Canvas rewrite of entire sculpt |
| Mesh/export | In-process Rust | IPC to same Rust core (good seam) |
| Ship model | Single native binary | Dual: Rust sidecar + web UI + packager |
| Effort | Incremental | **Multi-month rewrite** of working UI |
| Risk | Low | High regression on Map/Sculpt |

**Verdict: do not migrate now.**  
Core pipeline is already Rust; the cost is **rewriting every interactive surface** that already works. Tauri only if a hard product goal is web deploy, plugin marketplace, or multi-window browser UX — after fidelity/tooling stabilize.

**If ever:** extract pure `build`/`opt`/`tiles` as a library + CLI first (already mostly true), then thin shell — not a big-bang Svelte rewrite of sculpt.

## 6. Tooltips / toolset clarity

Existing: `SculptMode::help`, `SculptTool::help`, DEM tooltips, density/scale hover text.

Gaps:

- Weak **Brickadia mapping** (studs, flats, 1 brick = 3 flats pending, World vs Prefab, install path, overwrite `-2` trap)
- Density wording inverse of user intent
- Convert vs Map vs Sculpt overlap not explained
- Hyperlink/outdated product blurb on Convert header

**Pass structure:**

For each control, three lines:

1. **Does** — what changes in our data  
2. **In-game** — what the player sees / load path  
3. **Pair with** — related knobs (e.g. studs/m ↔ predicted size ↔ density)

Target surfaces: Map brick section, Map status bar, Sculpt surface/build/output, Layers, Zones.

## 7. Suggested execution order (house → product)

1. **Quick wins (no Spec):** window always-on-top fix + persist size; density rename; Convert header/link; tooltip pack  
2. **Fidelity Spec A:** cell budget tiers + UI “Max detail” + honest status when zoom capped  
3. **Fidelity Spec B:** USGS 3DEP wire-up (or offline LiDAR GeoTIFF path documented first)  
4. **Fidelity Spec C:** mesher streaming (only if A/B still insufficient)  
5. **Upstream harvest:** hdmap verify + optional wedge surface (feature flag)  
6. **Tauri:** only after explicit product decision + grill-me

## 8. Acceptance sketches

| Item | Done when |
|------|-----------|
| Fidelity clarity | Status shows m/cell + cells + why zoom is not max; density cannot be misread |
| Higher fidelity | Same 1 km box yields measurably more cells OR finer DEM source than z15 Terrarium where available |
| Window | Resize/maximize sticks; not always-on-top by default; usable on 1.5×/2× scale |
| Tooltips | New user can set Map knobs without vault; each sculpt tool names game effect |
| Tauri | N/A until decided |

## Related

- `ROADMAP.md`, `docs/IN_GAME_TEST.md`
- Vault [[brickadia-world-tools]] · [[brickadia-heightmap-toolkit]]
- Code: `tiles.rs` (`MAX_DEM_CELLS`, `pick_zoom`), `dem_sources.rs`, `map_tab.rs`, `opt/generate.rs`, `gui_main.rs`
