mod generate;
mod greedy;
mod quad;

pub use generate::*;
pub use greedy::*;
pub use quad::*;

/// Maximum world-unit footprint of a single merged brick on each XY axis. Both
/// mesh paths (greedy `max_quad = MAX_BRICK_UNITS / size`, quadtree
/// `cells * tile_scale <= MAX_BRICK_UNITS`) cap merges to this so no brick
/// exceeds it.
///
/// IN-GAME LIMIT, not the format ceiling. The `.brdb` `BrickSize` is `u16`
/// (format ceiling 65535 units), but Brickadia **silently drops procedural bricks
/// whose footprint is too large** — they never render, leaving holes. Flat areas
/// merge into the biggest bricks, so they gap first. 12000 (2400 studs) was
/// chosen against the format ceiling and confirmed in-game to gap; 500 units
/// (100 studs) is Meshiest's upstream default and renders cleanly. Large fills
/// fragment more, but "Tile this export" keeps per-tile counts in check, so
/// correctness wins. Raise only with in-game proof that bigger bricks render.
pub const MAX_BRICK_UNITS: u16 = 500;
