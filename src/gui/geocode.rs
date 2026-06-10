//! Free-text place search via the Mapbox Search Box forward API.
//!
//! Turns a query like "Mount Fuji", "Grand Canyon", or "1600 Pennsylvania Ave"
//! into a single lat/lon hit so the map can recenter. Uses the same Mapbox
//! token already required for the Mapbox DEM / imagery tile sources.
//!
//! Endpoint (verified against <https://docs.mapbox.com/api/search/search-box/>):
//! `GET https://api.mapbox.com/search/searchbox/v1/forward?q=…&access_token=…`
//! Response is a GeoJSON `FeatureCollection`; each feature carries
//! `geometry.coordinates = [lon, lat]` and a human label under
//! `properties.full_address` / `properties.name`. The Search Box variant (vs.
//! Geocoding v6) is what indexes POIs and natural features, so it resolves
//! landmarks the address-only geocoder cannot.

use std::time::Duration;

use super::USER_AGENT;
use super::tiles::redact_secrets;

const SEARCHBOX_FORWARD_URL: &str = "https://api.mapbox.com/search/searchbox/v1/forward";
const GEOCODE_TIMEOUT: Duration = Duration::from_secs(15);

/// A single resolved place: where to recenter, plus a label to show the user.
#[derive(Debug, Clone)]
pub(crate) struct GeocodeHit {
    pub(crate) lat: f64,
    pub(crate) lon: f64,
    pub(crate) label: String,
}

#[derive(Debug)]
pub(crate) enum GeocodeError {
    /// Query was empty/whitespace — caller should not have dispatched.
    EmptyQuery,
    /// No Mapbox token configured; search needs one (set it in Settings).
    MissingToken,
    /// HTTP status >= 400 from the API. Token is never included.
    Http(u16),
    /// Transport failure (DNS, TLS, timeout, connection reset).
    Network(String),
    /// 200 OK but the body did not parse as the expected GeoJSON shape.
    Decode(String),
    /// Valid response with zero features — nothing matched the query.
    NoResults(String),
}

impl std::fmt::Display for GeocodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyQuery => write!(f, "enter a place to search"),
            Self::MissingToken => {
                write!(f, "set a Mapbox token in Settings to search")
            }
            Self::Http(code) => write!(f, "search failed (HTTP {code})"),
            Self::Network(e) => write!(f, "network error: {e}"),
            Self::Decode(e) => write!(f, "could not read search results: {e}"),
            Self::NoResults(q) => write!(f, "no place found for \"{q}\""),
        }
    }
}

impl std::error::Error for GeocodeError {}

/// Forward-geocode `query` to its best single match, biased toward
/// `proximity` (the map's current target) when supplied.
///
/// Blocking — run on a worker thread. The token is passed via ureq's `.query`,
/// which percent-encodes it; it is never interpolated into a string that could
/// reach a log line or error.
pub(crate) fn forward(
    query: &str,
    token: &str,
    proximity: Option<(f64, f64)>,
) -> Result<GeocodeHit, GeocodeError> {
    let q = query.trim();
    if q.is_empty() {
        return Err(GeocodeError::EmptyQuery);
    }
    if token.trim().is_empty() {
        return Err(GeocodeError::MissingToken);
    }

    let agent = ureq::AgentBuilder::new()
        .timeout(GEOCODE_TIMEOUT)
        .user_agent(USER_AGENT)
        .build();

    let mut req = agent
        .get(SEARCHBOX_FORWARD_URL)
        .query("q", q)
        .query("limit", "1")
        .query("access_token", token);
    // Bias to where the user is already looking so "Mount Fuji" resolves the
    // nearby feature rather than a same-named POI on the far side of the globe.
    // Search Box expects `proximity=lon,lat`.
    let prox_value;
    if let Some((lat, lon)) = proximity {
        prox_value = format!("{lon},{lat}");
        req = req.query("proximity", &prox_value);
    }

    let response = match req.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(code, _)) => return Err(GeocodeError::Http(code)),
        // ureq's Transport Display embeds the request URL (with access_token) —
        // redact before the message can reach a log line or the GUI.
        Err(e) => return Err(GeocodeError::Network(redact_secrets(&e.to_string()))),
    };
    // Residual guard for a non-200 success status (e.g. a 3xx that survived
    // ureq's redirect handling) — without it a stray body becomes a confusing
    // Decode error downstream.
    if response.status() != 200 {
        return Err(GeocodeError::Http(response.status()));
    }

    let body = response
        .into_string()
        .map_err(|e| GeocodeError::Network(e.to_string()))?;
    parse_first_hit(&body, q)
}

/// Parse a Search Box `FeatureCollection` body and extract the first feature
/// as a [`GeocodeHit`]. Split out so it is unit-testable without the network.
fn parse_first_hit(body: &str, query: &str) -> Result<GeocodeHit, GeocodeError> {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|e| GeocodeError::Decode(e.to_string()))?;

    let feature = json
        .get("features")
        .and_then(|f| f.as_array())
        .and_then(|arr| arr.first());
    let Some(feature) = feature else {
        return Err(GeocodeError::NoResults(query.to_owned()));
    };

    let coords = feature
        .get("geometry")
        .and_then(|g| g.get("coordinates"))
        .and_then(|c| c.as_array())
        .ok_or_else(|| GeocodeError::Decode("feature has no geometry.coordinates".to_owned()))?;
    // GeoJSON order is [lon, lat].
    let lon = coords
        .first()
        .and_then(serde_json::Value::as_f64)
        .ok_or_else(|| GeocodeError::Decode("coordinates[0] (lon) missing".to_owned()))?;
    let lat = coords
        .get(1)
        .and_then(serde_json::Value::as_f64)
        .ok_or_else(|| GeocodeError::Decode("coordinates[1] (lat) missing".to_owned()))?;

    let props = feature.get("properties");
    let label = props
        .and_then(|p| p.get("full_address"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            props
                .and_then(|p| p.get("place_formatted"))
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| props.and_then(|p| p.get("name")).and_then(serde_json::Value::as_str))
        .unwrap_or(query)
        .to_owned();

    Ok(GeocodeHit { lat, lon, label })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_poi_feature() {
        // Trimmed real Search Box forward response shape (Eiffel Tower).
        let body = r#"{
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": { "type": "Point", "coordinates": [2.294519, 48.858122] },
                "properties": {
                    "name": "Eiffel Tower",
                    "feature_type": "poi",
                    "full_address": "75007 Paris, France"
                }
            }]
        }"#;
        let hit = parse_first_hit(body, "Eiffel Tower").expect("should parse");
        assert!((hit.lat - 48.858122).abs() < 1e-6, "lat from coords[1]");
        assert!((hit.lon - 2.294519).abs() < 1e-6, "lon from coords[0]");
        assert_eq!(hit.label, "75007 Paris, France");
    }

    #[test]
    fn falls_back_to_name_when_no_address() {
        let body = r#"{"type":"FeatureCollection","features":[{
            "geometry":{"coordinates":[-112.142028,36.054098]},
            "properties":{"name":"Grand Canyon","feature_type":"place"}
        }]}"#;
        let hit = parse_first_hit(body, "Grand Canyon").expect("should parse");
        assert_eq!(hit.label, "Grand Canyon");
        assert!((hit.lon - -112.142028).abs() < 1e-6);
    }

    #[test]
    fn empty_features_is_no_results() {
        let body = r#"{"type":"FeatureCollection","features":[]}"#;
        let err = parse_first_hit(body, "asdfqwerzxcv").expect_err("should be NoResults");
        assert!(matches!(err, GeocodeError::NoResults(_)));
    }

    #[test]
    fn malformed_body_is_decode_error() {
        let err = parse_first_hit("not json", "x").expect_err("should be Decode");
        assert!(matches!(err, GeocodeError::Decode(_)));
    }

    #[test]
    fn empty_query_rejected_before_network() {
        let err = forward("   ", "pk.tok", None).expect_err("empty query");
        assert!(matches!(err, GeocodeError::EmptyQuery));
    }

    #[test]
    fn missing_token_rejected_before_network() {
        let err = forward("Tokyo", "  ", None).expect_err("missing token");
        assert!(matches!(err, GeocodeError::MissingToken));
    }
}
