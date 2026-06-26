# Changelog

All notable changes to heightmap2brz (xgp0006 fork) are recorded here. Format
follows [Keep a Changelog](https://keepachangelog.com/); the crate version lives
in `Cargo.toml`. Design specs for larger features are under
`docs/superpowers/specs/`.

## [Unreleased] — 2026-06-25

### Fixed

- **A transient tile fetch failure no longer discards the whole DEM/imagery
  fetch.** `fetch_bbox` fetches tiles sequentially; previously any single
  transport error propagated with `?` and aborted the entire multi-tile fetch
  ("partial fetches are discarded"), so one momentary blip — e.g. `EAGAIN`
  ("Resource temporarily unavailable", os error 11) at TLS init on tile r11 c10 —
  killed dozens of otherwise-good tiles. Each tile fetch is now wrapped in a
  bounded `with_retry` (3 retries, 200 ms × attempt linear backoff) that retries
  only transient `Network` (transport) errors — HTTP 4xx/5xx status codes are
  not retried. Covers both the DEM-tile and imagery paths (both go through
  `fetch_bbox`); the OpenTopography single-shot REST path is unchanged.

- **Micro bricks no longer shrink 2–5× versus normal bricks.** `derive_scale`
  now clamps the *physical cell span* (`hscale * units_per_stud`) instead of the
  raw integer brick scale. Micro bricks are 1 unit/stud and normal bricks are 5
  units/stud, so micro needs 5× the integer scale to reach the same world — its
  ceiling is therefore `MAX_HORIZONTAL_SCALE * 5` (640) versus normal's 128.
  Previously the shared 128 cap saturated micro five times too early, so a micro
  build came out 22–43 % the size of the equivalent normal build at default
  settings (the "we aren't getting microbricks" report). Normal-brick output is
  byte-identical (`640 / 5 = 128`). The Map-tab "Scale capped" readout warning
  was mirrored to the same physical-span cap so it no longer fires prematurely
  for micro. Safe now only because of the size-aware quad clamp below (a single
  micro cell is ≤ 640 units, well under the u16 `BrickSize` ceiling).

- **Greedy mesher no longer emits oversized bricks at horizontal scale > 1.**
  The per-quad merge cap is now size-aware — `max_quad = 500 / size` — so every
  merged brick stays within the 500-unit procedural cap the quadtree path already
  enforced (`quad.rs::line_optimize`). The previous fixed `1000 / brick_scale`
  cap ignored `horizontal_scale`, letting a flat, single-colour region merge into
  bricks up to `500 × hscale` units (e.g. 3500 units at scale 7) that the game
  dropped or mis-rendered — the "some outputs aren't working right" symptom,
  worst with no imagery (one colour merges maximally). At horizontal scale 1 the
  cap is unchanged (100 cells normal / 500 micro), so single-box and grid map
  builds remain byte-identical (guarded by the existing identity tests).

### Added

- **Sculpt tab: "Export heightmap PNG".** Saves the edited `HeightField` as an
  rgba-encoded heightmap PNG — each cell's floor-relative height in metres is
  scaled by 100, rounded, and packed big-endian across the RGBA channels. This is
  the exact format Stage 1 (`geotiff2heightmap/map.py`) emits and the CLI/map
  pipeline's `HeightmapPNG` decoder reads, so a sculpted field round-trips
  losslessly back through the pipeline and is a shareable heightmap. Native save
  dialog, default filename from the output-name box.

### Notes

- The 500-unit value treated as Brickadia's maximum procedural brick size is
  inherited from the quadtree path and is **not yet verified in-game**. The
  greedy change above makes the two mesh paths self-consistent regardless; if the
  real engine limit turns out to be higher, both caps can be raised together.
