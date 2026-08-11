use super::generate::{BrickColumn, emit_column_bricks};
use crate::map::*;
use crate::util::*;
use brdb::Brick;
use std::{cmp::max, collections::HashSet};

#[derive(Debug, Default)]
struct Tile {
    index: usize,
    center: (u32, u32),
    size: (u32, u32),
    color: [u8; 4],
    height: u32,
    neighbors: HashSet<u32>,
    parent: Option<usize>,
}

pub struct QuadTree {
    tiles: Box<[Tile]>,
    width: u32,
    height: u32,
}

impl Tile {
    // determine if another tile is similar in all properties
    fn similar_quad(&self, other: &Self) -> bool {
        self.size == other.size
            && self.color == other.color
            && self.height == other.height
            && self.parent.is_none()
            && other.parent.is_none()
    }

    // determine if another tile is similar in all properties except potentially width or height as long as they are in a line
    fn similar_line(&self, other: &Self) -> bool {
        let is_vertical = self.center.0 == other.center.0;
        let is_horizontal = self.center.1 == other.center.1;

        (is_vertical && self.size.0 == other.size.0 || is_horizontal && self.size.1 == other.size.1)
            && self.color == other.color
            && self.height == other.height
            && self.parent.is_none()
            && other.parent.is_none()
    }

    // merge a few tiles with this one
    fn merge_quad(
        &mut self,
        top_right: &mut Self,
        bottom_left: &mut Self,
        bottom_right: &mut Self,
    ) {
        // update size
        self.size = (self.size.0 * 2, self.size.1 * 2);

        self.neighbors.extend(&top_right.neighbors);
        self.neighbors.extend(&bottom_left.neighbors);
        self.neighbors.extend(&bottom_right.neighbors);

        // update parents of merged nodes
        top_right.parent = Some(self.index);
        bottom_left.parent = Some(self.index);
        bottom_right.parent = Some(self.index);
    }
}

impl QuadTree {
    // create a heightmap grid from two images
    pub fn new(heightmap: &dyn Heightmap, colormap: &dyn Colormap) -> Result<Self, String> {
        let (width, height) = heightmap.size();

        if colormap.size() != heightmap.size() {
            return Err("Heightmap and colormap must have same dimensions".to_string());
        }

        let mut tiles = Vec::with_capacity((width * height) as usize);

        // add all the tiles to the heightmap
        for x in 0..width as i32 {
            for y in 0..height as i32 {
                tiles.push(Tile {
                    index: (x + y * height as i32) as usize,
                    center: (x as u32, y as u32),
                    // store a set of the neighbor's heights with each tile
                    // they will be joined when the tiles merge
                    neighbors: vec![(x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)]
                        .into_iter()
                        .filter(|(x, y)| {
                            *x >= 0 && *x < width as i32 && *y >= 0 && *y < height as i32
                        })
                        .map(|(x, y)| heightmap.at(x as u32, y as u32))
                        .fold(HashSet::new(), |mut set, height| {
                            set.insert(height);
                            set
                        }),
                    size: (1, 1),
                    color: colormap.at(x as u32, y as u32),
                    height: heightmap.at(x as u32, y as u32),
                    parent: None,
                })
            }
        }

        Ok(QuadTree {
            tiles: tiles.into_boxed_slice(),
            width,
            height,
        })
    }

    fn index(&self, x: u32, y: u32) -> usize {
        (y + x * self.height) as usize
    }

    // optimize bricks with size (level+1)
    pub fn quad_optimize_level(&mut self, level: u32) -> usize {
        let mut count = 0;

        // step amounts
        let space = 2_u32.pow(level);
        let step_amt = space as usize * 2;

        for x in (0..self.width - space).step_by(step_amt) {
            for y in (0..self.height - space).step_by(step_amt) {
                // split vertically (left/right columns)
                let (left, right) = self
                    .tiles
                    .split_at_mut(((x + space) * self.height) as usize);

                // split the columns horizontally
                let (top_left, bottom_left) =
                    left.split_at_mut((y + space + x * self.height) as usize);
                let (top_right, bottom_right) = right.split_at_mut((y + space) as usize);

                // first of each slice is the target cell
                let top_left = &mut top_left[(y + x * self.height) as usize];
                let bottom_left = &mut bottom_left[0];
                let top_right = &mut top_right[y as usize];
                let bottom_right = &mut bottom_right[0];

                // if these are not similar tiles, skip them
                if top_left.size.0 != space
                    || !top_left.similar_quad(top_right)
                    || !top_left.similar_quad(bottom_left)
                    || !top_left.similar_quad(bottom_right)
                {
                    continue;
                }

                count += 3;

                // merge the tiles into the first one
                top_left.merge_quad(top_right, bottom_left, bottom_right);
            }
        }

        count
    }

    // merge tiles that are arranged in a line
    fn merge_line(&mut self, start_i: usize, children: Vec<usize>) {
        // there is nothing to merge, return
        if children.is_empty() {
            return;
        }

        let mut new_neighbors = vec![];

        // determine direction of this merge
        let is_vertical = self.tiles[children[0]].center.0 == self.tiles[start_i].center.0;

        // determine the new size of the parent tile, make children point at the parent
        let new_size = children.iter().fold(0, |sum, &i| {
            let t = &mut self.tiles[i];
            // assign parent, extend parent's neighbors
            t.parent = Some(start_i);
            new_neighbors.push(t.neighbors.clone());

            // sum size depending on merge direction
            sum + if is_vertical { t.size.1 } else { t.size.0 }
        });

        let start = &mut self.tiles[start_i];

        for n in new_neighbors {
            start.neighbors.extend(&n);
        }

        // add the size to its respective dimension
        if is_vertical {
            start.size.1 += new_size
        } else {
            start.size.0 += new_size
        }
    }

    // optimize by nearby bricks in line
    pub fn line_optimize(&mut self, tile_scale: u32) -> usize {
        let mut count = 0;
        for x in 0..self.width {
            for y in 0..self.height {
                let start_i = self.index(x, y);
                let start = &self.tiles[start_i];
                if start.parent.is_some() {
                    continue;
                }

                let shift = start.size;
                let mut sx = shift.0;
                let mut horiz_tiles = vec![];
                let mut sy = shift.1;
                let mut vert_tiles = vec![];

                // determine longest horizontal merge
                while x + sx < self.width {
                    let i = self.index(x + sx, y);
                    let t = &self.tiles[i];
                    if (sx + t.size.0) * tile_scale > u32::from(super::MAX_BRICK_UNITS) || !start.similar_line(t) {
                        break;
                    }
                    horiz_tiles.push(i);
                    sx += t.size.0;
                }

                // determine longest vertical merge
                while y + sy < self.height {
                    let i = self.index(x, y + sy);
                    let t = &self.tiles[i];
                    if (sy + t.size.1) * tile_scale > u32::from(super::MAX_BRICK_UNITS) || !start.similar_line(t) {
                        break;
                    }
                    vert_tiles.push(i);
                    sy += t.size.1;
                }

                count += max(horiz_tiles.len(), vert_tiles.len());

                // merge whichever is largest
                self.merge_line(
                    start_i,
                    if horiz_tiles.len() > vert_tiles.len() {
                        horiz_tiles
                    } else {
                        vert_tiles
                    },
                );
            }
        }

        count
    }

    // convert quadtree state into bricks
    pub fn into_bricks(&self, options: GenOptions, width: u32, height: u32) -> Vec<Brick> {
        // Calculate offsets to center the bricks
        let offset_x = -(width as i32 * options.size as i32);
        let offset_y = -(height as i32 * options.size as i32);

        let mut bricks = vec![];
        for t in self.tiles.iter() {
            if t.parent.is_some() || options.cull && (t.height == 0 || t.color[3] == 0) {
                continue;
            }

            let mut z = (options.scale * t.height) as i32;

            // determine the height of this brick (difference of self and smallest neighbor)
            let raw_height = max(
                t.height as i32 - t.neighbors.iter().cloned().min().unwrap_or(0) as i32 + 1,
                2,
            );
            let mut desired_height = max(raw_height * options.scale as i32 / 2, 2);

            // snap bricks to grid
            if options.snap {
                z += 4 - z % 4;
                desired_height += 4 - desired_height % 4;
            }

            emit_column_bricks(
                &mut bricks,
                &options,
                BrickColumn {
                    z,
                    desired_height,
                    size_x: t.size.0 as u16 * options.size,
                    size_y: t.size.1 as u16 * options.size,
                    pos_x: (t.center.0 as i32 * 2 + t.size.0 as i32) * options.size as i32
                        + offset_x,
                    pos_y: (t.center.1 as i32 * 2 + t.size.1 as i32) * options.size as i32
                        + offset_y,
                    color: t.color,
                },
            );
        }
        bricks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brdb::assets::bricks::PB_DEFAULT_TILE;

    struct TestMap {
        width: u32,
        height: u32,
        heights: Vec<u32>,
        colors: Vec<[u8; 4]>,
    }

    impl TestMap {
        fn uniform(width: u32, height: u32, h: u32, color: [u8; 4]) -> Self {
            let cells = (width * height) as usize;
            TestMap {
                width,
                height,
                heights: vec![h; cells],
                colors: vec![color; cells],
            }
        }
    }

    impl Heightmap for TestMap {
        fn at(&self, x: u32, y: u32) -> u32 {
            self.heights[(x + y * self.width) as usize]
        }
        fn size(&self) -> (u32, u32) {
            (self.width, self.height)
        }
    }

    impl Colormap for TestMap {
        fn at(&self, x: u32, y: u32) -> [u8; 4] {
            self.colors[(x + y * self.width) as usize]
        }
        fn size(&self) -> (u32, u32) {
            (self.width, self.height)
        }
    }

    fn test_options() -> GenOptions {
        GenOptions {
            size: 5,
            scale: 1,
            asset: PB_DEFAULT_TILE,
            cull: false,
            micro: false,
            stud: false,
            snap: false,
            img: false,
            glow: false,
            hdmap: false,
            lrgb: false,
            nocollide: false,
            quadtree: true,
            greedy: false,
            fill_to_base: false,
            skip_floor: false,
            omit_below_h: 0,
            max_brick_units: crate::opt::MAX_BRICK_UNITS,
            streaming_mesh: false,
        }
    }

    #[test]
    fn new_builds_one_tile_per_pixel() {
        let map = TestMap {
            width: 2,
            height: 2,
            heights: vec![1, 2, 3, 4],
            colors: vec![
                [255, 0, 0, 255],
                [0, 255, 0, 255],
                [0, 0, 255, 255],
                [255, 255, 0, 255],
            ],
        };
        let quad = QuadTree::new(&map, &map).expect("quadtree build");

        assert_eq!(quad.tiles.len(), 4, "2x2 map must produce 4 tiles");
        for tile in quad.tiles.iter() {
            let (x, y) = tile.center;
            assert_eq!(tile.height, map.heights[(x + y * 2) as usize]);
            assert_eq!(tile.color, map.colors[(x + y * 2) as usize]);
            assert_eq!(tile.size, (1, 1));
            assert!(tile.parent.is_none());
        }
    }

    #[test]
    fn new_rejects_mismatched_dimensions() {
        let hm = TestMap::uniform(2, 2, 1, [255, 0, 0, 255]);
        let cm = TestMap::uniform(3, 2, 1, [255, 0, 0, 255]);
        assert!(QuadTree::new(&hm, &cm).is_err());
    }

    #[test]
    fn into_bricks_covers_every_unmerged_tile() {
        // Distinct colors prevent any merge, so a 2x2 map must yield exactly
        // 4 bricks at the 4 centered footprint positions.
        let map = TestMap {
            width: 2,
            height: 2,
            heights: vec![1; 4],
            colors: vec![
                [255, 0, 0, 255],
                [0, 255, 0, 255],
                [0, 0, 255, 255],
                [255, 255, 0, 255],
            ],
        };
        let quad = QuadTree::new(&map, &map).expect("quadtree build");
        let bricks = quad.into_bricks(test_options(), 2, 2);

        assert_eq!(bricks.len(), 4);
        let mut positions: Vec<(i32, i32)> = bricks
            .iter()
            .map(|b| (b.position.x, b.position.y))
            .collect();
        positions.sort_unstable();
        assert_eq!(positions, vec![(-5, -5), (-5, 5), (5, -5), (5, 5)]);
    }

    #[test]
    fn line_optimize_merges_a_uniform_row() {
        let map = TestMap::uniform(4, 1, 1, [128, 128, 128, 255]);
        let mut quad = QuadTree::new(&map, &map).expect("quadtree build");

        let unmerged = quad.into_bricks(test_options(), 4, 1).len();
        assert_eq!(unmerged, 4, "before optimization every pixel is a brick");

        let merged = quad.line_optimize(test_options().size as u32);
        assert!(merged > 0, "uniform row must merge at least one tile");
        assert_eq!(quad.line_optimize(test_options().size as u32), 0);

        let bricks = quad.into_bricks(test_options(), 4, 1);
        assert_eq!(bricks.len(), 1, "uniform 4x1 row must merge into one brick");
    }
}
