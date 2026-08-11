# F4 — Upstream (Meshiest) selective harvest

**Ticket:** BWT-F4  
**Status:** Notes 2026-08-11 — no full merge

## Worth taking

| Item | Action |
|------|--------|
| `--hdmap` RGB heightmaps | **Already in Convert** (`opt_hdmap` / HD Map). Docs: Stage-1 geotiff2heightmap + sculpt PNG export use packed RGBA. |
| `--wedge` / terraced wedge | Not in our tree. Optional future surface style behind flag — product decision. |
| Theme sandbox | Optional polish; not fidelity. |

## Do not take

- Audio / MIDI / video / text pipelines
- Blind merge of upstream `master` (fights sculpt/grid stack)

## Exit when

- [x] HD Map documented in Convert tooltips (done)
- [ ] Optional: wedge spike PRD if user wants alternate surface (not default)

## Next

Only implement wedge if product asks; F3 mesher is higher ROI for fidelity.
