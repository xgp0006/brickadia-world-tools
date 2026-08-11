# Brickadia World Tools

**Product:** DEM / heightmap → Brickadia `.brdb` / `.brz`  
**Shell:** **Tauri 2 + Svelte 5 + Deno** (primary)  
**Crate:** `heightmap` (Rust core; egui GUI **deprecated**)

| | |
|--|--|
| **Repo** | https://github.com/xgp0006/brickadia-world-tools |
| **Upstream** | https://github.com/Meshiest/heightmap2brz |
| **Host path** | `~/Projects/brickadia/heightmap2brz` (desktop) |

## Launch (primary)

```bash
brickadia-world-tools          # preferred
# aliases (same binary):
brickadia-world-tools-gui
heightmap2brz-gui
```

App menu: **Brickadia World Tools**.

Tabs: **Convert** · **Map** (DEM + Grid) · **Sculpt**.

### Rebuild after code changes

```bash
cd apps/desktop
deno task install              # once / after package.json changes
deno task tauri:build          # → target/release/brickadia-world-tools
# re-link if binary path unchanged (symlink already points at release)
```

Dev (hot reload UI):

```bash
cd apps/desktop && deno task tauri:dev
```

## Deprecated: egui shell

The old immediate-mode GUI (`heightmap_gui`) is **deprecated**. Prefer Tauri.

```bash
# only if you need the legacy binary:
brickadia-world-tools-legacy-egui
cargo build --release --features gui --bin heightmap_gui
```

Desktop entry for legacy is **hidden** (`NoDisplay=true`).

## CLI (Stage-2 only, still supported)

```bash
cargo build --release --bin heightmap
heightmap2brz --help           # symlink → target/release/heightmap
```

Legacy full GeoTIFF pipeline: `brickadia-map` → `../legacy/geotiff2heightmap/`.

## Develop

```bash
cargo test --lib
cargo test --lib --no-default-features --features dem
cd apps/desktop && deno task build
```

Config / API keys: `~/.config/heightmap2brz/config.toml` (mode 600).

## Docs

| Path | |
|------|--|
| [docs/program/PROGRAM.md](./docs/program/PROGRAM.md) | Phase board + tickets |
| [ROADMAP.md](./ROADMAP.md) | Status |
| [docs/IN_GAME_TEST.md](./docs/IN_GAME_TEST.md) | Brickadia QA |
| [docs/program/fidelity/](./docs/program/fidelity/) | F3 mesher · F5 flats |

## License / attribution

Upstream authorship retained (`Meshiest` / heightmap2brz). Keep attribution when redistributing.
