# Phase 5 — Launcher cutover (COMPLETE)

**Status:** **Done 2026-08-11** — Tauri is the product binary.  
**Tickets:** BWT-5.1–5.4

## Goal

Users launch **one** app: Tauri **Brickadia World Tools**. egui is deprecated / opt-in only.

## Shipped

| Item | Detail |
|------|--------|
| Release binary | `target/release/brickadia-world-tools` (~28 MB) |
| Primary launcher | `~/.local/bin/brickadia-world-tools` |
| Aliases | `brickadia-world-tools-gui`, `heightmap2brz-gui` → Tauri |
| Desktop | `brickadia-world-tools.desktop` + `heightmap2brz.desktop` → Tauri |
| Legacy egui | `brickadia-world-tools-legacy-egui` · desktop **NoDisplay** |
| Docs | README primary = Tauri |

## Rebuild

```bash
cd apps/desktop && deno task tauri:build
# AppImage may fail on linuxdeploy — binary still valid at target/release/
```

## Rollback

```bash
cargo build --release --features gui --bin heightmap_gui
brickadia-world-tools-legacy-egui
```

## Exit criteria (met)

- [x] Convert / Map / Sculpt on Tauri  
- [x] Default launchers → Tauri  
- [x] egui not default  
- [x] Config path unchanged (`~/.config/heightmap2brz/`)  
