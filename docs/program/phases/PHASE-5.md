# Phase 5 — Launcher cutover

**Status:** Not started  
**Tickets:** BWT-5.1–5.4  
**Depends on:** Phase 3 complete; Phase 4 MVP or explicit “Sculpt stays egui” decision

## Goal

Users launch **one** app: Tauri “Brickadia World Tools”. egui becomes optional/dev-only.

## Outcomes

1. `.desktop` + `brickadia-world-tools-gui` → Tauri binary (or new name + symlink)  
2. Dual-run window documented (egui via `cargo run --features gui`)  
3. README single path; ROADMAP marks migration complete  
4. Vault note updated  

## Cutover criteria (all required)

- [ ] Convert parity (Phase 2)  
- [ ] Map single-box parity (Phase 3)  
- [ ] Sculpt MVP **or** user accepts “Sculpt: run egui”  
- [ ] No data-loss path (config.toml shared)  
- [ ] In-game smoke by user on 2 worlds  

## Rollback

Keep `heightmap_gui` binary buildable for ≥1 release after cutover.
