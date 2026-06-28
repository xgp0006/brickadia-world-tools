//! Sculpt workspace: a brush-based terrain height editor (MVP #1, height
//! sculpting) layered on the existing brick-conversion pipeline.
//!
//! Stages 1–2 deliver the engine foundation: the [`HeightField`] grid and its
//! conversion seam to the converter's [`DemRaster`] input (Stage 1), and the
//! [`Brush`]/[`Tool`] sculpt engine (Stage 2). Stage 3 adds the egui tab
//! ([`sculpt_tab`]), [`SculptState`], and the [`convert::convert_heightfield`]
//! Sculpt → Convert seam. This module is the shared "layer 0" everything else
//! builds on.

mod brush;
mod convert;
mod heightfield;
mod paint;
mod sculpt_tab;
mod tools;

// The brush/tool/convert items are the engine internals; the sculpt tab and the
// convert seam consume them through their `super::` module paths, so they are
// NOT re-exported here. Only the surface the rest of the GUI touches is exported:
// `HeightField`/`FieldMeta` (the Map → Sculpt handoff) and the tab itself.
pub(crate) use heightfield::{FieldMeta, HeightField};
pub(crate) use sculpt_tab::{draw, SculptState};
