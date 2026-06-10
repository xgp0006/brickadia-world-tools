//! Coordinate input parsing for the Map tab. Decimal `lat, lon` only — DMS
//! and MGRS inputs are recognized and rejected with a clear error until real
//! parsers exist.

const MAX_INPUT_LEN: usize = 128;
const LAT_MIN: f64 = -90.0;
const LAT_MAX: f64 = 90.0;
const LON_MIN: f64 = -180.0;
const LON_MAX: f64 = 180.0;

#[derive(Debug)]
pub(crate) enum CoordParseError {
    Empty,
    TooLong(usize),
    NoSeparator,
    InvalidNumber(String),
    LatOutOfRange(f64),
    LonOutOfRange(f64),
    UnsupportedFormat(&'static str),
}

impl std::fmt::Display for CoordParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "input is empty"),
            Self::TooLong(n) => write!(f, "input is too long ({n} chars, max {MAX_INPUT_LEN})"),
            Self::NoSeparator => write!(f, "expected `lat, lon` separated by comma or space"),
            Self::InvalidNumber(token) => write!(f, "could not parse number: {token:?}"),
            Self::LatOutOfRange(v) => write!(f, "latitude {v} out of range [-90, 90]"),
            Self::LonOutOfRange(v) => write!(f, "longitude {v} out of range [-180, 180]"),
            Self::UnsupportedFormat(name) => {
                write!(f, "{name} parsing is not yet implemented — use decimal lat, lon")
            }
        }
    }
}

impl std::error::Error for CoordParseError {}

/// Parse a string like `"40.5234, -105.182"` or `"40.5234 -105.182"` into `(lat, lon)`.
///
/// Returns ranges-validated coordinates. DMS and MGRS forms are recognized
/// but error with `UnsupportedFormat`.
pub(crate) fn parse_lat_lon(input: &str) -> Result<(f64, f64), CoordParseError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(CoordParseError::Empty);
    }
    if trimmed.len() > MAX_INPUT_LEN {
        return Err(CoordParseError::TooLong(trimmed.len()));
    }

    if contains_dms_markers(trimmed) {
        return Err(CoordParseError::UnsupportedFormat("DMS"));
    }
    if looks_like_mgrs(trimmed) {
        return Err(CoordParseError::UnsupportedFormat("MGRS"));
    }

    let (lat_str, lon_str) = split_pair(trimmed)?;
    let lat: f64 = lat_str
        .parse()
        .map_err(|_| CoordParseError::InvalidNumber(lat_str.to_owned()))?;
    let lon: f64 = lon_str
        .parse()
        .map_err(|_| CoordParseError::InvalidNumber(lon_str.to_owned()))?;

    if !(LAT_MIN..=LAT_MAX).contains(&lat) {
        return Err(CoordParseError::LatOutOfRange(lat));
    }
    if !(LON_MIN..=LON_MAX).contains(&lon) {
        return Err(CoordParseError::LonOutOfRange(lon));
    }

    Ok((lat, lon))
}

fn split_pair(input: &str) -> Result<(&str, &str), CoordParseError> {
    if let Some((a, b)) = input.split_once(',') {
        let a = a.trim();
        let b = b.trim();
        if a.is_empty() || b.is_empty() {
            return Err(CoordParseError::NoSeparator);
        }
        return Ok((a, b));
    }
    let mut parts = input.split_ascii_whitespace();
    let a = parts.next().ok_or(CoordParseError::NoSeparator)?;
    let b = parts.next().ok_or(CoordParseError::NoSeparator)?;
    if parts.next().is_some() {
        return Err(CoordParseError::NoSeparator);
    }
    Ok((a, b))
}

fn contains_dms_markers(input: &str) -> bool {
    input.chars().any(|c| matches!(c, '°' | '\'' | '"' | '′' | '″'))
}

fn looks_like_mgrs(input: &str) -> bool {
    let compact: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.len() < 6 || compact.len() > 15 {
        return false;
    }
    let bytes = compact.as_bytes();
    let zone_two = bytes[0].is_ascii_digit() && bytes[1].is_ascii_digit() && bytes[2].is_ascii_alphabetic();
    let zone_one = bytes[0].is_ascii_digit() && bytes[1].is_ascii_alphabetic();
    zone_two || zone_one
}

pub(crate) fn format_lat_lon(lat: f64, lon: f64) -> String {
    format!("{lat:.5}, {lon:.5}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comma_form() {
        let (lat, lon) = parse_lat_lon("40.5234, -105.182").expect("valid comma form");
        assert!((lat - 40.5234).abs() < 1e-9, "lat={lat}");
        assert!((lon + 105.182).abs() < 1e-9, "lon={lon}");
    }

    #[test]
    fn parses_whitespace_form() {
        let (lat, lon) = parse_lat_lon("40.5234 -105.182").expect("valid whitespace form");
        assert!((lat - 40.5234).abs() < 1e-9 && (lon + 105.182).abs() < 1e-9);
    }

    #[test]
    fn rejects_lat_out_of_range() {
        assert!(matches!(parse_lat_lon("91.0, 0.0"), Err(CoordParseError::LatOutOfRange(_))));
    }

    #[test]
    fn rejects_lon_out_of_range() {
        assert!(matches!(parse_lat_lon("0.0, 181.0"), Err(CoordParseError::LonOutOfRange(_))));
    }

    #[test]
    fn rejects_three_tokens() {
        assert!(matches!(parse_lat_lon("1 2 3"), Err(CoordParseError::NoSeparator)));
    }

    #[test]
    fn rejects_empty() {
        assert!(matches!(parse_lat_lon("   "), Err(CoordParseError::Empty)));
    }

    #[test]
    fn rejects_nonnumeric() {
        assert!(matches!(parse_lat_lon("abc, def"), Err(CoordParseError::InvalidNumber(_))));
    }

    #[test]
    fn dms_dispatches_to_unsupported() {
        let r = parse_lat_lon("40°31'24\"N 105°10'55\"W");
        assert!(matches!(r, Err(CoordParseError::UnsupportedFormat("DMS"))), "got {r:?}");
    }

    #[test]
    fn mgrs_dispatches_to_unsupported() {
        let r = parse_lat_lon("13TDE 73523 88974");
        assert!(matches!(r, Err(CoordParseError::UnsupportedFormat("MGRS"))), "got {r:?}");
    }

    #[test]
    fn format_then_parse_round_trips() {
        let s = format_lat_lon(40.5417, -105.1556);
        let (lat, lon) = parse_lat_lon(&s).expect("formatted output must re-parse");
        assert!((lat - 40.5417).abs() < 1e-4 && (lon + 105.1556).abs() < 1e-4);
    }

    #[test]
    fn rejects_too_long_input() {
        let long = "1".repeat(MAX_INPUT_LEN + 1);
        assert!(matches!(parse_lat_lon(&long), Err(CoordParseError::TooLong(_))));
    }
}
