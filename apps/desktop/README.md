# Brickadia World Tools — desktop shell (Tauri + Svelte)

**Package manager: Deno** (`deno task …`). `package.json` only lists deps for Deno’s `nodeModulesDir`.

## Setup

```bash
cd apps/desktop
deno task install
```

## Run

```bash
deno task tauri:dev      # frontend + Rust
deno task dev            # Vite only (port 1420)
cargo check -p brickadia-world-tools   # from repo root
```

## Build

```bash
deno task tauri:build
# binary under apps/desktop/src-tauri/target/release/ (see tauri product name)
```

## Dual-run with egui

| | |
|--|--|
| **egui (full)** | `brickadia-world-tools-gui` — default app-grid entry |
| **Tauri** | `deno task tauri:dev` — Convert `/` · Map `/map` · Sculpt `/sculpt` |

Program phases: `docs/program/PROGRAM.md`. Core APIs: `heightmap::api::{convert, install, dem_predict, dem_fetch_build, sculpt}`.