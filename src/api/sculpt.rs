//! Sculpt session API for UI shells (Tauri Phase 4 + BWT-4.5 parity).
//!
//! Authoritative heightfield (+ paint grid, zones, layer stack) lives in Rust.
//! Frontend sends strokes and receives previews; export meshes via the Map/Sculpt
//! convert pipeline (`to_dem_raster` → `build_heightmap` → greedy bricks → `.brdb`).
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
use crate::gui::grid::{offset_fits_chunk_index, tile_world_offset};
use crate::gui::sculpt::{
    default_palette, shape_distance, Brush, BrushShape, Falloff, FieldMeta, Flatten, HeightField,
    LayerStack, Lower, PaintColormap, PaintGrid, Raise, SetHeight, Smooth, Stamp, StampKind, Tool,
    FLOOR_M,
};
use crate::gui::zones::{self, Zone, ZoneMode};
use crate::map::Colormap;
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
    /// One-shot parametric landform (cone/mesa/crater/ramp). Params: peak_m, stamp_*.
    Stamp,
    /// Palette-index splat (color only). Params: paint_index, paint_res.
    Paint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StampKindDto {
    #[default]
    Cone,
    Mesa,
    Crater,
    Ramp,
}

impl From<StampKindDto> for StampKind {
    fn from(k: StampKindDto) -> Self {
        match k {
            StampKindDto::Cone => StampKind::Cone,
            StampKindDto::Mesa => StampKind::Mesa,
            StampKindDto::Crater => StampKind::Crater,
            StampKindDto::Ramp => StampKind::Ramp,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ZoneModeDto {
    #[default]
    Omit,
    Include,
}

impl From<ZoneModeDto> for ZoneMode {
    fn from(m: ZoneModeDto) -> Self {
        match m {
            ZoneModeDto::Omit => ZoneMode::Omit,
            ZoneModeDto::Include => ZoneMode::Include,
        }
    }
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
    /// When true, snapshot field (+ paint) for 1-level undo before this dab.
    #[serde(default)]
    pub begin_stroke: bool,
    // ── Stamp ──────────────────────────────────────────────────────────────
    #[serde(default)]
    pub stamp_kind: StampKindDto,
    /// Peak height (m) deposited by Stamp; negative digs toward floor.
    #[serde(default = "default_peak_m")]
    pub peak_m: f32,
    /// Mesa/Crater inner ratio (0.05..=0.95).
    #[serde(default = "default_inner_ratio")]
    pub inner_ratio: f32,
    /// Ramp direction degrees (0 = +X).
    #[serde(default)]
    pub angle_deg: f32,
    // ── Paint ──────────────────────────────────────────────────────────────
    /// Palette index written by Paint (0 = unpainted / erase).
    #[serde(default = "default_paint_index")]
    pub paint_index: u8,
    /// Splat block resolution (1 = per-cell). Larger → coarser color blocks.
    #[serde(default = "default_paint_res")]
    pub paint_res: u32,
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

/// Height + optional paint preview.
///
/// `gray[y * width + x]` is 0..=255 (min→max). `paint` is palette indices (same
/// length); empty when the paint grid is blank. `palette` is RGBA swatches.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SculptPreview {
    pub width: u32,
    pub height: u32,
    pub min_m: f32,
    pub max_m: f32,
    pub gray: Vec<u8>,
    #[serde(default)]
    pub paint: Vec<u8>,
    #[serde(default)]
    pub palette: Vec<[u8; 4]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SculptPaletteInfo {
    pub session_id: u64,
    /// RGBA swatches; index 0 = unpainted default brick color.
    pub palette: Vec<[u8; 4]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SculptZoneAddRectRequest {
    pub session_id: u64,
    pub mode: ZoneModeDto,
    /// Half-open cell rect [x0,x1) × [y0,y1) (integers ok as f32).
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SculptZonesInfo {
    pub session_id: u64,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SculptLayerInfo {
    pub id: u32,
    pub name: String,
    pub color: [u8; 4],
    pub visible: bool,
    pub selected_cells: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SculptLayersInfo {
    pub session_id: u64,
    pub active: usize,
    pub grid_cols: u32,
    pub grid_rows: u32,
    pub layers: Vec<SculptLayerInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SculptLayerBoxRequest {
    pub session_id: u64,
    /// Box indices in the layer grid (cols×rows).
    pub bi: u32,
    pub bj: u32,
    /// true = select cells under the box; false = clear.
    pub on: bool,
    /// If set, switch active layer before painting.
    #[serde(default)]
    pub layer_index: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SculptLayerPartResult {
    pub layer_name: String,
    pub path: PathBuf,
    pub brick_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SculptLayersExportResult {
    pub parts: Vec<SculptLayerPartResult>,
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
fn default_peak_m() -> f32 {
    40.0
}
fn default_inner_ratio() -> f32 {
    0.4
}
fn default_paint_index() -> u8 {
    1
}
fn default_paint_res() -> u32 {
    1
}

// ── Session store ───────────────────────────────────────────────────────────

struct UndoSnap {
    cells: Vec<f32>,
    paint: Vec<u8>,
    zones: Vec<Zone>,
}

struct Session {
    field: HeightField,
    paint: PaintGrid,
    palette: Vec<[u8; 4]>,
    layers: LayerStack,
    zones: Vec<Zone>,
    /// One-level undo (height + paint + zones).
    undo: Option<UndoSnap>,
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
    let (w, h) = (field.width, field.height);
    let info = session_info(id, &field);
    store.sessions.insert(
        id,
        Session {
            paint: PaintGrid::blank(w, h),
            palette: default_palette(),
            layers: LayerStack::new(w, h),
            zones: Vec::new(),
            field,
            undo: None,
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

/// Height (+ paint) preview of the session field.
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
        let (paint, palette) = if sess.paint.is_blank() {
            (Vec::new(), Vec::new())
        } else {
            (sess.paint.cells.clone(), sess.palette.clone())
        };
        Ok(SculptPreview {
            width: f.width,
            height: f.height,
            min_m,
            max_m,
            gray,
            paint,
            palette,
        })
    })
}

/// Current palette swatches for the paint tool UI.
pub fn sculpt_palette(session_id: u64) -> Result<SculptPaletteInfo, String> {
    with_store(|s| {
        let sess = get_mut(s, session_id)?;
        Ok(SculptPaletteInfo {
            session_id,
            palette: sess.palette.clone(),
        })
    })
}

fn snapshot_undo(sess: &mut Session) {
    sess.undo = Some(UndoSnap {
        cells: sess.field.cells.clone(),
        paint: sess.paint.cells.clone(),
        zones: sess.zones.clone(),
    });
}

/// Apply one brush dab / stamp / paint. Returns updated session info.
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
    if !req.peak_m.is_finite() {
        return Err("peak_m must be finite".into());
    }

    with_store(|s| {
        let sess = get_mut(s, req.session_id)?;
        if req.begin_stroke {
            snapshot_undo(sess);
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
            SculptToolDto::Stamp => {
                Stamp {
                    kind: req.stamp_kind.into(),
                    peak_m: req.peak_m,
                    inner_ratio: req.inner_ratio.clamp(0.05, 0.95),
                    angle: req.angle_deg.to_radians(),
                }
                .apply(&mut sess.field, &brush, center);
            }
            SculptToolDto::Paint => {
                apply_paint_dab(
                    &mut sess.paint,
                    center,
                    req.radius_cells,
                    req.paint_index,
                    req.paint_res.max(1),
                );
            }
        }
        Ok(session_info(req.session_id, &sess.field))
    })
}

/// Hard-write palette index into every cell inside a circular brush footprint.
fn apply_paint_dab(
    paint: &mut PaintGrid,
    center: (f32, f32),
    radius_cells: f32,
    idx: u8,
    res: u32,
) {
    let r = radius_cells;
    if r <= 0.0 || !r.is_finite() || paint.width == 0 || paint.height == 0 {
        return;
    }
    let (cx, cy) = center;
    let min_x = (cx - r).floor().max(0.0) as u32;
    let max_x = (cx + r).ceil().min((paint.width - 1) as f32) as u32;
    let min_y = (cy - r).floor().max(0.0) as u32;
    let max_y = (cy + r).ceil().min((paint.height - 1) as f32) as u32;
    // Guard empty when center is far off-grid.
    if (cx + r) < 0.0 || (cy + r) < 0.0 {
        return;
    }
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            if shape_distance(BrushShape::Circle, dx, dy, r) < 1.0 {
                paint.set_block(x, y, res, idx);
            }
        }
    }
}

/// Restore the last `begin_stroke` snapshot (one level).
pub fn sculpt_undo(session_id: u64) -> Result<SculptSessionInfo, String> {
    with_store(|s| {
        let sess = get_mut(s, session_id)?;
        let Some(prev) = sess.undo.take() else {
            return Err("nothing to undo".into());
        };
        if prev.cells.len() != sess.field.cells.len() {
            return Err("undo buffer size mismatch".into());
        }
        if prev.paint.len() != sess.paint.cells.len() {
            return Err("undo paint buffer size mismatch".into());
        }
        sess.field.cells = prev.cells;
        sess.paint.cells = prev.paint;
        sess.zones = prev.zones;
        Ok(session_info(session_id, &sess.field))
    })
}

// ── Zones ───────────────────────────────────────────────────────────────────

/// Add an axis-aligned rect zone (omit/include). Snapshots undo.
pub fn sculpt_zone_add_rect(req: SculptZoneAddRectRequest) -> Result<SculptZonesInfo, String> {
    if ![req.x0, req.y0, req.x1, req.y1]
        .iter()
        .all(|v| v.is_finite())
    {
        return Err("zone rect coords must be finite".into());
    }
    with_store(|s| {
        let sess = get_mut(s, req.session_id)?;
        let (x0, x1) = if req.x0 <= req.x1 {
            (req.x0, req.x1)
        } else {
            (req.x1, req.x0)
        };
        let (y0, y1) = if req.y0 <= req.y1 {
            (req.y0, req.y1)
        } else {
            (req.y1, req.y0)
        };
        if (x1 - x0) < 1e-3 || (y1 - y0) < 1e-3 {
            return Err("zone rect has zero area".into());
        }
        snapshot_undo(sess);
        sess.zones.push(Zone {
            mode: req.mode.into(),
            polygon: vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)],
        });
        Ok(SculptZonesInfo {
            session_id: req.session_id,
            count: sess.zones.len(),
        })
    })
}

/// Drop all zones (undoable).
pub fn sculpt_zone_clear(session_id: u64) -> Result<SculptZonesInfo, String> {
    with_store(|s| {
        let sess = get_mut(s, session_id)?;
        if !sess.zones.is_empty() {
            snapshot_undo(sess);
            sess.zones.clear();
        }
        Ok(SculptZonesInfo {
            session_id,
            count: 0,
        })
    })
}

pub fn sculpt_zones_info(session_id: u64) -> Result<SculptZonesInfo, String> {
    with_store(|s| {
        let sess = get_mut(s, session_id)?;
        Ok(SculptZonesInfo {
            session_id,
            count: sess.zones.len(),
        })
    })
}

// ── Layers ──────────────────────────────────────────────────────────────────

fn layers_info(session_id: u64, sess: &Session) -> SculptLayersInfo {
    let layers = sess
        .layers
        .layers
        .iter()
        .map(|l| SculptLayerInfo {
            id: l.id.0,
            name: l.name.clone(),
            color: l.color,
            visible: l.visible,
            selected_cells: l.box_mask.iter().filter(|&&b| b).count(),
        })
        .collect();
    SculptLayersInfo {
        session_id,
        active: sess.layers.active,
        grid_cols: sess.layers.grid_div.0,
        grid_rows: sess.layers.grid_div.1,
        layers,
    }
}

pub fn sculpt_layers_info(session_id: u64) -> Result<SculptLayersInfo, String> {
    with_store(|s| {
        let sess = get_mut(s, session_id)?;
        Ok(layers_info(session_id, sess))
    })
}

/// Add a new empty layer above the active one.
pub fn sculpt_layer_add(session_id: u64) -> Result<SculptLayersInfo, String> {
    with_store(|s| {
        let sess = get_mut(s, session_id)?;
        let (w, h) = (sess.field.width, sess.field.height);
        sess.layers.add_layer(w, h);
        Ok(layers_info(session_id, sess))
    })
}

/// Set active layer index.
pub fn sculpt_layer_set_active(session_id: u64, index: usize) -> Result<SculptLayersInfo, String> {
    with_store(|s| {
        let sess = get_mut(s, session_id)?;
        if index >= sess.layers.layers.len() {
            return Err(format!(
                "layer index {index} out of range ({} layers)",
                sess.layers.layers.len()
            ));
        }
        sess.layers.active = index;
        Ok(layers_info(session_id, sess))
    })
}

/// Toggle/set a grid box selection on the active (or specified) layer.
pub fn sculpt_layer_paint_box(req: SculptLayerBoxRequest) -> Result<SculptLayersInfo, String> {
    with_store(|s| {
        let sess = get_mut(s, req.session_id)?;
        if let Some(i) = req.layer_index {
            if i >= sess.layers.layers.len() {
                return Err(format!("layer index {i} out of range"));
            }
            sess.layers.active = i;
        }
        let (w, h) = (sess.field.width, sess.field.height);
        let (cols, rows) = (
            sess.layers.grid_div.0.max(1),
            sess.layers.grid_div.1.max(1),
        );
        if req.bi >= cols || req.bj >= rows {
            return Err(format!(
                "box ({},{}) out of grid {}×{}",
                req.bi, req.bj, cols, rows
            ));
        }
        sess.layers.paint_box(w, h, req.bi, req.bj, req.on);
        Ok(layers_info(req.session_id, sess))
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
///
/// Uses paint colormap when the splat grid is non-blank; applies zone keep-mask
/// when any zones exist.
pub fn sculpt_export(
    req: SculptExportRequest,
    progress: impl Fn(SculptProgress) + Send + Sync + 'static,
    cancel: impl Fn() -> bool + Send + Sync + 'static,
) -> Result<SculptExportResult, String> {
    // Clone out of the lock so meshing does not hold the store.
    let (mut field, paint, palette, zones) = with_store(|s| {
        let sess = get_mut(s, req.session_id)?;
        Ok((
            sess.field.clone(),
            sess.paint.clone(),
            sess.palette.clone(),
            sess.zones.clone(),
        ))
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

    export_heightfield(
        &field,
        &paint,
        &palette,
        &zones,
        &req,
        progress,
        cancel_flag,
    )
}

/// Multi-save export: each visible non-empty layer → its own `.brdb` at absolute
/// world coords (top layer wins on claim). Geometry-only (no per-part paint).
pub fn sculpt_export_layers(
    req: SculptExportRequest,
    progress: impl Fn(SculptProgress) + Send + Sync + 'static,
    cancel: impl Fn() -> bool + Send + Sync + 'static,
) -> Result<SculptLayersExportResult, String> {
    let (mut field, layers) = with_store(|s| {
        let sess = get_mut(s, req.session_id)?;
        Ok((sess.field.clone(), sess.layers.clone()))
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
    let progress_cb = progress;
    let progress: ProgressFn = Arc::new(move |stage: BuildStage, frac: f32| {
        if cancel() {
            cancel_flag_tick.store(true, Ordering::Relaxed);
        }
        progress_cb(SculptProgress {
            phase: stage.label().to_string(),
            frac,
        });
    });

    export_layer_parts_api(&field, &layers, &req, progress, cancel_flag)
}

fn export_heightfield(
    field: &HeightField,
    paint: &PaintGrid,
    palette: &[[u8; 4]],
    zones: &[Zone],
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
    let paint_cm;
    let colormap: &dyn Colormap = if !paint.is_blank() && !palette.is_empty() {
        paint_cm = PaintColormap::new(&paint.cells, palette, dem_width, dem_height);
        &paint_cm
    } else {
        &flat
    };
    let keep_mask = (!zones.is_empty()).then(|| zones::rasterize(zones, dem_width, dem_height));
    // skip_floor=true: blank/floor cells reveal native Brickadia ground (sculpt).
    let bricks = generate_bricks_skip_floor(
        &heightmap,
        colormap,
        style,
        Some(0),
        offset,
        true,
        0,
        MAX_BRICK_UNITS,
        Arc::clone(&progress),
        Arc::clone(&cancel),
        keep_mask.as_deref(),
    )
    .map_err(|e| format!("brick gen: {e:?}"))?;
    let brick_count = bricks.len();

    progress(BuildStage::WritingSave, 0.0);
    let world = bricks_to_save(bricks);

    let out_path = resolve_out_path(req, &field.meta.source_name)?;
    write_brdb_world(&world, &out_path)?;
    progress(BuildStage::WritingSave, 1.0);

    let (installed_path, install_warning) = maybe_install(&out_path, req, &progress);

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

fn write_brdb_world(world: &brdb::World, out_path: &std::path::Path) -> Result<(), String> {
    let path_str = out_path
        .to_str()
        .ok_or_else(|| format!("non-UTF8 out path: {}", out_path.display()))?;
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    if out_path.exists() {
        std::fs::remove_file(out_path)
            .map_err(|e| format!("remove stale {}: {e}", out_path.display()))?;
    }
    write_save_world(world, path_str).map_err(|e| format!("write save: {e}"))
}

fn maybe_install(
    out_path: &PathBuf,
    req: &SculptExportRequest,
    progress: &ProgressFn,
) -> (Option<PathBuf>, Option<String>) {
    if !req.install {
        return (None, None);
    }
    progress(BuildStage::Installing, 0.0);
    let result = match crate::api::install::install_save(out_path, req.overwrite) {
        Ok(p) => (Some(p), None),
        Err(e) => (None, Some(e)),
    };
    progress(BuildStage::Installing, 1.0);
    result
}

/// Plan + mesh each claimed layer part (geometry-only). Mirrors egui
/// `export_layer_parts` algebra without pulling gui convert.
fn export_layer_parts_api(
    field: &HeightField,
    stack: &LayerStack,
    req: &SculptExportRequest,
    progress: ProgressFn,
    cancel: Arc<AtomicBool>,
) -> Result<SculptLayersExportResult, String> {
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
    let size_base = cell_size_units(horizontal_scale, field.meta.micro);
    let (fw, fh) = (field.width, field.height);
    let claimed = stack.claim(fw, fh);

    // Collect non-empty visible parts.
    struct Part {
        name: String,
        bbox: (u32, u32, u32, u32),
        keep_local: Vec<bool>,
        offset: (i32, i32),
    }
    let mut parts = Vec::new();
    for layer in &stack.layers {
        if !layer.visible {
            continue;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (fw, fh, 0u32, 0u32);
        let mut any = false;
        for y in 0..fh {
            for x in 0..fw {
                if claimed[(y * fw + x) as usize] == Some(layer.id) {
                    any = true;
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x + 1);
                    y1 = y1.max(y + 1);
                }
            }
        }
        if !any {
            continue;
        }
        let (bw, bh) = (x1 - x0, y1 - y0);
        let mut keep_local = Vec::with_capacity((bw * bh) as usize);
        for y in y0..y1 {
            for x in x0..x1 {
                keep_local.push(claimed[(y * fw + x) as usize] == Some(layer.id));
            }
        }
        // World-extent gate (same algebra as convert::plan_layer_parts).
        let axis_off = |nw: u32, global: u32| -> i32 {
            let off = i64::from(nw) * 2 * i64::from(size_base)
                - i64::from(global) * i64::from(size_base);
            off.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
        };
        if !offset_fits_chunk_index(axis_off(x0, fw), fw, size_base)
            || !offset_fits_chunk_index(axis_off(y0, fh), fh, size_base)
        {
            return Err(format!(
                "layer '{}' places a part outside Brickadia world extent",
                layer.name
            ));
        }
        let offset = tile_world_offset(x0, y0, fw, fh, size_base);
        parts.push(Part {
            name: layer.name.clone(),
            bbox: (x0, y0, x1, y1),
            keep_local,
            offset,
        });
    }
    if parts.is_empty() {
        return Err(
            "no layer has a selection to export — paint boxes on layers first".into(),
        );
    }

    let out_dir = if let Some(p) = req.out_file.as_ref() {
        if p.as_os_str().is_empty() {
            None
        } else if p.is_dir() || p.extension().is_none() {
            Some(p.clone())
        } else {
            p.parent().map(|d| d.to_path_buf())
        }
    } else {
        None
    };
    let builds = match &out_dir {
        Some(d) => d.clone(),
        None => crate::api::install::builds_dir().map_err(|e| e.to_string())?,
    };
    std::fs::create_dir_all(&builds).map_err(|e| format!("mkdir {}: {e}", builds.display()))?;

    let total = parts.len() as f32;
    let mut outcomes = Vec::with_capacity(parts.len());
    let mut seen = std::collections::HashSet::new();
    for (i, part) in parts.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        progress(
            BuildStage::GeneratingBricks,
            i as f32 / total.max(1.0),
        );
        let (x0, y0, x1, y1) = part.bbox;
        let sub = field.sub_field(x0, y0, x1, y1);
        let raster = sub.to_dem_raster();
        let heightmap = build_heightmap(&raster, vertical_scale, 0.0);
        let flat = FlatColormap::sculpt_default(sub.width, sub.height);
        let noop: ProgressFn = Arc::new(|_, _| {});
        let bricks = generate_bricks_skip_floor(
            &heightmap,
            &flat,
            style,
            Some(0),
            part.offset,
            true,
            0,
            MAX_BRICK_UNITS,
            noop,
            Arc::clone(&cancel),
            Some(&part.keep_local),
        )
        .map_err(|e| format!("brick gen layer '{}': {e:?}", part.name))?;
        if bricks.is_empty() {
            continue;
        }
        let brick_count = bricks.len();
        let world = bricks_to_save(bricks);
        let mut stem = format!("{}_{}", field.meta.source_name, part.name);
        if !seen.insert(sanitize_name(&stem)) {
            stem = format!("{}_{}_{}", field.meta.source_name, i + 1, part.name);
            seen.insert(sanitize_name(&stem));
        }
        let out_path = builds.join(format!("{}.brdb", sanitize_name(&stem)));
        write_brdb_world(&world, &out_path)?;
        let (installed_path, install_warning) = maybe_install(&out_path, req, &progress);
        outcomes.push(SculptLayerPartResult {
            layer_name: part.name.clone(),
            path: out_path,
            brick_count,
            installed_path,
            install_warning,
        });
    }
    progress(BuildStage::WritingSave, 1.0);
    if outcomes.is_empty() {
        return Err("all selected layer parts meshed to zero bricks".into());
    }
    Ok(SculptLayersExportResult { parts: outcomes })
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
        sculpt_apply_stroke(stroke(
            id,
            SculptToolDto::Raise,
            16.0,
            16.0,
            6.0,
            10.0,
            true,
        ))
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

    fn stroke(
        id: u64,
        tool: SculptToolDto,
        cx: f32,
        cy: f32,
        r: f32,
        strength: f32,
        begin: bool,
    ) -> SculptStrokeRequest {
        SculptStrokeRequest {
            session_id: id,
            tool,
            center_x: cx,
            center_y: cy,
            radius_cells: r,
            strength,
            target_m: 0.0,
            begin_stroke: begin,
            stamp_kind: StampKindDto::Cone,
            peak_m: 40.0,
            inner_ratio: 0.4,
            angle_deg: 0.0,
            paint_index: 1,
            paint_res: 1,
        }
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
        sculpt_apply_stroke(stroke(
            id,
            SculptToolDto::Raise,
            8.0,
            8.0,
            4.0,
            5.0,
            true,
        ))
        .unwrap();
        assert!(sculpt_info(id).unwrap().max_m > 0.0);
        sculpt_undo(id).unwrap();
        assert_eq!(sculpt_info(id).unwrap().max_m, FLOOR_M);
        let _ = sculpt_close(id);
    }

    #[test]
    fn stamp_cone_and_paint_export() {
        let name = unique_name("stamp_paint");
        let info = sculpt_create_blank(SculptCreateBlankRequest {
            width: 32,
            height: 32,
            cell_m: 1.0,
            studs_per_meter: 4.0,
            vertical_exaggeration: 1.0,
            micro: false,
            source_name: name.clone(),
        })
        .expect("blank");
        let id = info.session_id;

        let mut req = stroke(id, SculptToolDto::Stamp, 16.0, 16.0, 10.0, 0.0, true);
        req.stamp_kind = StampKindDto::Cone;
        req.peak_m = 30.0;
        sculpt_apply_stroke(req).expect("stamp");
        assert!(sculpt_info(id).unwrap().max_m > 20.0, "cone peak deposited");

        let mut paint = stroke(id, SculptToolDto::Paint, 16.0, 16.0, 8.0, 0.0, true);
        paint.paint_index = 2;
        sculpt_apply_stroke(paint).expect("paint");
        let prev = sculpt_preview(id).expect("preview");
        assert!(!prev.paint.is_empty(), "paint indices in preview");
        assert!(prev.paint.iter().any(|&i| i == 2), "swatch 2 written");
        assert!(!prev.palette.is_empty());

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
        .expect("export painted");
        assert!(result.brick_count > 0);
        let _ = std::fs::remove_file(&out);
        let _ = sculpt_close(id);
    }

    #[test]
    fn zone_omit_and_layer_export() {
        let name = unique_name("zone_layer");
        let info = sculpt_create_blank(SculptCreateBlankRequest {
            width: 24,
            height: 24,
            cell_m: 1.0,
            studs_per_meter: 4.0,
            vertical_exaggeration: 1.0,
            micro: false,
            source_name: name.clone(),
        })
        .unwrap();
        let id = info.session_id;
        // Raise whole field via large stamp
        let mut st = stroke(id, SculptToolDto::Stamp, 12.0, 12.0, 20.0, 0.0, true);
        st.peak_m = 15.0;
        st.stamp_kind = StampKindDto::Mesa;
        sculpt_apply_stroke(st).unwrap();

        sculpt_zone_add_rect(SculptZoneAddRectRequest {
            session_id: id,
            mode: ZoneModeDto::Omit,
            x0: 8.0,
            y0: 8.0,
            x1: 16.0,
            y1: 16.0,
        })
        .unwrap();
        assert_eq!(sculpt_zones_info(id).unwrap().count, 1);

        let layers = sculpt_layer_add(id).unwrap();
        assert_eq!(layers.layers.len(), 2);
        // Base gets whole field via both boxes of a 2×1? Use default 8×6 grid — paint box 0,0 on base.
        sculpt_layer_paint_box(SculptLayerBoxRequest {
            session_id: id,
            bi: 0,
            bj: 0,
            on: true,
            layer_index: Some(0),
        })
        .unwrap();
        sculpt_layer_paint_box(SculptLayerBoxRequest {
            session_id: id,
            bi: 1,
            bj: 0,
            on: true,
            layer_index: Some(1),
        })
        .unwrap();

        let out_dir = std::env::temp_dir().join(format!("{name}_parts"));
        let _ = std::fs::create_dir_all(&out_dir);
        let multi = sculpt_export_layers(
            SculptExportRequest {
                session_id: id,
                out_file: Some(out_dir.clone()),
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
        .expect("layer export");
        assert!(!multi.parts.is_empty());
        for p in &multi.parts {
            assert!(p.path.exists());
            let _ = std::fs::remove_file(&p.path);
        }
        let _ = std::fs::remove_dir_all(&out_dir);
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
