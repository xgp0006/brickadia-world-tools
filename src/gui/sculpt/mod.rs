//! Sculpt workspace: a brush-based terrain height editor layered on the
//! existing brick-conversion pipeline.
//!
//! **Engine (feature `dem`)** — [`HeightField`], [`brush`], [`tools`],
//! [`paint`], [`layers`]: pure (no egui). Tauri Phase 4 depends on these under
//! `dem` alone.
//!
//! **Shell (feature `gui`)** — egui tab + full convert seam (tiled/zones/layers
//! export helpers that still pull map_tab derive_scale). Kept out of the Tauri
//! `dem` feature so the desktop shell does not pull egui.

mod brush;
mod heightfield;
mod tools;
mod paint;
mod layers;

#[cfg(feature = "gui")]
mod convert;
#[cfg(feature = "gui")]
mod sculpt_tab;

// Public to the rest of the crate (api::sculpt, tests). The egui surface stays
// gui-gated so dem-only builds never name sculpt_tab.
pub(crate) use heightfield::{FieldMeta, HeightField, FLOOR_M, HEIGHTMAP_PNG_SCALE};

#[cfg(feature = "gui")]
pub(crate) use sculpt_tab::{draw, SculptState};

// Re-export engine pieces for api::sculpt (same-crate).
pub(crate) use brush::{shape_distance, Brush, BrushShape, Falloff};
pub(crate) use tools::{
    Flatten, Lower, Raise, SetHeight, Smooth, Stamp, StampKind, Tool,
};
pub(crate) use paint::{default_palette, PaintColormap, PaintGrid};
pub(crate) use layers::{LayerId, LayerStack};
