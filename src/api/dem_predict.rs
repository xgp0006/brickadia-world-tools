//! Pure DEM cell / resolution prediction for UI shells (no network).
//!
//! Mirrors the Map-tab status math so Tauri can show the same “~m/cell” readout
//! before a fetch. Full Map fetch still lives under `gui` (ureq + tiles).

use serde::{Deserialize, Serialize};

/// Web Mercator latitude clamp (same as slippy-map tile math).
const MERCATOR_LAT_LIMIT: f64 = 85.0511;
const TILE_SIZE_PX: f64 = 256.0;
const EARTH_CIRCUMFERENCE_M: f64 = 40_075_016.686;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DemSourceDto {
    #[default]
    AwsTerrarium,
    MapboxTerrainRgb,
    OpenTopography,
    Usgs3Dep,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemPredictRequest {
    pub north: f64,
    pub south: f64,
    pub east: f64,
    pub west: f64,
    #[serde(default)]
    pub dem_source: DemSourceDto,
    /// Downsample factor (1 = full detail). Same meaning as Map-tab Downsample.
    #[serde(default = "one_u16")]
    pub density_factor: u16,
}

fn one_u16() -> u16 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemPredictResult {
    /// Metres per DEM cell before density downsample.
    pub cell_m: f64,
    /// Effective metres per cell after density (`cell_m * density`).
    pub cell_m_eff: f64,
    /// Approximate cropped cell count after density (upper bound-ish).
    pub approx_cells: u64,
    /// Provider zoom when tile-based; `None` for GeoTIFF sources.
    pub zoom: Option<u32>,
    /// Provider max zoom when tile-based.
    pub zoom_cap: Option<u32>,
    pub notes: String,
}

/// Ground resolution of a Web Mercator pixel at `zoom` (metres/pixel at `lat`).
fn ground_resolution_m(lat_deg: f64, zoom: u32) -> f64 {
    let lat = lat_deg.clamp(-MERCATOR_LAT_LIMIT, MERCATOR_LAT_LIMIT);
    let n = 2f64.powi(zoom as i32);
    EARTH_CIRCUMFERENCE_M * lat.to_radians().cos() / (TILE_SIZE_PX * n)
}

const KM_PER_DEG_LAT: f64 = 110.574;
const KM_PER_DEG_LON_EQUATOR: f64 = 111.320;

/// Rough cell count for a bbox at mercator zoom (matches tiles::approx spirit).
fn approx_mercator_cells(north: f64, south: f64, east: f64, west: f64, zoom: u32) -> u64 {
    let lat = ((north + south) * 0.5).clamp(-MERCATOR_LAT_LIMIT, MERCATOR_LAT_LIMIT);
    let cell = ground_resolution_m(lat, zoom);
    if cell <= 0.0 {
        return 0;
    }
    // metres = deg × (km/deg) × 1000
    let height_m = ((north - south) * KM_PER_DEG_LAT * 1000.0).abs();
    let width_m = ((east - west) * KM_PER_DEG_LON_EQUATOR * lat.to_radians().cos() * 1000.0).abs();
    let cols = (width_m / cell).ceil().max(1.0) as u64;
    let rows = (height_m / cell).ceil().max(1.0) as u64;
    cols.saturating_mul(rows)
}

/// Predict DEM sampling for a box without fetching.
pub fn predict_dem_cells(req: DemPredictRequest) -> Result<DemPredictResult, String> {
    if !(req.north > req.south && req.east >= req.west) {
        return Err("bbox must have north>south and east>=west".into());
    }
    let density = u64::from(req.density_factor.max(1));
    let lat = (req.north + req.south) * 0.5;

    match req.dem_source {
        DemSourceDto::OpenTopography => {
            let cos_lat = lat.to_radians().cos().max(0.01);
            let cell_m = 30.92 * cos_lat.sqrt();
            let height_m = ((req.north - req.south) * KM_PER_DEG_LAT * 1000.0).abs();
            let width_m =
                ((req.east - req.west) * KM_PER_DEG_LON_EQUATOR * lat.to_radians().cos() * 1000.0)
                    .abs();
            let cols = (width_m / cell_m).ceil().max(1.0) as u64;
            let rows = (height_m / cell_m).ceil().max(1.0) as u64;
            let cells = cols.saturating_mul(rows) / density.saturating_mul(density).max(1);
            Ok(DemPredictResult {
                cell_m,
                cell_m_eff: cell_m * density as f64,
                approx_cells: cells,
                zoom: None,
                zoom_cap: None,
                notes: "SRTMGL1 ~30 m; free API key required".into(),
            })
        }
        DemSourceDto::Usgs3Dep => {
            // Same budget logic spirit as build::usgs_3dep_export_size (1 m target).
            let height_m = ((req.north - req.south) * KM_PER_DEG_LAT * 1000.0)
                .abs()
                .max(1.0);
            let width_m =
                ((req.east - req.west) * KM_PER_DEG_LON_EQUATOR * lat.to_radians().cos() * 1000.0)
                    .abs()
                    .max(1.0);
            let max_cells = 400_000f64;
            let mut cell_m = 1.0;
            let (cols, rows) = loop {
                let c = (width_m / cell_m).ceil().max(2.0);
                let r = (height_m / cell_m).ceil().max(2.0);
                if c * r <= max_cells {
                    break (c as u64, r as u64);
                }
                cell_m *= 1.5;
                if cell_m > 50_000.0 {
                    break (2, 2);
                }
            };
            let cells = cols.saturating_mul(rows) / density.saturating_mul(density).max(1);
            Ok(DemPredictResult {
                cell_m,
                cell_m_eff: cell_m * density as f64,
                approx_cells: cells,
                zoom: None,
                zoom_cap: None,
                notes: "USGS 3DEP National Map; US only; auto-coarsens for mesh budget".into(),
            })
        }
        DemSourceDto::AwsTerrarium | DemSourceDto::MapboxTerrainRgb => {
            let cap = 15u32;
            // Pick highest zoom under ~400k cells (same spirit as pick_zoom).
            let mut zoom = 1u32;
            for z in (1..=cap).rev() {
                if approx_mercator_cells(req.north, req.south, req.east, req.west, z) <= 400_000 {
                    zoom = z;
                    break;
                }
            }
            let cell_m = ground_resolution_m(lat, zoom);
            let cells = approx_mercator_cells(req.north, req.south, req.east, req.west, zoom)
                / density.saturating_mul(density).max(1);
            let name = match req.dem_source {
                DemSourceDto::AwsTerrarium => "AWS Terrarium",
                _ => "Mapbox Terrain-RGB",
            };
            Ok(DemPredictResult {
                cell_m,
                cell_m_eff: cell_m * density as f64,
                approx_cells: cells,
                zoom: Some(zoom),
                zoom_cap: Some(cap),
                notes: format!("{name} tile DEM; cap z{cap}"),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terrarium_tiny_box_hits_zoom_cap() {
        // ~1 km box
        let r = predict_dem_cells(DemPredictRequest {
            north: 40.01,
            south: 40.0,
            east: -105.0,
            west: -105.01,
            dem_source: DemSourceDto::AwsTerrarium,
            density_factor: 1,
        })
        .unwrap();
        assert_eq!(r.zoom_cap, Some(15));
        assert!(r.zoom.unwrap() >= 12, "small box should use high zoom, got {:?}", r.zoom);
        assert!(r.cell_m < 30.0, "should be finer than SRTM, got {}", r.cell_m);
    }

    #[test]
    fn usgs_notes_us_only() {
        let r = predict_dem_cells(DemPredictRequest {
            north: 40.01,
            south: 40.0,
            east: -105.0,
            west: -105.01,
            dem_source: DemSourceDto::Usgs3Dep,
            density_factor: 1,
        })
        .unwrap();
        assert!(r.notes.contains("US"));
        assert!(r.cell_m <= 5.0, "small box near 1 m, got {}", r.cell_m);
    }
}
