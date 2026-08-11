# Phase 3 — Tauri Map (MapLibre + DEM pipeline)

**Status:** Not started  
**Tickets:** BWT-3.1–3.9  
**Depends on:** Phase 2 exit (install path recommended before Map install)

## Goal

Map happy path in Tauri equals egui Map for **single-box** generate+install:

1. See basemap  
2. Draw bbox  
3. See predicted m/cell / cells (`dem_predict`)  
4. Fetch DEM + mesh + write + install  
5. Sources: Terrarium, Mapbox RGB, OpenTopo, **USGS 3DEP**

## Why MapLibre (not walkers)

- walkers stays on **egui** — correct choice for that shell  
- Tauri webview → **MapLibre GL JS** (or Leaflet if MapLibre blocked): industry basemap, bbox tools, scales well  
- DEM fidelity still 100% Rust core  

## Architecture

```
Svelte Map page
  MapLibre map + draw control
  Settings: DEM source, downsample, studs/m, brick type, keys
  Panel: dem_predict live
  Button: Build → dem_fetch_build
       events: build:progress
Rust
  api::predict_dem_cells (done)
  api or feature dem::fetch_and_build (NEW — peel from gui/build.rs)
  api::install_save (Phase 2)
```

## Work packages

### BWT-3.1–3.3 · Frontend map shell (no DEM network)

- Add `maplibre-gl` (+ draw plugin) via Deno/npm package.json  
- Full-page or split: map | controls  
- Rectangle bbox → north/south/east/west state  
- Call `dem_predict` on bbox/source change; show m/cell, cells, notes  

**Acceptance:** screenshot/viewport — basemap loads, box drawn, predict updates without crash.

### BWT-3.4–3.5 · DEM feature + `dem_fetch_build`

Hardest Rust piece: today fetch/mesh lives in `gui` behind `feature = "gui"`.

**Preferred approach:**

1. New Cargo feature `dem` = `ureq`, `tiff`, `serde_json`, `toml`, `dirs` + modules `tiles`, `dem_sources`, `imagery_sources`, `build` (or slim `build_dem.rs`) **without** egui/walkers.  
2. `gui` feature depends on `dem`.  
3. Tauri depends on `heightmap` with `features = ["dem"]` (not full gui).  
4. Command:

```rust
dem_fetch_build(req: DemBuildRequest) -> Result<DemBuildResult, String>
// emits build:progress { phase, frac }
```

**Acceptance:** `cargo test --features dem` (or lib with dem) green; Tauri can build a small Terrarium box offline-mocked or live.

### BWT-3.6–3.7 · Sources + install UI

- Source dropdown + Mapbox/OpenTopo key fields (reuse config.toml path)  
- Output name, overwrite, install checkbox  
- Status line mirrors egui zoom readout  

### BWT-3.8 · Grid

- **MVP:** single-box only; button “Large area? Use Grid in egui” OR minimal NxM later  
- Do not block Phase 3 exit on full grid parity  

### BWT-3.9 · Exit proof

| Test | Pass criteria |
|------|----------------|
| Terrarium small global box | `.brdb` + optional install |
| 3DEP small CONUS box | higher cell density than Terrarium for same km² when budget allows |
| Predict honesty | m/cell matches post-fetch within reasonable band |

## Non-goals

Sculpt tools, layers UI, audio/video upstream modes.

## Risks

| Risk | Mitigation |
|------|------------|
| dem feature split huge | Slice: first expose install only; then OpenTopo-style single function; then full BuildRequest |
| MapLibre Wayland WebView | Spike early; fallback Leaflet |
| Token secrets in errors | Keep `redact_secrets` on all paths |
