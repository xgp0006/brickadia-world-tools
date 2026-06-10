//! Slippy-map tile math + HTTP fetcher + stitcher.
//!
//! Shared across every tile-based source. Per-source code (`dem_sources`,
//! `imagery_sources`) supplies a `TileSource::url(coord) -> String` and the
//! pixel decode for the result; this module owns the geometry, the HTTP
//! pump, and the composite-and-crop.

use image::{ImageBuffer, Rgba, RgbaImage};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_TILES_PER_FETCH: usize = 64;
pub(crate) const TILE_SIZE_PX: u32 = 256;
const TARGET_TILE_COUNT_LOW: usize = 4;
const TARGET_TILE_COUNT_HIGH: usize = 16;
const MIN_ZOOM: u32 = 1;
const MAX_ZOOM: u32 = 18;
/// Web Mercator latitude limit — the slippy-map tile grid is undefined beyond it.
pub(crate) const MERCATOR_LAT_LIMIT: f64 = 85.0511;

use super::USER_AGENT;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TileCoord {
    pub(crate) z: u32,
    pub(crate) x: u32,
    pub(crate) y: u32,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct BBoxLatLon {
    pub(crate) north: f64,
    pub(crate) south: f64,
    pub(crate) east: f64,
    pub(crate) west: f64,
}

#[derive(Debug)]
pub(crate) enum TileFetchError {
    BboxTooLarge { tiles: usize, max: usize, zoom: u32 },
    Http { url: String, status: u16 },
    Network { url: String, source: String },
    Decode { url: String, source: String },
    Cancelled,
}

impl std::fmt::Display for TileFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BboxTooLarge { tiles, max, zoom } => write!(
                f,
                "bbox covers {tiles} tiles at zoom {zoom} (max {max} per request) — \
                 zoom out, shrink the bbox, or pick a lower-resolution source"
            ),
            Self::Http { url, status } => write!(f, "HTTP {status} fetching {url}"),
            Self::Network { url, source } => write!(f, "network error fetching {url}: {source}"),
            Self::Decode { url, source } => {
                write!(f, "could not decode tile from {url}: {source}")
            }
            Self::Cancelled => write!(f, "cancelled by user"),
        }
    }
}

impl std::error::Error for TileFetchError {}

pub(crate) trait TileSource: Send + Sync {
    fn url(&self, coord: TileCoord) -> String;
    /// Highest zoom at which this provider serves real 256px tile data.
    /// Requesting above it yields 404s (DEM tilesets) or upscaled, detail-free
    /// tiles (imagery). Default is the historical global ceiling; each provider
    /// overrides with its documented cap so `pick_zoom` never overshoots.
    fn max_zoom(&self) -> u32 {
        MAX_ZOOM
    }
}

/// A composite image stitched from N×M tiles plus the crop region within it
/// that corresponds to the requested bbox.
pub(crate) struct StitchedTiles {
    pub(crate) image: RgbaImage,
    pub(crate) zoom: u32,
    pub(crate) nw_tile: TileCoord,
    pub(crate) crop_x0: u32,
    pub(crate) crop_y0: u32,
    pub(crate) crop_x1: u32,
    pub(crate) crop_y1: u32,
}

impl StitchedTiles {
    pub(crate) fn cropped(&self) -> RgbaImage {
        debug_assert!(
            self.crop_x1 > self.crop_x0 && self.crop_y1 > self.crop_y0,
            "StitchedTiles::cropped precondition: crop window must have positive size",
        );
        let w = self.crop_x1 - self.crop_x0;
        let h = self.crop_y1 - self.crop_y0;
        image::imageops::crop_imm(&self.image, self.crop_x0, self.crop_y0, w, h).to_image()
    }
}

/// Pick the smallest zoom level that puts the bbox into the
/// `[TARGET_TILE_COUNT_LOW, TARGET_TILE_COUNT_HIGH]` window, never exceeding
/// `source_max_zoom` (the provider's documented data ceiling). A tiny bbox that
/// under-fills the target at every zoom clamps to that ceiling rather than the
/// global `MAX_ZOOM`, so we never request tiles the provider does not serve.
pub(crate) fn pick_zoom(bbox: BBoxLatLon, source_max_zoom: u32) -> u32 {
    debug_assert!(
        bbox.north > bbox.south && bbox.east >= bbox.west,
        "pick_zoom: bbox must be canonical (north>south, east>=west); got {bbox:?}",
    );
    // A bogus override can never widen the search past the global ceiling.
    let ceiling = source_max_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
    for z in MIN_ZOOM..=ceiling {
        let n = tile_count(bbox, z);
        if (TARGET_TILE_COUNT_LOW..=TARGET_TILE_COUNT_HIGH).contains(&n) {
            return z;
        }
        if n > TARGET_TILE_COUNT_HIGH {
            return z.saturating_sub(1).max(MIN_ZOOM);
        }
    }
    ceiling
}

pub(crate) fn tile_count(bbox: BBoxLatLon, zoom: u32) -> usize {
    let (nw, se) = tile_range(bbox, zoom);
    let dx = (se.x - nw.x + 1) as usize;
    let dy = (se.y - nw.y + 1) as usize;
    dx * dy
}

/// Inclusive NW and SE tile coordinates that cover the bbox at the given zoom.
pub(crate) fn tile_range(bbox: BBoxLatLon, zoom: u32) -> (TileCoord, TileCoord) {
    let (nw_x, nw_y) = lat_lon_to_tile(bbox.north, bbox.west, zoom);
    let (se_x, se_y) = lat_lon_to_tile(bbox.south, bbox.east, zoom);
    (
        TileCoord { z: zoom, x: nw_x, y: nw_y },
        TileCoord { z: zoom, x: se_x, y: se_y },
    )
}

/// Standard slippy-map projection: lat/lon → tile-grid x/y at the given zoom.
fn lat_lon_to_tile(lat: f64, lon: f64, zoom: u32) -> (u32, u32) {
    let n = 2.0_f64.powi(zoom as i32);
    let x = ((lon + 180.0) / 360.0 * n).floor() as i64;
    let lat_clamped = lat.clamp(-MERCATOR_LAT_LIMIT, MERCATOR_LAT_LIMIT);
    let lat_rad = lat_clamped.to_radians();
    let y = ((1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0 * n)
        .floor() as i64;
    let max = (n as i64).saturating_sub(1).max(0);
    (x.clamp(0, max) as u32, y.clamp(0, max) as u32)
}

/// Sub-pixel world coordinate of a lat/lon at the given zoom.
/// World canvas at zoom z is `TILE_SIZE_PX * 2^z` pixels per side.
fn lat_lon_to_world_px(lat: f64, lon: f64, zoom: u32) -> (f64, f64) {
    let n = 2.0_f64.powi(zoom as i32);
    let total = TILE_SIZE_PX as f64 * n;
    let lat_clamped = lat.clamp(-MERCATOR_LAT_LIMIT, MERCATOR_LAT_LIMIT);
    let lat_rad = lat_clamped.to_radians();
    let x = (lon + 180.0) / 360.0 * total;
    let y = (1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0 * total;
    (x, y)
}

/// Fetch every tile covering `bbox` from `source`, composite into a single
/// image, and compute the sub-pixel crop window for the bbox itself.
///
/// Progress callback receives values in [0.0, 1.0]. Cancellation is checked
/// between tiles — partial fetches are discarded.
pub(crate) fn fetch_bbox(
    bbox: BBoxLatLon,
    source: &dyn TileSource,
    progress: &(dyn Fn(f32) + Send + Sync),
    cancel: &AtomicBool,
) -> Result<StitchedTiles, TileFetchError> {
    debug_assert!(
        bbox.north > bbox.south && bbox.east >= bbox.west,
        "fetch_bbox precondition: bbox must be canonical; got {bbox:?}",
    );
    let zoom = pick_zoom(bbox, source.max_zoom());
    let (nw, se) = tile_range(bbox, zoom);
    let count = tile_count(bbox, zoom);
    if count > MAX_TILES_PER_FETCH {
        return Err(TileFetchError::BboxTooLarge {
            tiles: count,
            max: MAX_TILES_PER_FETCH,
            zoom,
        });
    }

    let grid_w = se.x - nw.x + 1;
    let grid_h = se.y - nw.y + 1;
    let canvas_w = grid_w * TILE_SIZE_PX;
    let canvas_h = grid_h * TILE_SIZE_PX;
    let mut canvas: RgbaImage = ImageBuffer::new(canvas_w, canvas_h);
    let agent = ureq::AgentBuilder::new()
        .timeout(HTTP_TIMEOUT)
        .user_agent(USER_AGENT)
        .build();

    let mut fetched = 0usize;
    for gy in 0..grid_h {
        for gx in 0..grid_w {
            if cancel.load(Ordering::Relaxed) {
                return Err(TileFetchError::Cancelled);
            }
            let coord = TileCoord { z: zoom, x: nw.x + gx, y: nw.y + gy };
            let tile = fetch_one_tile(&agent, source, coord)?;
            paste_tile(&mut canvas, &tile, gx * TILE_SIZE_PX, gy * TILE_SIZE_PX);
            fetched += 1;
            progress(fetched as f32 / count as f32);
        }
    }

    let crop = crop_window(bbox, zoom, nw, canvas_w, canvas_h);
    Ok(StitchedTiles {
        image: canvas,
        zoom,
        nw_tile: nw,
        crop_x0: crop.0,
        crop_y0: crop.1,
        crop_x1: crop.2,
        crop_y1: crop.3,
    })
}

fn fetch_one_tile(
    agent: &ureq::Agent,
    source: &dyn TileSource,
    coord: TileCoord,
) -> Result<RgbaImage, TileFetchError> {
    let url = source.url(coord);
    // `safe` is the token-redacted URL stored in any error so a secret never
    // reaches a log line or the GUI status bar (see redact_url). The raw `url`
    // is used only as the request target.
    let safe = redact_url(&url);
    // ureq 2.x returns Err(Error::Status) for any HTTP status >= 400, so the
    // >=400 cases never reach the post-call status check — route them to the
    // Http variant here with the real code. Transport failures become Network.
    let response = match agent.get(&url).call() {
        Ok(r) => r,
        Err(ureq::Error::Status(code, _)) => {
            return Err(TileFetchError::Http { url: safe, status: code });
        }
        // ureq's Transport Display embeds the full request URL — redact it too,
        // or the token survives into the error despite the redacted `url` field.
        Err(e) => {
            return Err(TileFetchError::Network {
                url: safe,
                source: redact_secrets(&e.to_string()),
            });
        }
    };
    // Residual guard for a 3xx that survives ureq's redirect cap (status < 400).
    if response.status() != 200 {
        return Err(TileFetchError::Http { url: safe, status: response.status() });
    }
    let mut bytes: Vec<u8> = Vec::with_capacity(64 * 1024);
    response
        .into_reader()
        .take(8 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|e| TileFetchError::Network { url: safe.clone(), source: e.to_string() })?;
    let img = image::load_from_memory(&bytes)
        .map_err(|e| TileFetchError::Decode { url: safe.clone(), source: e.to_string() })?
        .into_rgba8();
    // The whole-canvas offset math in fetch_bbox assumes every tile is exactly
    // TILE_SIZE_PX square. Enforce it as a real error (not debug_assert) — a
    // mis-sized third-party tile would otherwise walk paste_tile off the canvas
    // and panic in release. paste_tile keeps its debug_assert as defense-in-depth.
    if img.width() != TILE_SIZE_PX || img.height() != TILE_SIZE_PX {
        return Err(TileFetchError::Decode {
            url: safe,
            source: format!(
                "expected {TILE_SIZE_PX}x{TILE_SIZE_PX} tile, got {}x{}",
                img.width(),
                img.height()
            ),
        });
    }
    Ok(img)
}

/// Replace the value of any secret-bearing query parameter with `***` so a
/// URL embedded in an error never discloses an API token. Non-query URLs
/// (AWS Terrarium, ESRI) pass through unchanged so the error stays useful.
fn redact_url(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_owned();
    };
    let redacted: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, _)) if is_secret_param(k) => format!("{k}=***"),
            _ => pair.to_owned(),
        })
        .collect();
    format!("{base}?{}", redacted.join("&"))
}

fn is_secret_param(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k == "access_token" || k == "api_key" || k == "key" || k == "token"
}

/// Mask the value after any secret-bearing query parameter inside an arbitrary
/// message. Unlike [`redact_url`] this works on free text — ureq transport
/// errors embed the full request URL in their `Display` output, so any
/// stringified transport error must pass through here before reaching a log
/// line or the GUI.
pub(crate) fn redact_secrets(msg: &str) -> String {
    let mut out = msg.to_owned();
    for param in ["access_token=", "api_key=", "key=", "token="] {
        let mut from = 0;
        // Bounded: `from` strictly advances past each masked value. A later
        // pass can re-match inside an already-masked value (`key=` inside
        // `api_key=***`) — that re-mask is cosmetic and idempotent, never a leak.
        while from < out.len() {
            let lower = out.to_ascii_lowercase();
            let Some(rel) = lower[from..].find(param) else { break };
            let start = from + rel + param.len();
            let end = out[start..]
                .find(|c: char| {
                    !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '%' | '-'))
                })
                .map_or(out.len(), |i| start + i);
            out.replace_range(start..end, "***");
            from = start + 3;
        }
    }
    out
}

fn paste_tile(canvas: &mut RgbaImage, tile: &RgbaImage, ox: u32, oy: u32) {
    debug_assert!(
        ox + tile.width() <= canvas.width() && oy + tile.height() <= canvas.height(),
        "paste_tile precondition: tile must fit within canvas",
    );
    for y in 0..tile.height() {
        for x in 0..tile.width() {
            let p: Rgba<u8> = *tile.get_pixel(x, y);
            canvas.put_pixel(ox + x, oy + y, p);
        }
    }
}

fn crop_window(
    bbox: BBoxLatLon,
    zoom: u32,
    nw_tile: TileCoord,
    canvas_w: u32,
    canvas_h: u32,
) -> (u32, u32, u32, u32) {
    let (nw_world_x, nw_world_y) = lat_lon_to_world_px(bbox.north, bbox.west, zoom);
    let (se_world_x, se_world_y) = lat_lon_to_world_px(bbox.south, bbox.east, zoom);
    let canvas_x0 = (nw_tile.x * TILE_SIZE_PX) as f64;
    let canvas_y0 = (nw_tile.y * TILE_SIZE_PX) as f64;
    let x0 = ((nw_world_x - canvas_x0).round() as i64).clamp(0, canvas_w as i64 - 1) as u32;
    let y0 = ((nw_world_y - canvas_y0).round() as i64).clamp(0, canvas_h as i64 - 1) as u32;
    // Enforce a non-degenerate window as real logic, not a debug_assert: when a
    // thin bbox rounds both edges to the same pixel, force at least 1px so the
    // crop is never zero-width/height (which `image::crop_imm` would accept and
    // silently yield an empty raster — see decode_to_raster's empty guard).
    // x0 <= canvas_w-1, so x0+1 <= canvas_w and the .min keeps it in bounds.
    let x1_raw = ((se_world_x - canvas_x0).round() as i64).clamp(1, canvas_w as i64) as u32;
    let y1_raw = ((se_world_y - canvas_y0).round() as i64).clamp(1, canvas_h as i64) as u32;
    let x1 = x1_raw.max(x0 + 1).min(canvas_w);
    let y1 = y1_raw.max(y0 + 1).min(canvas_h);
    debug_assert!(x1 > x0 && y1 > y0, "crop_window postcondition: positive-size window");
    (x0, y0, x1, y1)
}

use std::io::Read;

#[cfg(test)]
mod tests {
    use super::*;

    fn horsetooth_bbox() -> BBoxLatLon {
        BBoxLatLon { north: 40.560, south: 40.523, east: -105.131, west: -105.183 }
    }

    #[test]
    fn lat_lon_to_tile_known_horsetooth_zoom_12() {
        // Horsetooth Reservoir (40.5417°N, -105.1556°W) at zoom 12 = tile
        // (z=12, x=851, y=1542) per the standard slippy-map formula
        // (https://wiki.openstreetmap.org/wiki/Slippy_map_tilenames).
        // Hand-verified:
        //   x = floor((-105.1556 + 180) / 360 * 4096) = 851
        //   y = floor((1 - ln(tan φ + sec φ)/π) / 2 * 4096) ≈ 1542
        let (x, y) = lat_lon_to_tile(40.5417, -105.1556, 12);
        assert_eq!(
            (x, y),
            (851, 1542),
            "Horsetooth at zoom 12 should map to (851, 1542)"
        );
    }

    #[test]
    fn lat_lon_to_tile_known_null_island_zoom_0() {
        // (0°N, 0°E) at zoom 0 → tile (0, 0). The whole world fits in one tile.
        let (x, y) = lat_lon_to_tile(0.0, 0.0, 0);
        assert_eq!((x, y), (0, 0), "Null Island at zoom 0 is the only tile");
    }

    #[test]
    fn lat_lon_to_tile_world_corners_zoom_2() {
        // At zoom 2, the world is a 4×4 tile grid.
        // NW corner (~85°N, -180°): tile (0, 0); SE corner (~-85°N, ~180°): tile (3, 3).
        let (nw_x, nw_y) = lat_lon_to_tile(84.0, -179.9, 2);
        assert_eq!((nw_x, nw_y), (0, 0));
        let (se_x, se_y) = lat_lon_to_tile(-84.0, 179.9, 2);
        assert_eq!((se_x, se_y), (3, 3));
    }

    #[test]
    fn pick_zoom_lands_in_target_window() {
        let z = pick_zoom(horsetooth_bbox(), MAX_ZOOM);
        let n = tile_count(horsetooth_bbox(), z);
        assert!(
            (TARGET_TILE_COUNT_LOW..=TARGET_TILE_COUNT_HIGH).contains(&n),
            "zoom {z} chose {n} tiles; expected in [{TARGET_TILE_COUNT_LOW}, {TARGET_TILE_COUNT_HIGH}]",
        );
    }

    #[test]
    fn pick_zoom_respects_low_provider_cap_on_tiny_bbox() {
        // ~11m hairline: under-fills the [LOW, HIGH] window at every zoom, so
        // uncapped it saturates to MAX_ZOOM. A z15 provider cap must clamp it to
        // 15 (the DEM ceiling), and an even lower cap must be honored too.
        let tiny = BBoxLatLon { north: 40.5001, south: 40.5000, east: -105.0000, west: -105.0001 };
        assert_eq!(
            pick_zoom(tiny, MAX_ZOOM),
            MAX_ZOOM,
            "control: an uncapped tiny bbox saturates at the global ceiling",
        );
        assert_eq!(pick_zoom(tiny, 15), 15, "must clamp to the provider z15 ceiling");
        assert!(pick_zoom(tiny, 12) <= 12, "must respect an even lower cap");
    }

    #[test]
    fn tile_range_canonical_order() {
        let (nw, se) = tile_range(horsetooth_bbox(), 12);
        assert!(nw.x <= se.x, "NW.x must be <= SE.x");
        assert!(nw.y <= se.y, "NW.y must be <= SE.y (Web Mercator y grows southward)");
        assert_eq!(nw.z, 12);
        assert_eq!(se.z, 12);
    }

    #[test]
    fn crop_window_within_canvas() {
        let bbox = horsetooth_bbox();
        let zoom = pick_zoom(bbox, MAX_ZOOM);
        let (nw, se) = tile_range(bbox, zoom);
        let canvas_w = (se.x - nw.x + 1) * TILE_SIZE_PX;
        let canvas_h = (se.y - nw.y + 1) * TILE_SIZE_PX;
        let (x0, y0, x1, y1) = crop_window(bbox, zoom, nw, canvas_w, canvas_h);
        assert!(x0 < x1 && y0 < y1, "crop must be non-degenerate");
        assert!(x1 <= canvas_w && y1 <= canvas_h, "crop must fit within canvas");
    }

    #[test]
    fn pick_zoom_keeps_tile_count_within_fetch_cap() {
        // The real safety property: pick_zoom must never select a zoom whose
        // tile count exceeds the per-fetch cap, which is what makes the
        // fetch_bbox BboxTooLarge branch a defensive backstop rather than the
        // primary guard. A continent-scale bbox is the stress case.
        let huge = BBoxLatLon { north: 70.0, south: -70.0, east: 180.0, west: -180.0 };
        let chosen = pick_zoom(huge, MAX_ZOOM);
        let n = tile_count(huge, chosen);
        assert!(
            n <= MAX_TILES_PER_FETCH,
            "pick_zoom chose zoom {chosen} with {n} tiles, exceeding the {MAX_TILES_PER_FETCH} cap",
        );
    }

    #[test]
    fn fetch_cap_is_meaningfully_sized() {
        // Guard against a vacuous cap: at a high fixed zoom a continent-scale
        // bbox genuinely produces far more than MAX_TILES_PER_FETCH tiles, so
        // the constant is bounding something real (not larger than any input).
        let huge = BBoxLatLon { north: 70.0, south: -70.0, east: 180.0, west: -180.0 };
        assert!(
            tile_count(huge, 8) > MAX_TILES_PER_FETCH,
            "expected a continent at zoom 8 to exceed the {MAX_TILES_PER_FETCH}-tile cap",
        );
    }

    #[test]
    fn redact_url_masks_token_but_keeps_path() {
        let dirty = "https://api.mapbox.com/v4/mapbox.satellite/12/851/1542.jpg90?access_token=pk.SECRET";
        let clean = redact_url(dirty);
        assert!(!clean.contains("pk.SECRET"), "token must be masked, got {clean}");
        assert!(clean.contains("access_token=***"), "masked param must remain, got {clean}");
        assert!(clean.contains("/12/851/1542.jpg90"), "path must survive, got {clean}");
    }

    #[test]
    fn redact_url_passthrough_for_keyless_urls() {
        let aws = "https://s3.amazonaws.com/elevation-tiles-prod/terrarium/12/851/1542.png";
        assert_eq!(redact_url(aws), aws, "keyless URLs must pass through unchanged");
    }

    #[test]
    fn redact_secrets_masks_token_inside_transport_error_text() {
        // Shape of a ureq Transport error Display: URL embedded mid-sentence.
        let msg = "https://api.mapbox.com/v4/x.png?access_token=pk.eyJSECRET123: \
                   Connection refused (os error 111)";
        let clean = redact_secrets(msg);
        assert!(!clean.contains("pk.eyJSECRET123"), "token must be masked: {clean}");
        assert!(clean.contains("access_token=***"));
        assert!(clean.contains("Connection refused"), "error detail must survive");
    }

    #[test]
    fn redact_secrets_masks_multiple_params_and_passes_clean_text() {
        let msg = "API_Key=abc123&demtype=SRTMGL1&token=zzz9 failed";
        let clean = redact_secrets(msg);
        assert!(!clean.contains("abc123") && !clean.contains("zzz9"), "{clean}");
        assert!(clean.contains("demtype=SRTMGL1"), "non-secret params survive");
        assert_eq!(redact_secrets("plain message"), "plain message");
    }

    #[test]
    fn crop_window_never_zero_width_for_thin_bbox() {
        // A near-vertical hairline bbox previously rounded both x edges to the
        // same pixel (zero-width crop -> empty raster -> INFINITY elevation).
        // crop_window must now force a >=1px window.
        let thin = BBoxLatLon { north: 40.5, south: 40.0, east: -105.0, west: -105.0001 };
        let zoom = pick_zoom(thin, MAX_ZOOM);
        let (nw, se) = tile_range(thin, zoom);
        let canvas_w = (se.x - nw.x + 1) * TILE_SIZE_PX;
        let canvas_h = (se.y - nw.y + 1) * TILE_SIZE_PX;
        let (x0, y0, x1, y1) = crop_window(thin, zoom, nw, canvas_w, canvas_h);
        assert!(x1 > x0, "thin-bbox crop must have positive width, got x0={x0} x1={x1}");
        assert!(y1 > y0, "crop must have positive height, got y0={y0} y1={y1}");
    }
}
