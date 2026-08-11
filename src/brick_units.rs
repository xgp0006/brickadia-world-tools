//! Brickadia vertical units as used by World Tools UI + mesh emission.
//!
//! # Mesh-derived (from our code — do not “confirm” differently without reading emit)
//!
//! In `opt/generate.rs::emit_column_bricks`, non-stud bricks use vertical parity
//! **2** (`BrickSize.z`), and world Z advances by `brick_height * 2` per segment.
//! The sculpt UI therefore treats **one FLAT (plate)** as **4 heightmap units**
//! of `h = round(m · vertical_scale)`:
//!
//! ```text
//! H_UNITS_PER_FLAT = 4
//! flats = meters * vscale / H_UNITS_PER_FLAT
//! ```
//!
//! # Game convention (must confirm in Brickadia — BWT-F5)
//!
//! ```text
//! FLATS_PER_BRICK = 3   // LEGO-style; UI “1b” = 3 flats
//! ```
//!
//! This ratio is **only** for human-facing bricks+flats labels (`1b 1f`). It does
//! not change mesh emission. If in-game measurement disagrees, change
//! [`FLATS_PER_BRICK`] here and rebuild — see
//! `docs/program/fidelity/F5-FLATS-AND-IN-GAME.md`.

/// Heightmap units of `h` per one Brickadia flat/plate (mesh-derived).
pub const H_UNITS_PER_FLAT: f32 = 4.0;

/// Flats per standard brick in UI (game convention — confirm BWT-F5).
pub const FLATS_PER_BRICK: i64 = 3;

/// Convert meters of terrain height to flats at the given vertical scale
/// (`vscale` = units of `h` per meter, from `derive_scale`).
pub fn meters_to_flats(m: f32, vscale: f32) -> f32 {
    m * vscale / H_UNITS_PER_FLAT
}

/// Inverse of [`meters_to_flats`].
pub fn flats_to_meters(flats: f32, vscale: f32) -> f32 {
    if vscale > 0.0 {
        flats * H_UNITS_PER_FLAT / vscale
    } else {
        0.0
    }
}

/// Format a signed flat count as Brickadia-style `Nb Mf` (e.g. `1b 2f`, `-1b`).
pub fn fmt_bricks_flats(flats: f32) -> String {
    let signed = flats.round() as i64;
    let sign = if signed < 0 { "-" } else { "" };
    let total = signed.abs();
    let (b, f) = (total / FLATS_PER_BRICK, total % FLATS_PER_BRICK);
    match (b, f) {
        (0, f) => format!("{sign}{f}f"),
        (b, 0) => format!("{sign}{b}b"),
        (b, f) => format!("{sign}{b}b {f}f"),
    }
}

/// Parse `3b 1f` / `3b1f` / `3b` / `1f` / bare flats → flat count.
pub fn parse_bricks_flats(s: &str) -> Option<f64> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, s.as_str()),
    };
    if body.is_empty() {
        return None;
    }
    let signed = |v: f64| if neg { -v } else { v };
    if let Ok(n) = body.parse::<f64>() {
        return Some(signed(n));
    }
    let mut flats = 0.0;
    let mut num = String::new();
    let mut saw_unit = false;
    for ch in body.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            num.push(ch);
        } else if ch == 'b' || ch == 'f' {
            let v: f64 = num.trim().parse().ok()?;
            num.clear();
            flats += if ch == 'b' {
                v * FLATS_PER_BRICK as f64
            } else {
                v
            };
            saw_unit = true;
        } else if ch.is_whitespace() {
            if !num.is_empty() {
                return None;
            }
        } else {
            return None;
        }
    }
    if !num.is_empty() {
        return None;
    }
    saw_unit.then(|| signed(flats))
}

/// Meters corresponding to UI `1b` at the given vscale (for calibration fixtures).
pub fn meters_for_one_brick_ui(vscale: f32) -> f32 {
    flats_to_meters(FLATS_PER_BRICK as f32, vscale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_constants_documented() {
        assert_eq!(H_UNITS_PER_FLAT, 4.0);
        assert_eq!(FLATS_PER_BRICK, 3);
        // 1b at vscale=1 → 12 m (3 flats × 4 units/flat)
        assert!((meters_for_one_brick_ui(1.0) - 12.0).abs() < 1e-5);
        eprintln!(
            "BWT-F5 contract: 1 flat = {H_UNITS_PER_FLAT} heightmap units; \
             1 brick UI = {FLATS_PER_BRICK} flats; 1b @ vscale=1 → {} m",
            meters_for_one_brick_ui(1.0)
        );
    }

    #[test]
    fn fmt_parse_roundtrip() {
        for flats in [0.0_f32, 1.0, 3.0, 4.0, 6.0, -3.0, -5.0] {
            let s = fmt_bricks_flats(flats);
            let p = parse_bricks_flats(&s).expect(&s);
            assert!(
                (p - f64::from(flats.round())).abs() < 1e-6,
                "fmt {flats} → {s} → {p}"
            );
        }
        assert_eq!(parse_bricks_flats("1b").map(|v| v as i64), Some(3));
        assert_eq!(parse_bricks_flats("1b 1f").map(|v| v as i64), Some(4));
        assert_eq!(parse_bricks_flats("2b").map(|v| v as i64), Some(6));
    }

    #[test]
    fn meters_flats_inverse() {
        let v = 2.0_f32;
        let m = 10.0_f32;
        let f = meters_to_flats(m, v);
        assert!((flats_to_meters(f, v) - m).abs() < 1e-5);
    }
}
