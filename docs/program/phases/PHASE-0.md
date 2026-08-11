# Phase 0 — egui baseline + house + fidelity P0

**Status:** Done (2026-08-11)  
**Tickets:** BWT-0.* · BWT-F0

## Goal

Ship a clean, documented egui product on `master` with honest fidelity controls and a public GitHub home — no Tauri required.

## Outcomes

- Repo https://github.com/xgp0006/brickadia-world-tools public; `origin` + `upstream`
- Feature train on `master`; layers MVP committed
- Release `heightmap_gui` fresh; launcher symlink correct
- README product-named; ROADMAP; IN_GAME_TEST; parent `legacy/`
- Window not always-on-top by default; density = Downsample; game-linked tooltips

## Exit gates (met)

- [x] `cargo test --lib` green  
- [x] `git push origin master`  
- [x] Vault [[brickadia-world-tools]] status  

## Notes

egui remains the **full** Map/Convert/Sculpt surface until Phase 5.
