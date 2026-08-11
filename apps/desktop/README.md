# Brickadia World Tools — Tauri + Svelte (primary UI)

**Package manager: Deno** (`deno task …`). This app is the **default product**.

## Setup

```bash
cd apps/desktop
deno task install
```

## Dev

```bash
deno task tauri:dev
```

## Release build

```bash
deno task tauri:build
# → ../../target/release/brickadia-world-tools
# AppImage step may fail (linuxdeploy); the binary is still fine.
```

Daily launch (after build; symlinks normally already set):

```bash
brickadia-world-tools
```

Routes: `/` Convert · `/map` Map+Grid · `/sculpt` Sculpt.

**Legacy egui (deprecated):** `brickadia-world-tools-legacy-egui`

Program: `docs/program/PROGRAM.md` · Phase 5 cutover complete.
