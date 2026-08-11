# Tauri + Svelte migration (committed)

**Decision (2026-08-11):** migrate the UI shell to **Tauri 2 + Svelte 5**. Rust mesh/DEM/export **stays**. egui is transitional until feature parity on Map + Convert + Sculpt.

Canonical product plan (fidelity): [FIDELITY_AND_PRODUCT_PLAN.md](./FIDELITY_AND_PRODUCT_PLAN.md).

## Why

- Clearer tool UI, better layout/scale control, modern component ecosystem
- Same Rust core already isolated under `src/gui/build.rs`, `tiles`, `opt/`, `util`
- User-facing product (World Tools) benefits from a real front-end toolkit

## Non-goals (v1 of shell)

- Reimplement mesh math in JS
- Upstream audio/MIDI/video modes
- Browser-only deploy (desktop Tauri first; web optional later)

## Architecture

```
┌─────────────────────────────────────────────┐
│  Svelte 5 UI (MapLibre/Leaflet, panels)     │
│  tooltips, layout, settings, progress       │
└──────────────────┬──────────────────────────┘
                   │ Tauri commands / events
┌──────────────────▼──────────────────────────┐
│  Rust crate `heightmap` (library)           │
│  dem fetch · stitch · mesh · brdb write     │
│  install to Proton Worlds · config.toml     │
└─────────────────────────────────────────────┘
```

### Command surface (v1 sketch)

| Command | Input | Output |
|---------|-------|--------|
| `config_get` / `config_set` | keys | config |
| `geocode` | query + proximity | candidates |
| `dem_predict` | bbox + dem + density + scale | m/cell, zoom, predicted studs, cell budget |
| `dem_fetch_build` | full build request | progress events → path / install result |
| `convert_build` | heightmap paths + gen options | path |
| `sculpt_*` | TBD (stateful session or document model) | field ops / export |
| `open_path` / `reveal` | path | OS |

Progress: Tauri events `build:progress { phase, frac }` (mirror current channel).

### Hard parts (order risk)

1. **Map tab** — replace `walkers` with MapLibre GL or Leaflet; bbox draw; basemap switch  
2. **Sculpt** — WebGL heightfield + paint/zones; largest rewrite  
3. **Convert** — easiest; file pickers + options form  
4. **Parity** — install path, overwrite, grid build, layers  

## Phases

| Phase | Deliverable | Exit |
|-------|-------------|------|
| **0** | egui still ships; P0 fidelity/window/tooltips (done alongside) | current users unbroken |
| **1** | Cargo workspace: `crates/heightmap-core` (logic) + thin `heightmap_gui` egui + empty `apps/desktop` Tauri | `cargo test` core green |
| **2** | Tauri app: Convert-only UI calling `convert_build` | produce `.brdb` from PNG end-to-end |
| **3** | Map: basemap + bbox + predict + fetch/build/install | parity Map happy path |
| **4** | Sculpt MVP (raise/lower + export) | then stamps/paint/zones/layers |
| **5** | Cut over launcher `brickadia-world-tools-gui` → Tauri binary; archive egui bin | dual-run period ends |

## egui freeze policy

- **Bugfixes + fidelity backend** (cell budget, DEM sources, mesh): allowed on egui path  
- **New sculpt features / big UX**: prefer Tauri unless blocking  
- Do not invest in egui-only chrome beyond P0

## Tooling

- Tauri 2, Rust 1.88+, Svelte 5 + Vite  
- Linux: WebKitGTK (system) — verify on Omarchy/Hyprland early in Phase 2  
- Keep single config path `~/.config/heightmap2brz/config.toml` for both shells during dual-run  

## Risk

| Risk | Mitigation |
|------|------------|
| Sculpt rewrite stalls product | Ship Map+Convert on Tauri first; keep egui sculpt until Phase 4 |
| Wayland WebView quirks | Spike Phase 2 week 1 on this host |
| Double maintenance | Short dual-run; core tests only in Rust |

## Status (2026-08-11)

| Item | State |
|------|--------|
| `heightmap::api::convert_heightmap` | **Done** — shared by egui Convert worker + Tauri |
| Workspace | **Done** — members `.` + `apps/desktop/src-tauri` |
| Tauri + Svelte Convert UI | **Scaffolded** — path inputs → `convert_build` |
| File dialog / progress events | Next (Phase 2 polish) |
| Map / Sculpt | Not started |

### Dev (Tauri) — **Deno**, not npm

```bash
cd apps/desktop
deno task install          # nodeModulesDir auto from package.json
deno task tauri:dev        # or: deno task tauri -- dev
# Rust-only:
cargo check -p brickadia-world-tools
```

`tauri.conf.json` hooks: `beforeDevCommand` / `beforeBuildCommand` = `deno task dev|build`.

egui still: `cargo build --release --features gui` → `brickadia-world-tools-gui`.

Do **not** start a greenfield Svelte sculpt before Convert E2E is solid (dialog + progress + install).
