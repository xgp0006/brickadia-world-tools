use crate::map::{Colormap, ColormapPNG, Heightmap, HeightmapFlat, HeightmapPNG};
use brdb::{
    BString, Brick, Position, World,
    assets::bricks::B_SPAWN_POINT,
};
use std::{ffi::OsStr, path::PathBuf};

pub struct GenOptions {
    pub size: u16,
    pub scale: u32,
    pub asset: BString,
    pub cull: bool,
    pub micro: bool,
    pub stud: bool,
    pub snap: bool,
    pub img: bool,
    pub glow: bool,
    pub hdmap: bool,
    pub lrgb: bool,
    pub nocollide: bool,
    pub quadtree: bool,
    pub greedy: bool,
    /// Greedy path only: fill each terrain column from its height DOWN to the
    /// common base plane (solid, watertight ground) instead of a constant 2-unit
    /// surface tile. Set by the Map tab, whose heights are normalized to ~0 and
    /// brick-count-capped. The Convert tab and CLI leave it false: their heights
    /// are un-normalized and `vertical_scale` reaches 100, so filling to base
    /// could emit an unbounded brick stack (and those paths have no MAX_BRICKS
    /// guard). img2brick mode ignores it regardless.
    pub fill_to_base: bool,
    /// Greedy + fill_to_base terrain path only: when set, a column whose fill
    /// height bottoms out at the common base plane (i.e. `h == min_height`, so
    /// `(h - min_height) == 0`) emits NO bricks — the native Brickadia flat
    /// ground serves as the floor, so flat areas reveal the world floor instead
    /// of a redundant plate. Set `true` by the sculpt/blank-canvas convert.
    /// Default `false` keeps every existing single-box/grid output
    /// byte-identical (those paths never skip). Has no effect unless both
    /// `fill_to_base` is set and `img` is clear (mirrors the fill_to_base gate).
    pub skip_floor: bool,
}

impl GenOptions {
    pub fn base_height(&self) -> i32 {
        if self.stud {
            5
        } else if self.micro {
            1
        } else {
            2
        }
    }
}

// convert gamma to linear gamma
pub fn to_linear_gamma(c: u8) -> u8 {
    let cf = (c as f64) / 255.0;
    (if cf > 0.04045 {
        (cf / 1.055 + 0.0521327).powf(2.4) * 255.0
    } else {
        cf / 12.192 * 255.0
    }) as u8
}

// convert sRGB to linear rgb
pub fn to_linear_rgb(rgb: [u8; 4]) -> [u8; 4] {
    [
        to_linear_gamma(rgb[0]),
        to_linear_gamma(rgb[1]),
        to_linear_gamma(rgb[2]),
        rgb[3],
    ]
}

// given an array of bricks, create a save
pub fn bricks_to_save(bricks: Vec<Brick>) -> World {
    let mut world = World::new();

    // Inject a B_SpawnPoint above the terrain centroid so the player has
    // somewhere to land when loading the save as a world. Upstream
    // heightmap2brz emits bricks only — Brickadia falls back to (0,0,0)
    // when there's no spawn, which spawns the player inside the geometry
    // (black screen) for large terrains.
    if !bricks.is_empty() {
        let mut min_x = i32::MAX;
        let mut max_x = i32::MIN;
        let mut min_y = i32::MAX;
        let mut max_y = i32::MIN;
        let mut max_z = i32::MIN;
        for b in &bricks {
            min_x = min_x.min(b.position.x);
            max_x = max_x.max(b.position.x);
            min_y = min_y.min(b.position.y);
            max_y = max_y.max(b.position.y);
            max_z = max_z.max(b.position.z);
        }
        let cx = (min_x + max_x) / 2;
        let cy = (min_y + max_y) / 2;
        // 50 studs above the highest brick — small drop onto the peak.
        let spawn_z = max_z.saturating_add(50);

        let spawn = Brick {
            asset: B_SPAWN_POINT,
            position: Position::new(cx, cy, spawn_z),
            ..Default::default()
        };
        world.add_brick(spawn);
    }

    world.add_bricks(bricks);
    world.meta.bundle.description = "Save generated from heightmap file".to_string();
    world
}

// get extension from filename
pub fn file_ext(filename: &std::path::Path) -> Option<&str> {
    filename.extension().and_then(OsStr::to_str)
}

pub type MapPair = (Box<dyn Heightmap>, Box<dyn Colormap>);

/// Decodes the heightmap/colormap input files into map objects. When
/// `colormap_file` is `None` the first heightmap doubles as the colormap.
pub fn maps_from_files(
    options: &GenOptions,
    heightmap_files: Vec<PathBuf>,
    colormap_file: Option<PathBuf>,
) -> Result<MapPair, String> {
    let first_heightmap = heightmap_files
        .first()
        .map(|s| s.to_owned())
        .unwrap_or_else(|| "".into());
    let colormap_file = colormap_file.unwrap_or(first_heightmap);

    // colormap file parsing
    let colormap = match file_ext(&colormap_file)
        .map(|s| s.to_lowercase())
        .as_deref()
    {
        Some("png") | Some("jpg") | Some("jpeg") => ColormapPNG::new(&colormap_file, options.lrgb)
            .map_err(|e| format!("Error reading colormap: {:?}", e))?,
        Some(ext) => {
            return Err(format!("Unsupported colormap format '{}'", ext));
        }
        None => {
            return Err(format!(
                "Missing colormap format for '{}'",
                colormap_file.display()
            ));
        }
    };

    // heightmap file parsing
    let heightmap: Box<dyn Heightmap> = if heightmap_files.iter().all(|f| {
        matches!(
            file_ext(f).map(|s| s.to_lowercase()).as_deref(),
            Some("png") | Some("jpg") | Some("jpeg")
        )
    }) {
        if options.img {
            Box::new(HeightmapFlat::new(colormap.size())?)
        } else {
            match HeightmapPNG::new(heightmap_files.iter().collect(), options.hdmap) {
                Ok(map) => Box::new(map),
                Err(error) => {
                    return Err(format!("Error reading heightmap: {:?}", error));
                }
            }
        }
    } else {
        return Err("Unsupported heightmap format".to_string());
    };

    Ok((heightmap, Box::new(colormap)))
}

/// Writes an already-built `World` to `out_file`. The format is picked from the
/// extension: `.brz` or `.brdb` (case-insensitive). Factored out of
/// [`write_save`] so the grid output layer can write a `World` it built once
/// (stitched accumulator or per-tile) without rebuilding it per format.
pub fn write_save_world(world: &World, out_file: &str) -> Result<(), String> {
    let lower = out_file.to_lowercase();
    if lower.ends_with(".brz") {
        let brz = world
            .to_brz_vec()
            .map_err(|e| format!("failed to encode brz: {e}"))?;
        std::fs::write(out_file, brz).map_err(|e| format!("failed to write file: {e}"))
    } else if lower.ends_with(".brdb") {
        world
            .write_brdb(out_file)
            .map_err(|e| format!("failed to write file: {e}"))
    } else {
        Err("output file must end with .brz or .brdb".to_string())
    }
}

/// Encodes bricks as a save and writes it to `out_file`. The format is picked
/// from the extension: `.brz` or `.brdb` (case-insensitive).
pub fn write_save(out_file: &str, bricks: Vec<Brick>) -> Result<(), String> {
    let data = bricks_to_save(bricks);
    write_save_world(&data, out_file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brick_at(x: i32, y: i32, z: i32) -> Brick {
        Brick { position: Position::new(x, y, z), ..Default::default() }
    }

    #[test]
    fn injects_one_spawn_point_at_centroid_above_peak() {
        // x in {0,100,50} -> cx = (0+100)/2 = 50
        // y in {0,40,80}  -> cy = (0+80)/2  = 40
        // z in {0,200,120} -> max_z = 200 -> spawn_z = 250
        let input = vec![brick_at(0, 0, 0), brick_at(100, 40, 200), brick_at(50, 80, 120)];
        let n = input.len();
        let world = bricks_to_save(input);

        assert_eq!(world.bricks.len(), n + 1, "exactly one spawn brick should be added");
        let spawn = &world.bricks[0];
        assert_eq!((spawn.position.x, spawn.position.y, spawn.position.z), (50, 40, 250));
        assert!(
            format!("{:?}", spawn.asset).contains("SpawnPoint"),
            "first brick must be a B_SpawnPoint, got {:?}",
            spawn.asset,
        );
    }

    #[test]
    fn injects_no_spawn_for_empty_brick_set() {
        let world = bricks_to_save(vec![]);
        assert!(world.bricks.is_empty(), "no spawn point should be injected when there are no bricks");
    }
}
