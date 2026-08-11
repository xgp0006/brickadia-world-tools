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
```

Core convert API: `heightmap::api::convert_heightmap` (shared with egui).
