# F5 + BWT-3.9 — In-game exit proof & FLATS_PER_BRICK calibration

**Tickets:** BWT-F5 (FLATS), BWT-3.9 (Phase 3 exit: Terrarium + 3DEP worlds load)  
**Status:** Procedure + fixtures ready 2026-08-11 — **needs human eyes in Brickadia**  
**Game:** Steam APPID **2199420** (Proton)

### Pre-staged for you (this host)

| World | Location | Purpose |
|-------|----------|---------|
| **`flats-cal-1b`** | `…/Saved/Worlds/flats-cal-1b.brdb` | 16×16 plateau at UI **1b** height (h=12 @ vscale 1) |
| **`exit-terrarium`** | Worlds/ (if copy succeeded) or `builds/smoke-terrarium.brdb` | Terrarium load smoke |

Regenerate flats fixture:

```bash
cargo test --lib write_flats_calibration_world -- --ignored --nocapture
```

---

## 1. Why this exists

Almost everything else can be unit-tested. Two things cannot:

1. **Brickadia silently drops** oversized bricks / bad saves → “only spawn” or holes.  
2. **`FLATS_PER_BRICK = 3`** is a **game convention** (LEGO-style), **not** derived from our mesh math. Our code uses it only for **UI labels** (sculpt height drag). Wrong value ⇒ UI lies; mesh can still be fine.

Mesh vertical math that **is** derived from our code (solid):

| Concept | Value | Source |
|---------|-------|--------|
| Min plate step (non-stud) | `BrickSize.z` parity **2** | `emit_column_bricks` |
| Z step per brick segment | `brick_height * 2` | `emit_column_bricks` |
| **1 flat** in heightmap units | **4** (`H_UNITS_PER_FLAT`) | comment + UI: one plate with z-half extent 2 → 4 units of `h` |
| **1 brick (UI)** | **3 flats** | `FLATS_PER_BRICK` — **confirm in game** |

Shared constants: `src/brick_units.rs` (tests lock the contract).

---

## 2. Paths

| Role | Path |
|------|------|
| Worlds | `~/.steam/steam/steamapps/compatdata/2199420/pfx/drive_c/users/steamuser/AppData/Local/Brickadia/Saved/Worlds/` |
| Prefabs | `…/Saved/Prefabs/` |
| Staging | `~/.local/share/heightmap2brz/builds/` |
| Tooling smoke (no game) | `builds/smoke-terrarium.brdb` (~15k bricks, 2026-08-11) |

Always **Overwrite** or open the newest `-2`/`-3` name.

---

## 3. BWT-3.9 — Phase 3 exit (worlds load)

Do **once per major release** (or after DEM pipeline changes).

### 3A — Terrarium (global free)

| Step | Action | Pass |
|------|--------|------|
| 1 | egui **or** Tauri Map: source **AWS Terrarium**, small box (~1–2 km), Downsample **1**, studs/m ~4, name `exit-terrarium` | Build succeeds |
| 2 | Install Worlds + Overwrite | File in Worlds/ |
| 3 | Launch Brickadia → load `exit-terrarium` | Terrain visible, **not** only spawn |
| 4 | Walk surface; spawn is above ground | No black void load |
| 5 | Optional: same box, studs/m **8** → world larger, brick count similar | Scale model OK |

**Record:** date · shell (egui/Tauri) · commit · pass/fail  
`_______________________________________________`

### 3B — USGS 3DEP (US only)

| Step | Action | Pass |
|------|--------|------|
| 1 | CONUS bbox (e.g. Colorado foothills), source **USGS 3DEP**, small area, Downsample 1 | Build succeeds (not “outside US” JSON) |
| 2 | Install + load `exit-3dep` | Terrain visible |
| 3 | Compare to Terrarium same box: 3DEP should look **finer** (more local detail) when budget allows | Qualitative OK |

**Record:** date · bbox (N/S/E/W) · pass/fail  
`_______________________________________________`

### 3C — Convert smoke

| Step | Action | Pass |
|------|--------|------|
| 1 | Convert `example_maps/gradient.png` → `exit-gradient.brdb` | Loads |

---

## 4. BWT-F5 — FLATS_PER_BRICK calibration (precise)

### 4.1 Goal

Confirm that when the sculpt UI says **`1b`** (one brick), the in-game **stack height** matches **three** plate/flat steps (or document the true ratio).

### 4.2 Theory (what we emit)

For **sculpt / skip_floor / fill_to_base** terrain (Map-like solid fill):

- Heightmap sample `h = round(meters * vertical_scale)`.  
- Fill height in brick units ≈ `(h - min_h) * scale / 2` with `scale` = GenOptions vertical (often 1 for sculpt).  
- Each emitted plate: `BrickSize.z = 2` (min), position steps by `2 * brick_height` in world Z.

UI conversion (sculpt):

```
flats = meters * vscale / 4
1b = 3 flats  ⇒  meters = 3 * 4 / vscale = 12 / vscale
```

At **vscale = 1** (1:1 vertical units per meter from derive_scale with exaggeration 1 and matching studs):  
**1b UI ⇒ 12 m** of field height ⇒ `h = 12` if scale maps 1:1.

Easier path: **don't fight meters** — use a **fixed heightmap** and count flats from **h**.

### 4.3 Fixture method (recommended)

#### Tooling (pre-flight, no game)

```bash
cd ~/Projects/brickadia/heightmap2brz
cargo test --lib brick_units -- --nocapture
# prints contract + example conversions
```

Optional: generate a calibration world (when helper exists):

```bash
# After release build:
# Creates builds/flats-cal-1b.brdb — a 16×16 plateau of height h corresponding to UI "1b" at vscale=1
cargo test --lib flats_calibration_world -- --ignored --nocapture
```

#### In-game procedure

1. **Build** `flats-cal-1b` (or sculpt blank 32×32, **Set height** to exactly `1b` with vscale readout ≈1, fill a pad, export SmoothTile, install as `flats-cal-1b`).  
2. Load world in Brickadia.  
3. Open **build/mode tools** that show brick/plate dimensions (or place a known 1-brick reference from palette next to the pad).  
4. Measure plateau stack:
   - If UI shows height in **bricks+flats**, read it.  
   - Else: count **plate layers** visually vs a `1b` palette brick.  
5. **Pass criteria:**
   - Stack matches **1 brick** (or **3 flats**) within one plate of rounding.  
6. Repeat with UI set to **`1b 1f`** (4 flats) — stack should be one plate taller than `1b`.

#### Record table

| UI input | Expected (if FLATS=3) | Observed in-game | Match? |
|----------|----------------------|------------------|--------|
| `1f` | 1 plate | | ☐ |
| `1b` | 3 plates / 1 brick | | ☐ |
| `1b 1f` | 4 plates | | ☐ |
| `2b` | 6 plates | | ☐ |

**Commit / date / shell:** `_________________________________`  
**Verdict:** ☐ **FLATS_PER_BRICK=3 confirmed** · ☐ **Different: ____** (then change `src/brick_units.rs` + sculpt UI + retest)

### 4.4 If the ratio is wrong

1. Update `FLATS_PER_BRICK` in `src/brick_units.rs` only (single source of truth).  
2. Rebuild release GUI.  
3. Re-run §4.3 table.  
4. Note in vault [[brickadia-world-tools]] log with **REFUTED** old ratio.

---

## 5. Full smoke matrix (optional expansion)

| # | Area | Check |
|---|------|--------|
| S1 | Map Terrarium | §3A |
| S2 | Map 3DEP | §3B |
| S3 | Convert gradient | §3C |
| S4 | FLATS | §4 |
| S5 | Sculpt raise/export | Visible hill |
| S6 | Zone omit | Hole |
| S7 | Grid 2×2 | No seam holes |
| S8 | Max brick size | No plain holes |

---

## 6. Ticket close rules

| Ticket | Close when |
|--------|------------|
| **BWT-3.9** | §3A + §3B both pass (3B skip only if no US interest; note “N/A”) |
| **BWT-F5** | §4.3 table filled; ratio confirmed or code updated |

Agent cannot close these without your observations. Tooling can only prepare fixtures and document contracts.

---

## 7. Related

- `docs/IN_GAME_TEST.md` — short checklist (points here for FLATS detail)  
- `src/brick_units.rs` — constants + tests  
- Memory: `local/project_brickadia_world_tools.md`  
- Vault: [[brickadia-world-tools]]
