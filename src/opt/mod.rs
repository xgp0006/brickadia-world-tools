mod generate;
mod greedy;
mod quad;

pub use generate::*;
pub use greedy::*;
pub use quad::*;

/// Maximum world-unit footprint of a single merged brick on each XY axis. Both
/// mesh paths (greedy `max_quad = MAX_BRICK_UNITS / size`, quadtree
/// `cells * tile_scale <= MAX_BRICK_UNITS`) cap merges to this so no brick
/// exceeds it. NOT a hard engine limit: the `.brdb` `BrickSize` is `u16`, so the
/// format ceiling is 65535 units (~13107 studs); the old `500` (100 studs) was
/// Meshiest's conservative upstream default and forced huge fills to fragment
/// into hundreds of thousands of tiny bricks (the load-hang). 12000 units = 2400
/// studs is a generous, u16-safe band — raise toward the format ceiling if
/// in-game testing shows larger bricks render fine.
pub(crate) const MAX_BRICK_UNITS: u16 = 12_000;
