# Brickadia World Tools

**User-facing name:** Brickadia World Tools  
**Crate / binaries (internal):** `heightmap` · `heightmap_gui` · `heightmap` (CLI)

Fork of [Meshiest/heightmap2brz](https://github.com/Meshiest/heightmap2brz), heavily extended for real-world DEM → Brickadia terrain.

| | |
|--|--|
| **Repo** | https://github.com/xgp0006/brickadia-world-tools |
| **Upstream** | https://github.com/Meshiest/heightmap2brz (`upstream` remote) |
| **Host path** | `~/Projects/brickadia/heightmap2brz` (desktop) |

## What it does

- **Map tab** — fetch DEM (AWS Terrarium, Mapbox Terrain-RGB, OpenTopography SRTMGL1), imagery, bbox, brick type, density, scale, generate `.brdb`/`.brz`, install into Brickadia’s Proton prefix (APPID `2199420`).
- **Convert tab** — heightmap PNG (+ optional colormap) → save.
- **Sculpt tab** — brushes, stamps, splat paint, terrace, freedraw omit/include zones, free-angle rotation (baked into export), **export layers** (box-selected multi-save), heightmap PNG export.

## Launch

**Classic (full Map/Convert/Sculpt/Grid — default app-grid entry):**

```bash
brickadia-world-tools-gui    # preferred symlink → target/release/heightmap_gui
# or
heightmap2brz-gui
cargo build --release --features gui   # after code changes (launcher uses release)
```

**Tauri shell (Convert + Map + Grid + Sculpt MVP):**

```bash
brickadia-world-tools-tauri            # ~/.local/bin wrapper
# or
cd apps/desktop && deno task tauri:dev
# release:
cd apps/desktop && deno task tauri:build
```

Program / tickets: [docs/program/PROGRAM.md](./docs/program/PROGRAM.md).

CLI Stage-2 only:

```bash
cargo build --release --bin heightmap   # if bin exists without gui feature
heightmap2brz --help
```

Legacy full GeoTIFF pipeline (parent tree): `brickadia-map` → `../legacy/geotiff2heightmap/`.

## Develop

```bash
cargo test --lib
cargo clippy --all-targets --all-features -- -D warnings   # preferred gate
```

Requires Rust **1.88+**, edition **2024**.

Config / API keys: `~/.config/heightmap2brz/config.toml` (mode 600).

## Docs

| Path | |
|------|--|
| [ROADMAP.md](./ROADMAP.md) | Status + next work |
| [docs/IN_GAME_TEST.md](./docs/IN_GAME_TEST.md) | Manual Brickadia checklist |
| [docs/superpowers/specs/](./docs/superpowers/specs/) | Feature design specs |
| [CHANGELOG.md](./CHANGELOG.md) | Notable changes |

Vault (desktop): `brickadia-world-tools` project · `brickadia-heightmap-toolkit` reference.

## License / attribution

Upstream authorship retained in history (`Meshiest` / heightmap2brz). See upstream README for original license terms; keep attribution when redistributing.
