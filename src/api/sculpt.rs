//! Sculpt session API for UI shells (Tauri Phase 4 MVP).
//!
//! Authoritative heightfield lives in Rust. Frontend sends strokes and receives
//! greyscale previews; export meshes via the Map/Sculpt convert pipeline
//! (`to_dem_raster` → `build_heightmap` → greedy bricks → `.brdb`).
//!
//! Requires `feature = "dem"`. Does not pull egui.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use serde::{Deserialize, Serialize};

use crate::gui::build::{
    BlockType, BrickStyle, BuildStage, FlatColormap, ProgressFn, build_heightmap, cell_size_units,
    generate_bricks_skip_floor, sanitize_name,
};
use crate::gui::sculpt::{
    Brush, BrushShape, Falloff, FieldMeta, Flatten, HeightField, Lower, Raise, SetHeight, Smooth,
    Tool, FLOOR_M,
};
use crate::opt::MAX_BRICK_UNITS;
use crate::util::{bricks_to_save, write_save_world};

/// Matches Map/dem_build horizontal scale clamp spirit.
const MAX_HORIZONTAL_SCALE: u16 = 128;
/// Blank / load cell budget (same order as DEM budget; keep MVP interactive).
const MAX_SCULPT_CELLS: u64 = 400_000;
const DEFAULT_CELL_M: f64 = 1.0;
const DEFAULT_STUDS: f32 = 4.0;

// ── DTOs ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SculptToolDto {
    #[default]
    Raise,
    Lower,
    Smooth,
    Flatten,
    Set,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SculptCreateBlankRequest {
    pub width: u32,
    pub height: u32,
    #[serde(default = "default_cell_m")]
    pub cell_m: f64,
    #[serde(default = "default_studs")]
    pub studs_per_meter: f32,
    #[serde(default = "default_exag")]
    pub vertical_exaggeration: f32,
    #[serde(default)]
    pub micro: bool,
    #[serde(default = "default_name")]
    pub source_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SculptLoadPngRequest {
    pub path: PathBuf,
    #[serde(default = "default_cell_m")]
    pub cell_m: f64,
    #[serde(default = "default_studs")]
    pub studs_per_meter: f32,
    #[serde(default = "default_exag")]
    pub vertical_exaggeration: f32,
    #[serde(default)]
    pub micro: bool,
    /// Override stem; empty → file stem.
    #[serde(default)]
    pub source_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SculptStrokeRequest {
    pub session_id: u64,
    pub tool: SculptToolDto,
    /// Fractional cell center (field coords).
    pub center_x: f32,
    pub center_y: f32,
    #[serde(default = "default_radius")]
    pub radius_cells: f32,
    #[serde(default = "default_strength")]
    pub strength: f32,
    /// Target height (m) for Flatten / Set. Ignored by Raise/Lower/Smooth.
    #[serde(default)]
    pub target_m: f32,
    /// When true, snapshot field for 1-level undo before this dab (stroke start).
    #[serde(default)]
    pub begin_stroke: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SculptExportRequest {
    pub session_id: u64,
    /// Optional full path ending in `.brdb` / `.brz`. Empty → builds_dir/stem.brdb.
    #[serde(default)]
    pub out_file: Option<PathBuf>,
    #[serde(default)]
    pub install: bool,
    #[serde(default)]
    pub overwrite: bool,
    #[serde(default)]
    pub micro: Option<bool>,
    #[serde(default)]
    pub studs_per_meter: Option<f32>,
    #[serde(default)]
    pub vertical_exaggeration: Option<f32>,
    #[serde(default)]
    pub cell_m: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SculptSessionInfo {
    pub session_id: u64,
    pub width: u32,
    pub height: u32,
    pub min_m: f32,
    pub max_m: f32,
    pub cell_m: f64,
    pub studs_per_meter: f32,
    pub vertical_exaggeration: f32,
    pub micro: bool,
    pub source_name: String,
}

/// Greyscale height preview: `gray[y * width + x]` is 0..=255 (min→max).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SculptPreview {
    pub width: u32,
    pub height: u32,
    pub min_m: f32,
    pub max_m: f32,
    pub gray: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SculptExportResult {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SculptProgress {
    pub phase: String,
    pub frac: f32,
}

fn default_cell_m() -> f64 {
    DEFAULT_CELL_M
}
fn default_studs() -> f32 {
    DEFAULT_STUDS
}
fn default_exag() -> f32 {
    1.0
}
fn default_name() -> String {
    "sculpt".into()
}
fn default_radius() -> f32 {
    8.0
}
fn default_strength() -> f32 {
    2.0
}

// ── Session store ───────────────────────────────────────────────────────────

struct Session {
    field: HeightField,
    /// Previous cells for one-level undo (None if nothing to undo).
    undo_cells: Option<Vec<f32>>,
}

struct Store {
    next_id: u64,
    sessions: HashMap<u64, Session>,
}

static STORE: LazyLock<Mutex<Store>> = LazyLock::new(|| {
    Mutex::new(Store {
        next_id: 1,
        sessions: HashMap::new(),
    })
});

fn with_store<R>(f: impl FnOnce(&mut Store) -> Result<R, String>) -> Result<R, String> {
    let mut g = STORE
        .lock()
        .map_err(|_| "sculpt session store poisoned".to_string())?;
    f(&mut g)
}

fn meta_from(
    cell_m: f64,
    studs: f32,
    exag: f32,
    micro: bool,
    source_name: String,
) -> FieldMeta {
    FieldMeta {
        cell_m: cell_m.max(1e-6),
        studs_per_meter: studs.clamp(0.5, 32.0),
        vertical_exaggeration: exag.clamp(0.25, 8.0),
        micro,
        centroid_lat: 0.0,
        source_name: if source_name.trim().is_empty() {
            "sculpt".into()
        } else {
            source_name
        },
    }
}

fn check_dims(w: u32, h: u32) -> Result<(), String> {
    if w == 0 || h == 0 {
        return Err("width and height must be ≥ 1".into());
    }
    let cells = u64::from(w).saturating_mul(u64::from(h));
    if cells > MAX_SCULPT_CELLS {
        return Err(format!(
            "field {w}×{h} = {cells} cells exceeds sculpt budget {MAX_SCULPT_CELLS}"
        ));
    }
    Ok(())
}

fn insert_session(store: &mut Store, field: HeightField) -> SculptSessionInfo {
    let id = store.next_id;
    store.next_id = store.next_id.saturating_add(1);
    let info = session_info(id, &field);
    store.sessions.insert(
        id,
        Session {
            field,
            undo_cells: None,
        },
    );
    info
}

fn session_info(id: u64, field: &HeightField) -> SculptSessionInfo {
    let (min_m, max_m) = field.min_max();
    SculptSessionInfo {
        session_id: id,
        width: field.width,
        height: field.height,
        min_m,
        max_m,
        cell_m: field.meta.cell_m,
        studs_per_meter: field.meta.studs_per_meter,
        vertical_exaggeration: field.meta.vertical_exaggeration,
        micro: field.meta.micro,
        source_name: field.meta.source_name.clone(),
    }
}

fn get_mut(store: &mut Store, id: u64) -> Result<&mut Session, String> {
    store
        .sessions
        .get_mut(&id)
        .ok_or_else(|| format!("unknown sculpt session {id}"))
}

// ── Public commands ─────────────────────────────────────────────────────────

/// Create a flat (floor) heightfield session.
pub fn sculpt_create_blank(req: SculptCreateBlankRequest) -> Result<SculptSessionInfo, String> {
    check_dims(req.width, req.height)?;
    let meta = meta_from(
        req.cell_m,
        req.studs_per_meter,
        req.vertical_exaggeration,
        req.micro,
        req.source_name,
    );
    let field = HeightField::flat(req.width, req.height, meta);
    with_store(|s| Ok(insert_session(s, field)))
}

/// Load a heightmap PNG into a new session.
///
/// 4-channel RGBA → packed Stage-1 / sculpt export decoder; otherwise luminance
/// meters (same branch as the egui sculpt importer).
pub fn sculpt_load_png(req: SculptLoadPngRequest) -> Result<SculptSessionInfo, String> {
    let dynimg = image::ImageReader::open(&req.path)
        .map_err(|e| format!("open {}: {e}", req.path.display()))?
        .decode()
        .map_err(|e| format!("decode {}: {e}", req.path.display()))?;
    if dynimg.width() == 0 || dynimg.height() == 0 {
        return Err("image has zero dimensions".into());
    }
    check_dims(dynimg.width(), dynimg.height())?;

    let stem = req
        .source_name
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            req.path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "sculpt".into());
    let meta = meta_from(
        req.cell_m,
        req.studs_per_meter,
        req.vertical_exaggeration,
        req.micro,
        stem,
    );

    let field = if matches!(
        dynimg.color(),
        image::ColorType::Rgba8 | image::ColorType::Rgba16
    ) {
        HeightField::from_heightmap_png(&dynimg.to_rgba8(), meta)
    } else {
        HeightField::from_image(&dynimg.to_luma8(), meta)
    };
    with_store(|s| Ok(insert_session(s, field)))
}

/// Drop a session (optional; process exit also clears).
pub fn sculpt_close(session_id: u64) -> Result<(), String> {
    with_store(|s| {
        s.sessions.remove(&session_id);
        Ok(())
    })
}

/// Session metadata without full preview payload.
pub fn sculpt_info(session_id: u64) -> Result<SculptSessionInfo, String> {
    with_store(|s| {
        let sess = get_mut(s, session_id)?;
        Ok(session_info(session_id, &sess.field))
    })
}

/// Greyscale preview of the heightfield (min→0, max→255; flat field → 0).
pub fn sculpt_preview(session_id: u64) -> Result<SculptPreview, String> {
    with_store(|s| {
        let sess = get_mut(s, session_id)?;
        let f = &sess.field;
        let (min_m, max_m) = f.min_max();
        let span = (max_m - min_m).max(1e-6);
        let mut gray = Vec::with_capacity(f.cells.len());
        for &c in &f.cells {
            let t = ((c - min_m) / span).clamp(0.0, 1.0);
            gray.push((t * 255.0).round() as u8);
        }
        Ok(SculptPreview {
            width: f.width,
            height: f.height,
            min_m,
            max_m,
            gray,
        })
    })
}

/// Apply one brush dab. Returns updated session info (cheap) for UI readouts.
pub fn sculpt_apply_stroke(req: SculptStrokeRequest) -> Result<SculptSessionInfo, String> {
    if !(req.radius_cells.is_finite() && req.radius_cells > 0.0) {
        return Err("radius_cells must be finite and > 0".into());
    }
    if !req.strength.is_finite() {
        return Err("strength must be finite".into());
    }
    if !req.center_x.is_finite() || !req.center_y.is_finite() {
        return Err("center must be finite".into());
    }

    with_store(|s| {
        let sess = get_mut(s, req.session_id)?;
        if req.begin_stroke {
            sess.undo_cells = Some(sess.field.cells.clone());
        }

        let brush = Brush {
            shape: BrushShape::Circle,
            radius_cells: req.radius_cells,
            strength: req.strength.max(0.0),
            falloff: Falloff::Smoothstep,
        };
        let center = (req.center_x, req.center_y);
        match req.tool {
            SculptToolDto::Raise => Raise.apply(&mut sess.field, &brush, center),
            SculptToolDto::Lower => Lower.apply(&mut sess.field, &brush, center),
            SculptToolDto::Smooth => {
                // Smooth strength is blend 0..=1; clamp for safety.
                let mut b = brush;
                b.strength = brush.strength.clamp(0.0, 1.0);
                Smooth.apply(&mut sess.field, &b, center);
            }
            SculptToolDto::Flatten => {
                let mut b = brush;
                b.strength = brush.strength.clamp(0.0, 1.0);
                Flatten {
                    target: req.target_m.max(FLOOR_M),
                }
                .apply(&mut sess.field, &b, center);
            }
            SculptToolDto::Set => {
                let mut b = brush;
                b.strength = brush.strength.clamp(0.0, 1.0);
                SetHeight {
                    target: req.target_m.max(FLOOR_M),
                }
                .apply(&mut sess.field, &b, center);
            }
        }
        Ok(session_info(req.session_id, &sess.field))
    })
}

/// Restore the last `begin_stroke` snapshot (one level).
pub fn sculpt_undo(session_id: u64) -> Result<SculptSessionInfo, String> {
    with_store(|s| {
        let sess = get_mut(s, session_id)?;
        let Some(prev) = sess.undo_cells.take() else {
            return Err("nothing to undo".into());
        };
        if prev.len() != sess.field.cells.len() {
            return Err("undo buffer size mismatch".into());
        }
        sess.field.cells = prev;
        Ok(session_info(session_id, &sess.field))
    })
}

/// Same math as Map-tab / dem_build `derive_scale` — keep in lockstep.
fn derive_scale(
    cell_m_eff: f64,
    studs_per_meter: f32,
    exaggeration: f32,
    micro: bool,
) -> (u16, f32) {
    let cell_m_eff = cell_m_eff.max(1e-6);
    let upf = if micro { 1.0 } else { 5.0 };
    let max_hscale = f64::from(MAX_HORIZONTAL_SCALE) * 5.0 / upf;
    let hscale = ((f64::from(studs_per_meter) * 5.0 * cell_m_eff) / (2.0 * upf))
        .round()
        .clamp(1.0, max_hscale) as u16;
    let vertical =
        ((2.0 * f64::from(hscale) * upf / cell_m_eff) * f64::from(exaggeration)) as f32;
    (hscale, vertical)
}

/// Mesh the session field and write `.brdb` (optional install).
pub fn sculpt_export(
    req: SculptExportRequest,
    progress: impl Fn(SculptProgress) + Send + Sync + 'static,
    cancel: impl Fn() -> bool + Send + Sync + 'static,
) -> Result<SculptExportResult, String> {
    // Clone field out of the lock so meshing does not hold the store.
    let mut field = with_store(|s| {
        let sess = get_mut(s, req.session_id)?;
        Ok(sess.field.clone())
    })?;

    if let Some(v) = req.micro {
        field.meta.micro = v;
    }
    if let Some(v) = req.studs_per_meter {
        field.meta.studs_per_meter = v.clamp(0.5, 32.0);
    }
    if let Some(v) = req.vertical_exaggeration {
        field.meta.vertical_exaggeration = v.clamp(0.25, 8.0);
    }
    if let Some(v) = req.cell_m {
        field.meta.cell_m = v.max(1e-6);
    }

    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_flag_tick = Arc::clone(&cancel_flag);
    let progress: ProgressFn = Arc::new(move |stage: BuildStage, frac: f32| {
        if cancel() {
            cancel_flag_tick.store(true, Ordering::Relaxed);
        }
        progress(SculptProgress {
            phase: stage.label().to_string(),
            frac,
        });
    });

    export_heightfield(&field, &req, progress, cancel_flag)
}

fn export_heightfield(
    field: &HeightField,
    req: &SculptExportRequest,
    progress: ProgressFn,
    cancel: Arc<AtomicBool>,
) -> Result<SculptExportResult, String> {
    let (elevation_min_m, elevation_max_m) = field.min_max();
    let dem_width = field.width;
    let dem_height = field.height;

    let (horizontal_scale, vertical_scale) = derive_scale(
        field.meta.cell_m,
        field.meta.studs_per_meter,
        field.meta.vertical_exaggeration,
        field.meta.micro,
    );
    let block_type = if field.meta.micro {
        BlockType::Micro
    } else {
        BlockType::SmoothTile
    };
    let style = BrickStyle::new(block_type, horizontal_scale, false, false);
    let size = i32::from(cell_size_units(horizontal_scale, field.meta.micro));
    let offset = (-(dem_width as i32 * size), -(dem_height as i32 * size));

    progress(BuildStage::GeneratingBricks, 0.0);
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }

    let raster = field.to_dem_raster();
    let heightmap = build_heightmap(&raster, vertical_scale, 0.0);
    let flat = FlatColormap::sculpt_default(dem_width, dem_height);
    // skip_floor=true: blank/floor cells reveal native Brickadia ground (sculpt).
    let bricks = generate_bricks_skip_floor(
        &heightmap,
        &flat,
        style,
        Some(0),
        offset,
        true,
        0,
        MAX_BRICK_UNITS,
        Arc::clone(&progress),
        Arc::clone(&cancel),
        None,
    )
    .map_err(|e| format!("brick gen: {e:?}"))?;
    let brick_count = bricks.len();

    progress(BuildStage::WritingSave, 0.0);
    let world = bricks_to_save(bricks);

    let out_path = resolve_out_path(req, &field.meta.source_name)?;
    let path_str = out_path
        .to_str()
        .ok_or_else(|| format!("non-UTF8 out path: {}", out_path.display()))?;
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    // brdb is append-open; delete stale same-name first (mirrors build::write_brdb).
    if out_path.exists() {
        std::fs::remove_file(&out_path)
            .map_err(|e| format!("remove stale {}: {e}", out_path.display()))?;
    }
    write_save_world(&world, path_str).map_err(|e| format!("write save: {e}"))?;
    progress(BuildStage::WritingSave, 1.0);

    let mut installed_path = None;
    let mut install_warning = None;
    if req.install {
        progress(BuildStage::Installing, 0.0);
        match crate::api::install::install_save(&out_path, req.overwrite) {
            Ok(p) => installed_path = Some(p),
            Err(e) => install_warning = Some(e),
        }
        progress(BuildStage::Installing, 1.0);
    }

    Ok(SculptExportResult {
        path: out_path,
        installed_path,
        install_warning,
        brick_count,
        dem_width,
        dem_height,
        elevation_min_m,
        elevation_max_m,
    })
}

fn resolve_out_path(req: &SculptExportRequest, source_name: &str) -> Result<PathBuf, String> {
    if let Some(p) = req.out_file.as_ref() {
        if p.as_os_str().is_empty() {
            // fall through
        } else {
            let s = p.to_string_lossy();
            if !(s.ends_with(".brdb") || s.ends_with(".brz")) {
                return Err("out_file must end with .brdb or .brz".into());
            }
            return Ok(p.clone());
        }
    }
    let builds = crate::api::install::builds_dir().map_err(|e| e.to_string())?;
    let stem = sanitize_name(source_name);
    Ok(builds.join(format!("{stem}.brdb")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn unique_name(prefix: &str) -> String {
        format!(
            "{prefix}_{}_{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[test]
    fn blank_raise_preview_export() {
        let name = unique_name("sculpt_api");
        let info = sculpt_create_blank(SculptCreateBlankRequest {
            width: 32,
            height: 32,
            cell_m: 1.0,
            studs_per_meter: 4.0,
            vertical_exaggeration: 1.0,
            micro: false,
            source_name: name.clone(),
        })
        .expect("create blank");
        assert_eq!((info.width, info.height), (32, 32));
        assert_eq!(info.min_m, FLOOR_M);

        let id = info.session_id;
        sculpt_apply_stroke(SculptStrokeRequest {
            session_id: id,
            tool: SculptToolDto::Raise,
            center_x: 16.0,
            center_y: 16.0,
            radius_cells: 6.0,
            strength: 10.0,
            target_m: 0.0,
            begin_stroke: true,
        })
        .expect("raise");

        let prev = sculpt_preview(id).expect("preview");
        assert_eq!(prev.gray.len(), 32 * 32);
        assert!(prev.max_m > prev.min_m, "raise must lift max");
        assert!(prev.gray.iter().any(|&g| g > 0), "preview not all black");

        let out = std::env::temp_dir().join(format!("{name}.brdb"));
        let result = sculpt_export(
            SculptExportRequest {
                session_id: id,
                out_file: Some(out.clone()),
                install: false,
                overwrite: false,
                micro: None,
                studs_per_meter: None,
                vertical_exaggeration: None,
                cell_m: None,
            },
            |_| {},
            || false,
        )
        .expect("export");
        assert!(result.path.exists(), "brdb written");
        assert!(result.brick_count > 0, "raised terrain emits bricks");
        let _ = std::fs::remove_file(&out);
        let _ = sculpt_close(id);
    }

    #[test]
    fn undo_restores_after_stroke() {
        let info = sculpt_create_blank(SculptCreateBlankRequest {
            width: 16,
            height: 16,
            cell_m: 1.0,
            studs_per_meter: 4.0,
            vertical_exaggeration: 1.0,
            micro: false,
            source_name: unique_name("undo"),
        })
        .unwrap();
        let id = info.session_id;
        sculpt_apply_stroke(SculptStrokeRequest {
            session_id: id,
            tool: SculptToolDto::Raise,
            center_x: 8.0,
            center_y: 8.0,
            radius_cells: 4.0,
            strength: 5.0,
            target_m: 0.0,
            begin_stroke: true,
        })
        .unwrap();
        assert!(sculpt_info(id).unwrap().max_m > 0.0);
        sculpt_undo(id).unwrap();
        assert_eq!(sculpt_info(id).unwrap().max_m, FLOOR_M);
        let _ = sculpt_close(id);
    }

    #[test]
    fn load_luma_png() {
        let path = std::env::temp_dir().join(format!(
            "sculpt_luma_{}_{}.png",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let img = image::GrayImage::from_fn(8, 8, |x, y| image::Luma([((x + y) * 10) as u8]));
        img.save(&path).unwrap();
        let info = sculpt_load_png(SculptLoadPngRequest {
            path: path.clone(),
            cell_m: 2.0,
            studs_per_meter: 4.0,
            vertical_exaggeration: 1.0,
            micro: false,
            source_name: None,
        })
        .expect("load luma");
        assert_eq!((info.width, info.height), (8, 8));
        assert!(info.max_m > 0.0);
        let _ = std::fs::remove_file(path);
        let _ = sculpt_close(info.session_id);
    }
}
