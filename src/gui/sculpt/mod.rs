//! Sculpt workspace: a brush-based terrain height editor layered on the
//! existing brick-conversion pipeline.
//!
//! **Engine (feature `dem`)** — [`HeightField`], [`brush`], [`tools`]: pure
//! (no egui). Tauri Phase 4 and unit tests depend on these under `dem` alone.
//!
//! **Shell (feature `gui`)** — egui tab, paint/zones/layers, full convert seam.
//! Kept out of the Tauri `dem` feature so the desktop shell does not pull egui.

mod brush;
mod heightfield;
mod tools;

#[cfg(feature = "gui")]
mod convert;
#[cfg(feature = "gui")]
mod layers;
#[cfg(feature = "gui")]
mod paint;
#[cfg(feature = "gui")]
mod sculpt_tab;

// Public to the rest of the crate (api::sculpt, tests). The egui surface stays
// gui-gated so dem-only builds never name sculpt_tab.
pub(crate) use heightfield::{FieldMeta, HeightField, FLOOR_M, HEIGHTMAP_PNG_SCALE};

#[cfg(feature = "gui")]
pub(crate) use sculpt_tab::{draw, SculptState};

// Re-export engine pieces for api::sculpt (same-crate).
pub(crate) use brush::{Brush, BrushShape, Falloff};
pub(crate) use tools::{Flatten, Lower, Raise, SetHeight, Smooth, Tool};
