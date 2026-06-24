//! walkers `TileSource` adapters so the slippy-map PREVIEW renders the selected
//! imagery basemap (ESRI World Imagery / Mapbox Satellite) instead of always
//! OpenStreetMap. The URL templates MIRROR the build-time ones in
//! `imagery_sources` — they are pinned byte-equal by
//! `preview_urls_equal_the_build_time_templates`, so the preview can never drift
//! from the provider the build actually colormaps with. This is a cosmetic
//! preview path only — `build.rs` fetches its own DEM + imagery independently and
//! is untouched by anything here.

use walkers::sources::{Attribution, TileSource};
use walkers::{HttpTiles, TileId};

use super::imagery_sources::ImagerySource;

/// ESRI World Imagery XYZ basemap. URL path is z/y/x (the ESRI convention),
/// identical to `imagery_sources::EsriWorldImageryTiles`.
struct EsriPreview;

impl TileSource for EsriPreview {
    fn tile_url(&self, t: TileId) -> String {
        format!(
            "https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{}/{}/{}",
            t.zoom, t.y, t.x,
        )
    }
    fn attribution(&self) -> Attribution {
        Attribution {
            text: "Esri, Maxar, Earthstar Geographics",
            url: "https://www.esri.com",
            logo_light: None,
            logo_dark: None,
        }
    }
    fn max_zoom(&self) -> u8 {
        23
    }
}

/// Mapbox Satellite raster basemap. The token rides in the URL exactly as the
/// build path does (`imagery_sources::MapboxSatelliteTiles`); walkers logs tile
/// URLs only at `trace` and the app never surfaces this URL, so the token is no
/// more exposed than at build time. Do NOT plumb preview tile URLs into the GUI
/// or a log line.
struct MapboxSatellitePreview {
    token: String,
}

impl TileSource for MapboxSatellitePreview {
    fn tile_url(&self, t: TileId) -> String {
        format!(
            "https://api.mapbox.com/v4/mapbox.satellite/{}/{}/{}.jpg90?access_token={}",
            t.zoom, t.x, t.y, self.token,
        )
    }
    fn attribution(&self) -> Attribution {
        Attribution {
            text: "© Mapbox, © OpenStreetMap",
            url: "https://www.mapbox.com/about/maps/",
            logo_light: None,
            logo_dark: None,
        }
    }
    fn max_zoom(&self) -> u8 {
        22
    }
}

/// Which basemap the preview should render, derived purely from the selected
/// imagery source + token. OSM is the fallback for None / unwired / missing
/// token, so the preview is never blank and OSM stays the safe default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PreviewBasemap {
    Osm,
    Esri,
    Mapbox(String),
}

impl PreviewBasemap {
    /// Resolve the imagery picker + token into the basemap to show. Mapbox falls
    /// back to OSM when the token is missing or blank, so the preview never goes
    /// to a dead grey grid.
    pub(crate) fn resolve(src: ImagerySource, token: Option<&str>) -> Self {
        match src {
            ImagerySource::EsriWorldImagery => Self::Esri,
            ImagerySource::MapboxSatellite => {
                match token.map(str::trim).filter(|t| !t.is_empty()) {
                    Some(t) => Self::Mapbox(t.to_owned()),
                    None => Self::Osm,
                }
            }
            ImagerySource::None | ImagerySource::UsgsOrthoimagery => Self::Osm,
        }
    }

    /// Build the `HttpTiles` for this basemap. `HttpTiles` is non-generic (it
    /// monomorphizes the source internally), so every variant yields the one
    /// `HttpTiles` type the Map tab stores and swaps.
    pub(crate) fn build_tiles(&self, ctx: &egui::Context) -> HttpTiles {
        use walkers::sources::OpenStreetMap;
        match self {
            Self::Osm => HttpTiles::new(OpenStreetMap, ctx.clone()),
            Self::Esri => HttpTiles::new(EsriPreview, ctx.clone()),
            Self::Mapbox(t) => {
                HttpTiles::new(MapboxSatellitePreview { token: t.clone() }, ctx.clone())
            }
        }
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Osm => "OpenStreetMap",
            Self::Esri => "ESRI World Imagery",
            Self::Mapbox(_) => "Mapbox Satellite",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_falls_back_to_osm_without_imagery_or_token() {
        assert_eq!(PreviewBasemap::resolve(ImagerySource::None, None), PreviewBasemap::Osm);
        assert_eq!(
            PreviewBasemap::resolve(ImagerySource::UsgsOrthoimagery, Some("pk.x")),
            PreviewBasemap::Osm,
            "an unwired source must not produce a dead basemap",
        );
        assert_eq!(PreviewBasemap::resolve(ImagerySource::EsriWorldImagery, None), PreviewBasemap::Esri);
        // Mapbox needs a non-blank token, else OSM fallback.
        assert_eq!(PreviewBasemap::resolve(ImagerySource::MapboxSatellite, None), PreviewBasemap::Osm);
        assert_eq!(PreviewBasemap::resolve(ImagerySource::MapboxSatellite, Some("   ")), PreviewBasemap::Osm);
        assert_eq!(
            PreviewBasemap::resolve(ImagerySource::MapboxSatellite, Some("pk.tok")),
            PreviewBasemap::Mapbox("pk.tok".to_owned()),
        );
    }

    #[test]
    fn preview_urls_equal_the_build_time_templates() {
        // Drift guard: the preview adapters must produce byte-identical URLs to
        // the build-time tile sources, or the preview would show a different
        // provider/quality than the build colormaps with. UFCS disambiguates the
        // local `tiles::TileSource::url` from walkers' `tile_url`.
        use crate::gui::imagery_sources::{EsriWorldImageryTiles, MapboxSatelliteTiles};
        use crate::gui::tiles::{TileCoord, TileSource as BuildSrc};
        let coord = TileCoord { z: 12, x: 851, y: 1542 };
        let tile = TileId { zoom: 12, x: 851, y: 1542 };
        assert_eq!(
            EsriPreview.tile_url(tile),
            BuildSrc::url(&EsriWorldImageryTiles, coord),
            "preview ESRI URL must EQUAL the build URL",
        );
        let tok = "pk.tok".to_owned();
        assert_eq!(
            MapboxSatellitePreview { token: tok.clone() }.tile_url(tile),
            BuildSrc::url(&MapboxSatelliteTiles { token: tok }, coord),
            "preview Mapbox URL must EQUAL the build URL (incl token)",
        );
    }

    #[test]
    fn preview_urls_match_the_build_time_templates() {
        // ESRI z/y/x; Mapbox z/x/y.jpg90 + token — must mirror imagery_sources so
        // the preview shows the same provider the build will colormap with.
        let esri = EsriPreview.tile_url(TileId { zoom: 12, x: 851, y: 1542 });
        assert!(esri.ends_with("/tile/12/1542/851"), "ESRI must be y-before-x, got {esri}");
        let mb = MapboxSatellitePreview { token: "pk.tok".into() }
            .tile_url(TileId { zoom: 12, x: 851, y: 1542 });
        assert!(mb.contains("/12/851/1542.jpg90"), "Mapbox must be z/x/y.jpg90, got {mb}");
        assert!(mb.contains("access_token=pk.tok"), "token must ride in the URL, got {mb}");
    }
}
