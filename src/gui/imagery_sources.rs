//! Imagery (satellite) source catalog for the colormap layer: ESRI World
//! Imagery and Mapbox Satellite via the shared tile stitcher. USGS
//! orthoimagery is metadata-only until wired.

use super::dem_sources::{Coverage, RequiredKey};
use super::tiles::{TileCoord, TileSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ImagerySource {
    None,
    EsriWorldImagery,
    MapboxSatellite,
    UsgsOrthoimagery,
}

impl ImagerySource {
    pub(crate) const ALL: &'static [Self] = &[
        Self::None,
        Self::EsriWorldImagery,
        Self::MapboxSatellite,
        Self::UsgsOrthoimagery,
    ];

    pub(crate) const fn display_label(self) -> &'static str {
        match self {
            Self::None => "None (terrain only, no colormap)",
            Self::EsriWorldImagery => "ESRI World Imagery (free, global)",
            Self::MapboxSatellite => "Mapbox Satellite (token, global)",
            Self::UsgsOrthoimagery => "USGS Orthoimagery (free, US only, 1m)",
        }
    }

    pub(crate) const fn required_key(self) -> Option<RequiredKey> {
        match self {
            Self::None | Self::EsriWorldImagery | Self::UsgsOrthoimagery => None,
            Self::MapboxSatellite => Some(RequiredKey::MapboxToken),
        }
    }

    pub(crate) const fn coverage(self) -> Coverage {
        match self {
            Self::None | Self::EsriWorldImagery | Self::MapboxSatellite => Coverage::Global,
            Self::UsgsOrthoimagery => Coverage::UnitedStates,
        }
    }

    pub(crate) const fn tooltip(self) -> &'static str {
        match self {
            Self::None => {
                "Skip the colormap fetch entirely. \
                 Terrain renders in the default brick material."
            }
            Self::EsriWorldImagery => {
                "Standard XYZ tiles from ArcGIS. \
                 No signup. Recommended default."
            }
            Self::MapboxSatellite => {
                "Higher resolution than ESRI in most areas. \
                 Needs a free Mapbox account token (set in Settings)."
            }
            Self::UsgsOrthoimagery => {
                "1m orthoimagery, US contiguous only. \
                 No signup, but bbox must be within US territory."
            }
        }
    }
}

/// `TileSource` impl for ESRI World Imagery (ArcGIS Online).
///
/// URL pattern: `.../World_Imagery/MapServer/tile/{z}/{y}/{x}` — note the
/// y-before-x ordering in the URL path (ESRI convention). Tile addressing
/// is identical to the XYZ standard so the same `TileCoord` works.
pub(crate) struct EsriWorldImageryTiles;

impl TileSource for EsriWorldImageryTiles {
    fn url(&self, coord: TileCoord) -> String {
        format!(
            "https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{}/{}/{}",
            coord.z, coord.y, coord.x,
        )
    }

    /// World_Imagery MapServer tiling scheme defines LODs 0–23 (service
    /// metadata f=json maxLOD=23). Above native imagery it upscales, no 404.
    fn max_zoom(&self) -> u32 {
        23
    }
}

/// `TileSource` impl for Mapbox Satellite imagery.
///
/// URL pattern:
/// `api.mapbox.com/v4/mapbox.satellite/{z}/{x}/{y}.jpg90?access_token=…`.
/// The `.jpg90` extension is Mapbox's JPEG-quality-90 raster format.
pub(crate) struct MapboxSatelliteTiles {
    pub(crate) token: String,
}

impl TileSource for MapboxSatelliteTiles {
    fn url(&self, coord: TileCoord) -> String {
        format!(
            "https://api.mapbox.com/v4/mapbox.satellite/{}/{}/{}.jpg90?access_token={}",
            coord.z, coord.x, coord.y, self.token,
        )
    }

    /// Mapbox maps span 23 zoom levels (max z22). Native imagery resolution
    /// varies by region; above it the tiles are overzoomed, not detail-bearing.
    fn max_zoom(&self) -> u32 {
        22
    }
}

/// Construct the `TileSource` for an [`ImagerySource`] variant. The `token`
/// parameter is required for token-bearing variants (`MapboxSatellite`);
/// other variants ignore it. Returns `None` for the unwired
/// `UsgsOrthoimagery`, for the explicit `None` variant, and
/// for any token-required variant whose token is missing or empty.
pub(crate) fn tile_source_for(
    source: ImagerySource,
    token: Option<&str>,
) -> Option<Box<dyn TileSource>> {
    match source {
        ImagerySource::EsriWorldImagery => Some(Box::new(EsriWorldImageryTiles)),
        ImagerySource::MapboxSatellite => {
            let t = token?.trim();
            if t.is_empty() {
                return None;
            }
            Some(Box::new(MapboxSatelliteTiles { token: t.to_owned() }))
        }
        ImagerySource::None | ImagerySource::UsgsOrthoimagery => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esri_url_format() {
        let url = EsriWorldImageryTiles.url(TileCoord { z: 12, x: 851, y: 1542 });
        // Verify y/x ordering: zoom/y/x = 12/1542/851
        assert!(
            url.ends_with("/tile/12/1542/851"),
            "ESRI URL must use y-before-x ordering; got {url}",
        );
    }

    #[test]
    fn esri_tile_source_ignores_token() {
        // ESRI World Imagery is public; passing a token must not change behavior.
        assert!(tile_source_for(ImagerySource::EsriWorldImagery, None).is_some());
        assert!(tile_source_for(ImagerySource::EsriWorldImagery, Some("ignored")).is_some());
    }

    #[test]
    fn none_imagery_returns_none() {
        assert!(tile_source_for(ImagerySource::None, None).is_none());
        assert!(tile_source_for(ImagerySource::None, Some("pk.x")).is_none());
    }

    #[test]
    fn mapbox_satellite_requires_nonempty_token() {
        assert!(tile_source_for(ImagerySource::MapboxSatellite, None).is_none());
        assert!(tile_source_for(ImagerySource::MapboxSatellite, Some("")).is_none());
        assert!(tile_source_for(ImagerySource::MapboxSatellite, Some("   ")).is_none());
        assert!(tile_source_for(ImagerySource::MapboxSatellite, Some("pk.realish")).is_some());
    }

    #[test]
    fn mapbox_satellite_url_format() {
        let src = MapboxSatelliteTiles { token: "pk.test_token_abc".to_owned() };
        let url = src.url(TileCoord { z: 12, x: 851, y: 1542 });
        assert!(
            url.contains("/12/851/1542.jpg90"),
            "Mapbox Satellite URL must contain z/x/y/.jpg90, got {url}",
        );
        assert!(
            url.contains("access_token=pk.test_token_abc"),
            "Mapbox Satellite URL must propagate access token, got {url}",
        );
    }

    #[test]
    fn usgs_ortho_not_yet_wired() {
        assert!(tile_source_for(ImagerySource::UsgsOrthoimagery, None).is_none());
    }
}
