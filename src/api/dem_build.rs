//! DEM fetch + mesh + write for UI shells (Tauri Map, future CLI).
//!
//! Wraps [`crate::gui::build::run_build`] with serializable DTOs and progress
//! callbacks. Requires `feature = "dem"`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::api::convert::BrickModeDto;
use crate::api::dem_predict::{predict_dem_cells, DemPredictRequest, DemSourceDto};
use crate::gui::build::{
    self, BlockType, BuildOutcome, BuildRequest, BuildStage, ProgressFn,
};
use crate::gui::config::Config;
use crate::gui::dem_sources::DemSource;
use crate::gui::imagery_sources::ImagerySource;
use crate::gui::tiles::BBoxLatLon;

const DEFAULT_STUDS_PER_METER: f32 = 4.0;

fn one_u16() -> u16 {
    1
}
fn default_studs() -> f32 {
    DEFAULT_STUDS_PER_METER
}
fn default_exag() -> f32 {
    1.0
}
fn default_name() -> String {
    "map-build".into()
}

/// Serializable DEM build request (Tauri `dem_fetch_build` args).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemBuildRequest {
    pub north: f64,
    pub south: f64,
    pub east: f64,
    pub west: f64,
    #[serde(default)]
    pub dem_source: DemSourceDto,
    /// Integer DEM downsample (1 = full detail). Same as Map-tab Downsample.
    #[serde(default = "one_u16")]
    pub density_factor: u16,
    /// Target studs per real-world metre (Map-tab true-scale control).
    #[serde(default = "default_studs")]
    pub studs_per_meter: f32,
    /// Vertical relief multiplier (1.0 = faithful 1:1).
    #[serde(default = "default_exag")]
    pub vertical_exaggeration: f32,
    #[serde(default = "default_name")]
    pub output_name: String,
    /// Copy into Brickadia Worlds after write (soft-fail).
    #[serde(default)]
    pub install: bool,
    /// When installing, overwrite same stem instead of suffixing `-2`, …
    #[serde(default)]
    pub overwrite: bool,
    /// Mapbox token; empty/None falls back to `~/.config/heightmap2brz/config.toml`.
    #[serde(default)]
    pub mapbox_token: Option<String>,
    /// OpenTopography API key; empty/None falls back to config.toml.
    #[serde(default)]
    pub opentopo_key: Option<String>,
    #[serde(default)]
    pub brick_mode: BrickModeDto,
    #[serde(default)]
    pub glow: bool,
    #[serde(default)]
    pub no_collision: bool,
    #[serde(default)]
    pub omit_below_m: f32,
    #[serde(default)]
    pub floor_level_m: f32,
}

/// Progress snapshot (phase labels match egui `BuildStage::label`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemBuildProgress {
    pub phase: String,
    /// 0.0..=1.0 within the current phase.
    pub frac: f32,
}

/// Successful DEM build result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemBuildResult {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_warning: Option<String>,
    pub brick_count: usize,
    pub dem_width: u32,
    pub dem_height: u32,
    pub elevation_min_m: f32,
    pub elevation_max_m: f32,
}

fn map_dem_source(s: DemSourceDto) -> DemSource {
    match s {
        DemSourceDto::AwsTerrarium => DemSource::AwsTerrarium,
        DemSourceDto::MapboxTerrainRgb => DemSource::MapboxTerrainRgb,
        DemSourceDto::OpenTopography => DemSource::OpenTopography,
        DemSourceDto::OpenTopographyCop30 => DemSource::OpenTopographyCop30,
        DemSourceDto::Usgs3Dep => DemSource::Usgs3Dep,
    }
}

fn map_block_type(m: BrickModeDto) -> BlockType {
    match m {
        BrickModeDto::Default => BlockType::Brick,
        BrickModeDto::Tile => BlockType::Tile,
        BrickModeDto::SmoothTile => BlockType::SmoothTile,
        BrickModeDto::Stud => BlockType::Studded,
        BrickModeDto::Micro => BlockType::Micro,
    }
}

fn nonempty(s: Option<String>) -> Option<String> {
    s.and_then(|t| {
        let t = t.trim().to_owned();
        if t.is_empty() { None } else { Some(t) }
    })
}

fn resolve_tokens(req: &DemBuildRequest) -> (Option<String>, Option<String>) {
    let mut mapbox = nonempty(req.mapbox_token.clone());
    let mut opentopo = nonempty(req.opentopo_key.clone());
    if mapbox.is_none() || opentopo.is_none() {
        if let Ok(cfg) = Config::load() {
            if mapbox.is_none() {
                mapbox = nonempty(cfg.mapbox_token);
            }
            if opentopo.is_none() {
                opentopo = nonempty(cfg.opentopo_api_key);
            }
        }
    }
    (mapbox, opentopo)
}

/// Fetch DEM for the bbox, mesh, write `.brdb` under builds_dir, optional install.
///
/// `progress` receives phase labels compatible with egui Map-tab stages.
/// `cancel` is polled on each progress tick (and yields [`BuildError::Cancelled`]
/// via the shared atomic when true).
pub fn dem_fetch_build(
    req: DemBuildRequest,
    progress: impl Fn(DemBuildProgress) + Send + Sync + 'static,
    cancel: impl Fn() -> bool + Send + Sync + 'static,
) -> Result<DemBuildResult, String> {
    if !(req.north > req.south && req.east >= req.west) {
        return Err("bbox must have north>south and east>=west".into());
    }

    let dem_source = map_dem_source(req.dem_source);
    let block_type = map_block_type(req.brick_mode);
    let density = req.density_factor.max(1);
    let studs = req.studs_per_meter.clamp(0.5, 32.0);
    let exag = req.vertical_exaggeration.clamp(0.25, 8.0);

    // Predict cell size the same way the Map shell does, then derive brick scales.
    let pred = predict_dem_cells(DemPredictRequest {
        north: req.north,
        south: req.south,
        east: req.east,
        west: req.west,
        dem_source: req.dem_source,
        density_factor: density,
    })?;
    let cell_m_eff = pred.cell_m_eff.max(1e-6);
    let (horizontal_scale, vertical_scale) =
        crate::gui::scale::derive_scale(cell_m_eff, studs, exag, block_type.micro());

    let (mapbox_token, opentopo_key) = resolve_tokens(&req);

    let request = BuildRequest {
        bbox: BBoxLatLon {
            north: req.north,
            south: req.south,
            east: req.east,
            west: req.west,
        },
        name: req.output_name.clone(),
        dem_source,
        // MVP: flat colormap (imagery picker is BWT-3.6+ / egui-only for now).
        imagery_source: ImagerySource::None,
        mapbox_token,
        opentopo_key,
        vertical_scale,
        density_factor: density,
        horizontal_scale,
        block_type,
        glow: req.glow,
        no_collision: req.no_collision,
        install_to_brickadia: req.install,
        overwrite_world: req.overwrite,
        omit_below_m: req.omit_below_m.max(0.0),
        floor_level_m: req.floor_level_m.max(0.0),
    };

    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_cb = Arc::new(cancel);
    let progress_cb = Arc::new(progress);
    let cf = Arc::clone(&cancel_flag);
    let progress_fn: ProgressFn = Arc::new(move |stage: BuildStage, frac: f32| {
        if cancel_cb() {
            cf.store(true, Ordering::Relaxed);
        }
        progress_cb(DemBuildProgress {
            phase: stage.label().to_string(),
            frac,
        });
    });

    let outcome: BuildOutcome =
        build::run_build(request, progress_fn, cancel_flag).map_err(|e| e.to_string())?;

    Ok(DemBuildResult {
        path: outcome.brdb_path,
        installed_path: outcome.installed_path,
        install_warning: outcome.install_warning,
        brick_count: outcome.brick_count,
        dem_width: outcome.dem_width,
        dem_height: outcome.dem_height,
        elevation_min_m: outcome.elevation_min_m,
        elevation_max_m: outcome.elevation_max_m,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_scale_matches_true_1to1_spirit() {
        // ~3.63 m cells, 4 studs/m, no micro → same spirit as map_tab unit test.
        let (hs, v) = crate::gui::scale::derive_scale(3.63, 4.0, 1.0, false);
        assert!(hs >= 1);
        let horiz_units_per_m = 2.0 * f64::from(hs) * 5.0 / 3.63;
        assert!(
            (f64::from(v) - horiz_units_per_m).abs() < 0.5,
            "v={v} horiz={horiz_units_per_m}"
        );
    }

    #[test]
    fn rejects_bad_bbox() {
        let err = dem_fetch_build(
            DemBuildRequest {
                north: 40.0,
                south: 40.1,
                east: -105.0,
                west: -105.1,
                dem_source: DemSourceDto::AwsTerrarium,
                density_factor: 1,
                studs_per_meter: 4.0,
                vertical_exaggeration: 1.0,
                output_name: "x".into(),
                install: false,
                overwrite: false,
                mapbox_token: None,
                opentopo_key: None,
                brick_mode: BrickModeDto::Tile,
                glow: false,
                no_collision: false,
                omit_below_m: 0.0,
                floor_level_m: 0.0,
            },
            |_| {},
            || false,
        )
        .unwrap_err();
        assert!(err.contains("bbox"), "{err}");
    }
}
