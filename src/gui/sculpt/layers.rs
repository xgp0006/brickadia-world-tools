//! Export layers (design spec `docs/superpowers/specs/2026-06-30-sculpt-layers-design.md`).
//!
//! A [`LayerStack`] is an ordered set of selections of the field; each visible,
//! non-empty layer exports as its OWN save, written at true absolute world
//! coordinates, so loading every part in Brickadia auto-assembles one world. This
//! module is the PURE data model + the z-order claim pass that turns overlapping
//! selections into a strict partition (top layer wins). The export pipeline that
//! turns a claimed partition into bricks lives in [`super::convert`]; the canvas
//! overlay + panel UI live in [`super::sculpt_tab`].
//!
//! MVP scope (this phase): box selections only, single resolution. Lasso regions
//! (Phase 2), per-layer color/visibility UI (Phase 3), per-layer settings + the
//! integer resolution multiplier (Phase 1/5) extend this without changing the
//! claim-pass contract.

/// A stable per-layer identity — survives reordering/rename, keys the claim map.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub(crate) struct LayerId(pub u32);

/// Eight visually distinct layer-identity colors, spaced around the wheel and
/// kept clear of the paint palette (a layer color marks a region on the canvas —
/// it never colors exported bricks). New layers cycle through these.
pub(crate) const LAYER_COLORS: [[u8; 4]; 8] = [
    [0xE6, 0x6E, 0x52, 0xFF], // ember
    [0x4F, 0xB0, 0xE6, 0xFF], // sky
    [0x7E, 0xD3, 0x6B, 0xFF], // leaf
    [0xE6, 0xB7, 0x4F, 0xFF], // amber
    [0xB8, 0x7C, 0xE6, 0xFF], // violet
    [0xE6, 0x6F, 0xB0, 0xFF], // rose
    [0x52, 0xD6, 0xC0, 0xFF], // teal
    [0xC9, 0xCF, 0x5A, 0xFF], // chartreuse
];

/// One export layer: a per-field-cell selection plus its identity (name/color/
/// visibility). The selection is stored as CELLS (`box_mask`), not box indices,
/// so changing the grid divisions never loses a pick.
#[derive(Clone)]
pub(crate) struct Layer {
    pub id: LayerId,
    pub name: String,
    pub color: [u8; 4],
    /// Hidden layers are excluded from export AND from the claim pass (their cells
    /// fall through to a lower layer, or go unclaimed).
    pub visible: bool,
    /// Row-major per-cell selection (len = `fw * fh`). `true` = this layer wants
    /// the cell; the claim pass resolves overlaps to a single owner.
    pub box_mask: Vec<bool>,
}

/// The ordered layer set. Index `0` is the BOTTOM layer; the LAST element is the
/// TOP (highest precedence in the claim pass). Always holds ≥ 1 layer (the
/// un-deletable "Base").
#[derive(Clone)]
pub(crate) struct LayerStack {
    pub layers: Vec<Layer>,
    pub active: usize,
    /// Box-grid divisions `(cols, rows)` — a UI aid for picking rectangles of
    /// cells; the selection itself is always per-cell.
    pub grid_div: (u32, u32),
    next_id: u32,
}

impl LayerStack {
    /// A fresh stack over an `fw × fh` field: one empty un-deletable "Base" layer,
    /// an 8×6 box grid.
    pub fn new(fw: u32, fh: u32) -> Self {
        let base = Layer {
            id: LayerId(0),
            name: "Base".to_owned(),
            color: LAYER_COLORS[0],
            visible: true,
            box_mask: vec![false; (fw as usize) * (fh as usize)],
        };
        Self { layers: vec![base], active: 0, grid_div: (8, 6), next_id: 1 }
    }

    fn fresh_id(&mut self) -> LayerId {
        let id = LayerId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Add a new empty layer above the active one and make it active. Its color
    /// cycles through [`LAYER_COLORS`]; its name is `"Layer N"`.
    pub fn add_layer(&mut self, fw: u32, fh: u32) {
        let id = self.fresh_id();
        let color = LAYER_COLORS[(id.0 as usize) % LAYER_COLORS.len()];
        let layer = Layer {
            id,
            name: format!("Layer {}", id.0),
            color,
            visible: true,
            box_mask: vec![false; (fw as usize) * (fh as usize)],
        };
        let at = (self.active + 1).min(self.layers.len());
        self.layers.insert(at, layer);
        self.active = at;
    }

    pub fn active_layer_mut(&mut self) -> &mut Layer {
        let i = self.active.min(self.layers.len() - 1);
        &mut self.layers[i]
    }

    /// The half-open cell range `[x0,x1) × [y0,y1)` covered by box `(bi, bj)` of
    /// the current `grid_div` over an `fw × fh` field. Boxes partition the field
    /// exactly (no gap, no overlap) via integer `bi*fw/cols` boundaries.
    pub fn box_cell_range(&self, fw: u32, fh: u32, bi: u32, bj: u32) -> (u32, u32, u32, u32) {
        let (cols, rows) = (self.grid_div.0.max(1), self.grid_div.1.max(1));
        let x0 = bi * fw / cols;
        let x1 = (bi + 1) * fw / cols;
        let y0 = bj * fh / rows;
        let y1 = (bj + 1) * fh / rows;
        (x0, y0, x1.max(x0), y1.max(y0))
    }

    /// Whether the ACTIVE layer fully owns box `(bi, bj)` (every cell set). Used
    /// to toggle a box on the canvas: full → clear it, otherwise → set it.
    pub fn box_full_in_active(&self, fw: u32, fh: u32, bi: u32, bj: u32) -> bool {
        let (x0, y0, x1, y1) = self.box_cell_range(fw, fh, bi, bj);
        if x0 >= x1 || y0 >= y1 {
            return false;
        }
        let mask = &self.layers[self.active.min(self.layers.len() - 1)].box_mask;
        for y in y0..y1 {
            for x in x0..x1 {
                if !mask.get((y * fw + x) as usize).copied().unwrap_or(false) {
                    return false;
                }
            }
        }
        true
    }

    /// Set (or clear) every cell under box `(bi, bj)` in the active layer.
    pub fn paint_box(&mut self, fw: u32, fh: u32, bi: u32, bj: u32, on: bool) {
        let (x0, y0, x1, y1) = self.box_cell_range(fw, fh, bi, bj);
        let mask = &mut self.active_layer_mut().box_mask;
        for y in y0..y1 {
            for x in x0..x1 {
                let idx = (y * fw + x) as usize;
                if idx < mask.len() {
                    mask[idx] = on;
                }
            }
        }
    }

    /// The z-order claim pass: resolve overlapping selections to a strict
    /// partition. Each cell is claimed by the HIGHEST visible layer that selects
    /// it (top = last in `layers`). Returns a per-cell owner map (`len = fw*fh`).
    /// Hidden layers don't participate, so their cells fall through to a lower
    /// layer or stay `None` (unclaimed → not exported by any part).
    pub fn claim(&self, fw: u32, fh: u32) -> Vec<Option<LayerId>> {
        let mut claimed = vec![None; (fw as usize) * (fh as usize)];
        // Top layer first → it wins every contested cell; lower layers fill only
        // the gaps it left.
        for layer in self.layers.iter().rev() {
            if !layer.visible {
                continue;
            }
            for (idx, owner) in claimed.iter_mut().enumerate() {
                if owner.is_none() && layer.box_mask.get(idx).copied().unwrap_or(false) {
                    *owner = Some(layer.id);
                }
            }
        }
        claimed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_ranges_partition_the_field() {
        let mut s = LayerStack::new(10, 8);
        s.grid_div = (2, 2);
        // Four boxes tile the field with no gap/overlap.
        assert_eq!(s.box_cell_range(10, 8, 0, 0), (0, 0, 5, 4));
        assert_eq!(s.box_cell_range(10, 8, 1, 0), (5, 0, 10, 4));
        assert_eq!(s.box_cell_range(10, 8, 0, 1), (0, 4, 5, 8));
        assert_eq!(s.box_cell_range(10, 8, 1, 1), (5, 4, 10, 8));
    }

    #[test]
    fn paint_box_sets_only_its_cells() {
        let mut s = LayerStack::new(4, 4);
        s.grid_div = (2, 1);
        s.active = 0;
        s.paint_box(4, 4, 0, 0, true); // left half: cols 0..2, all rows
        let m = &s.layers[0].box_mask;
        for y in 0..4 {
            assert!(m[(y * 4) as usize] && m[(y * 4 + 1) as usize], "left cols set");
            assert!(!m[(y * 4 + 2) as usize] && !m[(y * 4 + 3) as usize], "right cols clear");
        }
    }

    #[test]
    fn claim_top_layer_wins_contested_cells() {
        let mut s = LayerStack::new(4, 4);
        s.grid_div = (1, 1);
        s.active = 0;
        s.paint_box(4, 4, 0, 0, true); // Base selects the whole field
        s.add_layer(4, 4); // a top layer (index 1, now active)
        s.paint_box(4, 4, 0, 0, true); // also selects the whole field
        let top = s.layers[1].id;
        let claimed = s.claim(4, 4);
        assert!(claimed.iter().all(|c| *c == Some(top)), "the top layer wins every contested cell");

        // Hiding the top layer falls every cell through to Base.
        s.layers[1].visible = false;
        let base = s.layers[0].id;
        let claimed = s.claim(4, 4);
        assert!(claimed.iter().all(|c| *c == Some(base)), "a hidden top layer falls through to Base");
    }

    #[test]
    fn unselected_cells_stay_unclaimed() {
        let mut s = LayerStack::new(4, 4);
        s.grid_div = (2, 1);
        s.active = 0;
        s.paint_box(4, 4, 0, 0, true); // only the left half is selected
        let claimed = s.claim(4, 4);
        let id = s.layers[0].id;
        for y in 0..4 {
            assert_eq!(claimed[(y * 4) as usize], Some(id), "left is claimed");
            assert_eq!(claimed[(y * 4 + 3) as usize], None, "right is unclaimed (exported by no part)");
        }
    }
}
