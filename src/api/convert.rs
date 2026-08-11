//! Convert-tab pipeline: heightmap (+ optional colormap) → `.brdb` / `.brz`.
//!
//! Extracted from the egui worker so Tauri (and CLI wrappers) share one path.

use std::cell::RefCell;
use std::path::PathBuf;

use brdb::assets::bricks::{
    PB_DEFAULT_BRICK, PB_DEFAULT_MICRO_BRICK, PB_DEFAULT_SMOOTH_TILE, PB_DEFAULT_STUDDED,
    PB_DEFAULT_TILE,
};
use brdb::BString;
use serde::{Deserialize, Serialize};

use crate::opt::{gen_opt_heightmap, MAX_BRICK_UNITS};
use crate::util::{maps_from_files, write_save, GenOptions};

/// Brick surface type exposed to UI shells (maps to brdb procedural assets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BrickModeDto {
    #[default]
    Default,
    Tile,
    SmoothTile,
    Stud,
    Micro,
}

impl BrickModeDto {
    fn asset(self) -> BString {
        match self {
            Self::Default => PB_DEFAULT_BRICK,
            Self::Tile => PB_DEFAULT_TILE,
            Self::SmoothTile => PB_DEFAULT_SMOOTH_TILE,
            Self::Stud => PB_DEFAULT_STUDDED,
            Self::Micro => PB_DEFAULT_MICRO_BRICK,
        }
    }

    fn micro(self) -> bool {
        matches!(self, Self::Micro)
    }

    fn stud(self) -> bool {
        matches!(self, Self::Stud)
    }
}

/// Serializable convert request (Tauri command args / future HTTP).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConvertRequest {
    pub heightmaps: Vec<PathBuf>,
    pub colormap: Option<PathBuf>,
    /// Output path; must end with `.brdb` or `.brz`.
    pub out_file: PathBuf,
    #[serde(default)]
    pub brick_mode: BrickModeDto,
    /// Stud size per cell for micro; non-micro multiplies by 5 (stud→unit).
    #[serde(default = "default_horizontal_size")]
    pub horizontal_size: u16,
    #[serde(default = "default_vertical_scale")]
    pub vertical_scale: u32,
    #[serde(default)]
    pub greedy: bool,
    #[serde(default)]
    pub quadtree: bool,
    #[serde(default)]
    pub cull: bool,
    #[serde(default)]
    pub nocollide: bool,
    #[serde(default)]
    pub glow: bool,
    /// RGB-encoded high-detail heightmap (Stage-1 / sculpt export PNGs).
    #[serde(default)]
    pub hdmap: bool,
    #[serde(default)]
    pub lrgb: bool,
    #[serde(default)]
    pub snap: bool,
    /// After a successful write, copy into Brickadia Worlds/Prefabs (soft-fail).
    #[serde(default)]
    pub install: bool,
    /// When installing, overwrite `<stem>.<ext>` instead of suffixing `-2`, …
    #[serde(default)]
    pub overwrite: bool,
}

fn default_horizontal_size() -> u16 {
    1
}

fn default_vertical_scale() -> u32 {
    1
}

/// Progress snapshot for UI shells.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConvertProgress {
    pub phase: String,
    /// 0.0..=1.0 during work; shells may use >1 as a "done" sentinel.
    pub frac: f32,
}

/// Successful convert result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConvertResult {
    pub out_file: PathBuf,
    pub absolute_path: PathBuf,
    /// Present when `install` succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_path: Option<PathBuf>,
    /// Soft-fail message when convert wrote but install did not (e.g. no Proton prefix).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_warning: Option<String>,
}

impl ConvertRequest {
    pub fn to_gen_options(&self) -> GenOptions {
        let micro = self.brick_mode.micro();
        let size = if micro {
            self.horizontal_size
        } else {
            self.horizontal_size.saturating_mul(5)
        };
        GenOptions {
            size,
            scale: self.vertical_scale,
            cull: self.cull,
            asset: self.brick_mode.asset(),
            micro,
            stud: self.brick_mode.stud(),
            snap: self.snap,
            // img2brick: colormap only, no heightmaps
            img: self.heightmaps.is_empty() && self.colormap.is_some(),
            glow: self.glow,
            hdmap: self.hdmap,
            lrgb: self.lrgb,
            nocollide: self.nocollide,
            quadtree: self.quadtree,
            greedy: self.greedy,
            fill_to_base: false,
            skip_floor: false,
            omit_below_h: 0,
            max_brick_units: MAX_BRICK_UNITS,
        }
    }
}

/// Run the full convert pipeline. `progress` is called at phase boundaries and
/// during meshing; `is_cancelled` may abort between phases (and inside the
/// mesher when it polls).
///
/// Progress uses `RefCell` because `gen_opt_heightmap` only accepts `Fn` (not
/// `FnMut`) for its progress callback.
pub fn convert_heightmap(
    request: ConvertRequest,
    progress: impl Fn(ConvertProgress),
    is_cancelled: impl Fn() -> bool,
) -> Result<ConvertResult, String> {
    let out_file = resolve_out_file(&request)?;
    let out_str = out_file
        .to_str()
        .ok_or_else(|| "out_file path is not valid UTF-8".to_string())?
        .to_string();

    if let Some(parent) = out_file.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create output directory {}: {e}", parent.display()))?;
        }
    }

    let progress = RefCell::new(progress);
    let report = |phase: &str, frac: f32| {
        progress.borrow()(ConvertProgress {
            phase: phase.into(),
            frac,
        });
    };

    report("Reading", 0.0);

    let options = request.to_gen_options();
    let (heightmap, colormap) =
        maps_from_files(&options, request.heightmaps.clone(), request.colormap.clone())?;

    if is_cancelled() {
        return Err("cancelled".into());
    }

    report("Generating", 0.10);

    let bricks = gen_opt_heightmap(
        &*heightmap,
        &*colormap,
        options,
        None,
        None,
        |p| {
            report("Generating", 0.1 + 0.85 * p);
            !is_cancelled()
        },
    )?;

    if is_cancelled() {
        return Err("cancelled".into());
    }

    report("Writing", 0.95);

    write_save(&out_str, bricks)?;

    if is_cancelled() {
        return Err("cancelled".into());
    }

    let absolute_path = out_file
        .canonicalize()
        .unwrap_or_else(|_| out_file.clone());

    let mut installed_path = None;
    let mut install_warning = None;
    if request.install {
        report("Installing", 0.98);
        match crate::api::install::install_save(&absolute_path, request.overwrite) {
            Ok(dest) => installed_path = Some(dest),
            Err(e) => {
                // Soft-fail: .brdb/.brz is already on disk.
                install_warning = Some(if crate::api::install::is_prefix_missing(&e) {
                    format!(
                        "{e} — import {} manually",
                        absolute_path.display()
                    )
                } else {
                    format!(
                        "install into Brickadia failed ({e}) — the save was still written to {}; import it manually",
                        absolute_path.display()
                    )
                });
            }
        }
    }

    report("Finished", 1.0);

    Ok(ConvertResult {
        out_file,
        absolute_path,
        installed_path,
        install_warning,
    })
}

/// Empty `out_file` → `builds_dir/<heightmap-stem>.brdb` (or `world.brdb`).
fn resolve_out_file(request: &ConvertRequest) -> Result<PathBuf, String> {
    if !request.out_file.as_os_str().is_empty() {
        return Ok(request.out_file.clone());
    }
    let stem = request
        .heightmaps
        .first()
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("world");
    let dir = crate::api::install::builds_dir()?;
    Ok(dir.join(format!("{stem}.brdb")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn convert_gradient_png_writes_brdb() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let hm = root.join("example_maps/gradient.png");
        assert!(hm.is_file(), "fixture missing: {}", hm.display());

        let out_dir = std::env::temp_dir().join("bwt_convert_api_test");
        let _ = std::fs::create_dir_all(&out_dir);
        let out = out_dir.join("gradient_api.brdb");
        let _ = std::fs::remove_file(&out);

        let req = ConvertRequest {
            heightmaps: vec![hm],
            colormap: None,
            out_file: out.clone(),
            brick_mode: BrickModeDto::Tile,
            horizontal_size: 1,
            vertical_scale: 1,
            greedy: true,
            quadtree: false,
            cull: false,
            nocollide: false,
            glow: false,
            hdmap: false,
            lrgb: false,
            snap: false,
            install: false,
            overwrite: false,
        };

        let result = convert_heightmap(req, |_| {}, || false).expect("convert");
        assert!(result.out_file.is_file() || out.is_file());
        assert!(result.installed_path.is_none());
        assert!(result.install_warning.is_none());
        let meta = std::fs::metadata(&out).expect("output exists");
        assert!(meta.len() > 100, "brdb should be non-trivial, got {} bytes", meta.len());
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn brick_mode_micro_size_not_multiplied_by_five() {
        let req = ConvertRequest {
            heightmaps: vec![],
            colormap: None,
            out_file: "x.brdb".into(),
            brick_mode: BrickModeDto::Micro,
            horizontal_size: 2,
            vertical_scale: 1,
            greedy: false,
            quadtree: false,
            cull: false,
            nocollide: false,
            glow: false,
            hdmap: false,
            lrgb: false,
            snap: false,
            install: false,
            overwrite: false,
        };
        assert_eq!(req.to_gen_options().size, 2);
        let req2 = ConvertRequest {
            brick_mode: BrickModeDto::Default,
            horizontal_size: 2,
            ..req
        };
        assert_eq!(req2.to_gen_options().size, 10);
    }

    #[test]
    fn empty_out_file_defaults_under_builds_dir() {
        let req = ConvertRequest {
            heightmaps: vec!["/tmp/gradient.png".into()],
            colormap: None,
            out_file: PathBuf::new(),
            brick_mode: BrickModeDto::Tile,
            horizontal_size: 1,
            vertical_scale: 1,
            greedy: false,
            quadtree: false,
            cull: false,
            nocollide: false,
            glow: false,
            hdmap: false,
            lrgb: false,
            snap: false,
            install: false,
            overwrite: false,
        };
        let out = resolve_out_file(&req).expect("resolve");
        assert!(out.ends_with("gradient.brdb"), "{}", out.display());
    }
}
