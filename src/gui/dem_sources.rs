//! DEM (digital elevation model) source catalog: picker metadata, XYZ tile
//! URLs, and pixel→meters decoders for AWS Terrarium and Mapbox Terrain-RGB.
//! OpenTopography is not an XYZ source — build.rs fetches it as a single-shot
//! bbox GeoTIFF via its REST API. USGS 3DEP is metadata-only until wired.

use super::tiles::{TileCoord, TileSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DemSource {
    AwsTerrarium,
    MapboxTerrainRgb,
    OpenTopography,
    Usgs3Dep,
}

impl DemSource {
    pub(crate) const ALL: &'static [Self] = &[
        Self::AwsTerrarium,
        Self::MapboxTerrainRgb,
        Self::OpenTopography,
        Self::Usgs3Dep,
    ];

    pub(crate) const fn display_label(self) -> &'static str {
        match self {
            Self::AwsTerrarium => "AWS Terrarium (free, global, 30m)",
            Self::MapboxTerrainRgb => "Mapbox Terrain-RGB (token, 30m)",
            Self::OpenTopography => "OpenTopography SRTM (free key, 30m)",
            Self::Usgs3Dep => "USGS 3DEP (free, US only, 1m LiDAR)",
        }
    }

    pub(crate) const fn required_key(self) -> Option<RequiredKey> {
        match self {
            Self::AwsTerrarium | Self::Usgs3Dep => None,
            Self::MapboxTerrainRgb => Some(RequiredKey::MapboxToken),
            Self::OpenTopography => Some(RequiredKey::OpenTopoApiKey),
        }
    }

    pub(crate) const fn coverage(self) -> Coverage {
        match self {
            Self::AwsTerrarium | Self::MapboxTerrainRgb | Self::OpenTopography => Coverage::Global,
            Self::Usgs3Dep => Coverage::UnitedStates,
        }
    }

    pub(crate) const fn tooltip(self) -> &'static str {
        match self {
            Self::AwsTerrarium => {
                "Public S3 tile pyramid, RGB-encoded heights. \
                 No signup. Recommended default."
            }
            Self::MapboxTerrainRgb => {
                "Higher quality than AWS Terrarium. \
                 Needs a free Mapbox account token (set in Settings)."
            }
            Self::OpenTopography => {
                "SRTMGL1 (~30 m) GeoTIFF from portal.opentopography.org. \
                 Free API key required (set in Settings). 450,000 km² cap per request."
            }
            Self::Usgs3Dep => {
                "1m LiDAR-derived DEM, US contiguous only. \
                 No signup, but bbox must be within US territory."
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RequiredKey {
    MapboxToken,
    OpenTopoApiKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Coverage {
    Global,
    UnitedStates,
}

impl Coverage {
    pub(crate) const fn short_badge(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::UnitedStates => "US only",
        }
    }
}

/// `TileSource` impl for AWS Terrarium DEM tiles (public S3, no auth).
///
/// URL template: `s3.amazonaws.com/elevation-tiles-prod/terrarium/{z}/{x}/{y}.png`
/// Tile format: 256×256 PNG, RGB-encoded heights via
/// `meters = (R*256 + G + B/256) - 32768`. See [decode_aws_terrarium_pixel].
pub(crate) struct AwsTerrariumTiles;

impl TileSource for AwsTerrariumTiles {
    fn url(&self, coord: TileCoord) -> String {
        format!(
            "https://s3.amazonaws.com/elevation-tiles-prod/terrarium/{}/{}/{}.png",
            coord.z, coord.x, coord.y,
        )
    }

    /// Tilezen/Mapzen Terrain Tiles serve 256px terrarium data through z15;
    /// requesting past z15 returns HTTP 404 (joerd use-service docs).
    fn max_zoom(&self) -> u32 {
        15
    }
}

/// Decode a single AWS Terrarium pixel to meters above sea level.
///
/// Formula (from <https://github.com/tilezen/joerd/blob/master/docs/formats.md>):
/// `meters = (R*256 + G + B/256) - 32768`.
///
/// Sea level reads as `(128*256 + 0 + 0/256) - 32768 = 0`. Mt. Everest
/// (~8848m) reads as `R=163, G=176, B=…` clustered near the upper bound;
/// the deepest ocean trench (~−11034m) reads near the lower bound.
pub(crate) fn decode_aws_terrarium_pixel(rgb: [u8; 3]) -> f32 {
    let r = rgb[0] as f32;
    let g = rgb[1] as f32;
    let b = rgb[2] as f32;
    r * 256.0 + g + b / 256.0 - 32768.0
}

/// `TileSource` impl for Mapbox Terrain-RGB DEM tiles.
///
/// URL template: `api.mapbox.com/v4/mapbox.terrain-rgb/{z}/{x}/{y}.pngraw?access_token=…`
/// Tile format: 256×256 PNG, RGB-encoded heights via the Mapbox formula
/// `meters = (R*65536 + G*256 + B) * 0.1 - 10000` (see [decode_mapbox_terrain_rgb_pixel]).
///
/// Requires a Mapbox access token; held by value in this struct so the
/// `TileSource` trait remains parameter-free.
pub(crate) struct MapboxTerrainRgbTiles {
    pub(crate) token: String,
}

impl TileSource for MapboxTerrainRgbTiles {
    fn url(&self, coord: TileCoord) -> String {
        format!(
            "https://api.mapbox.com/v4/mapbox.terrain-rgb/{}/{}/{}.pngraw?access_token={}",
            coord.z, coord.x, coord.y, self.token,
        )
    }

    /// mapbox-terrain-rgb-v1 encodes data to the equivalent of z15 at 256px;
    /// higher zooms add no elevation detail (Mapbox tileset reference).
    fn max_zoom(&self) -> u32 {
        15
    }
}

/// Decode a single Mapbox Terrain-RGB pixel to meters above sea level.
///
/// Formula (from <https://docs.mapbox.com/data/tilesets/reference/mapbox-terrain-rgb-v1/>):
/// `meters = (R*65536 + G*256 + B) * 0.1 - 10000`.
///
/// Range: −10000 m floor (pixel `[0,0,0]` decodes to −10000) up to ~+8848 m
/// for Everest. Resolution: 0.1 m per smallest step (versus AWS Terrarium's
/// ~0.004 m — Mapbox is coarser by ~25× but adequate for brick height).
pub(crate) fn decode_mapbox_terrain_rgb_pixel(rgb: [u8; 3]) -> f32 {
    let r = rgb[0] as f32;
    let g = rgb[1] as f32;
    let b = rgb[2] as f32;
    (r * 65536.0 + g * 256.0 + b) * 0.1 - 10000.0
}

/// Construct the `TileSource` for a [`DemSource`] variant. The `token`
/// parameter is required for token-bearing variants (`MapboxTerrainRgb`);
/// other variants ignore it. Returns `None` for non-tile variants
/// (`OpenTopography` fetches via REST in build.rs; `Usgs3Dep` is unwired) and
/// for any variant whose required token is missing or empty.
pub(crate) fn tile_source_for(
    source: DemSource,
    token: Option<&str>,
) -> Option<Box<dyn TileSource>> {
    match source {
        DemSource::AwsTerrarium => Some(Box::new(AwsTerrariumTiles)),
        DemSource::MapboxTerrainRgb => {
            let t = token?.trim();
            if t.is_empty() {
                return None;
            }
            Some(Box::new(MapboxTerrainRgbTiles { token: t.to_owned() }))
        }
        DemSource::OpenTopography | DemSource::Usgs3Dep => None,
    }
}

/// Per-source decoder dispatch: stitched RGBA pixel → meters above sea level.
///
/// Returns `None` for variants that don't use a tile decoder path (single-shot
/// GeoTIFF sources are handled in their own clients).
pub(crate) fn decode_pixel_for(source: DemSource, rgba: [u8; 4]) -> Option<f32> {
    match source {
        DemSource::AwsTerrarium => Some(decode_aws_terrarium_pixel([rgba[0], rgba[1], rgba[2]])),
        DemSource::MapboxTerrainRgb => Some(decode_mapbox_terrain_rgb_pixel([
            rgba[0], rgba[1], rgba[2],
        ])),
        DemSource::OpenTopography | DemSource::Usgs3Dep => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terrarium_decode_known_sentinels() {
        // Sea level → R=128, G=0, B=0
        assert!((decode_aws_terrarium_pixel([128, 0, 0]) - 0.0).abs() < 0.01);
        // -32768 m (formula floor) → R=0, G=0, B=0
        assert!((decode_aws_terrarium_pixel([0, 0, 0]) - (-32768.0)).abs() < 0.01);
        // Smallest positive step (1/256 m above the floor) → R=0, G=0, B=1
        assert!((decode_aws_terrarium_pixel([0, 0, 1]) - (-32768.0 + 1.0 / 256.0)).abs() < 1e-4);
    }

    #[test]
    fn terrarium_decode_midrange_pixel_in_expected_band() {
        // NOT ground truth — there are no real Horsetooth pixel bytes here
        // without IO. This only checks the formula maps a mid-range encoding
        // into a plausible montane band (a gross scale/offset break would
        // fail). True ground-truth coverage is the live b2_1 E2E elevation
        // assertion; the sentinel test above covers the exact reference points.
        let mid = decode_aws_terrarium_pixel([137, 4, 0]);
        assert!(
            (2000.0..=2700.0).contains(&mid),
            "mid-range Terrarium pixel decoded to {mid} m, expected a montane ~2300 m band",
        );
    }

    #[test]
    fn mapbox_terrain_rgb_decode_known_sentinels() {
        // Floor: pixel [0,0,0] → -10000 m
        assert!(
            (decode_mapbox_terrain_rgb_pixel([0, 0, 0]) - (-10000.0)).abs() < 0.01,
            "Mapbox floor pixel must decode to -10000 m",
        );
        // Sea level: solve (R*65536 + G*256 + B) * 0.1 = 10000 → 100000.
        // 100000 = R*65536 + G*256 + B → R=1, G=134, B=160.
        let sea_level = decode_mapbox_terrain_rgb_pixel([1, 134, 160]);
        assert!(
            sea_level.abs() < 0.1,
            "sea-level pixel decoded to {sea_level} m, expected ~0",
        );
    }

    #[test]
    fn mapbox_url_includes_token() {
        let src = MapboxTerrainRgbTiles { token: "pk.test_token_xyz".to_owned() };
        let url = src.url(TileCoord { z: 12, x: 851, y: 1542 });
        assert!(
            url.contains("/12/851/1542.pngraw"),
            "URL must contain z/x/y/.pngraw, got {url}",
        );
        assert!(
            url.contains("access_token=pk.test_token_xyz"),
            "URL must propagate access token, got {url}",
        );
    }

    #[test]
    fn tile_source_for_mapbox_requires_nonempty_token() {
        assert!(tile_source_for(DemSource::MapboxTerrainRgb, None).is_none());
        assert!(tile_source_for(DemSource::MapboxTerrainRgb, Some("")).is_none());
        assert!(tile_source_for(DemSource::MapboxTerrainRgb, Some("   ")).is_none());
        assert!(tile_source_for(DemSource::MapboxTerrainRgb, Some("pk.realish")).is_some());
    }

    #[test]
    fn tile_source_for_aws_terrarium_ignores_token() {
        // AWS Terrarium is public S3; passing a token must not change behavior.
        assert!(tile_source_for(DemSource::AwsTerrarium, None).is_some());
        assert!(tile_source_for(DemSource::AwsTerrarium, Some("ignored")).is_some());
    }
}
