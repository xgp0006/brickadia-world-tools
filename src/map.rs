use image::RgbaImage;
use std::{
    path::{Path, PathBuf},
    result::Result,
};

use crate::util::to_linear_rgb;

// generic heightmap trait returns scalar from X and Y
pub trait Heightmap {
    fn at(&self, x: u32, y: u32) -> u32;
    fn size(&self) -> (u32, u32);
}

// generic colormap trait returns color from X and Y
pub trait Colormap {
    fn at(&self, x: u32, y: u32) -> [u8; 4];
    fn size(&self) -> (u32, u32);
}

// PNG based heightmaps
pub struct HeightmapPNG {
    maps: Vec<RgbaImage>,
    rgba_encoded: bool,
}

// Heightmap lookup
impl Heightmap for HeightmapPNG {
    fn at(&self, x: u32, y: u32) -> u32 {
        if self.rgba_encoded {
            self.maps
                .iter()
                .fold(0, |sum, m| sum + u32::from_be_bytes(m.get_pixel(x, y).0))
        } else {
            self.maps
                .iter()
                .fold(0, |sum, m| sum + m.get_pixel(x, y).0[0] as u32)
        }
    }

    fn size(&self) -> (u32, u32) {
        (self.maps[0].width(), self.maps[0].height())
    }
}

// Heightmap image input
impl HeightmapPNG {
    pub fn new(images: Vec<&PathBuf>, rgba_encoded: bool) -> Result<Self, String> {
        if images.is_empty() {
            return Err("HeightmapPNG requires at least one image".to_string());
        }

        // read in the maps
        let mut maps: Vec<RgbaImage> = vec![];
        for file in images {
            if let Ok(img) = image::open(file) {
                maps.push(img.to_rgba8());
            } else {
                return Err(format!("Could not open image {}", file.display()));
            }
        }

        // check to ensure all images have the same dimensions
        let height = maps[0].height();
        let width = maps[0].width();
        for m in &maps {
            if m.height() != height || m.width() != width {
                return Err("Mismatched heightmap sizes".to_string());
            }
        }

        // return a reference to save on memory
        Ok(HeightmapPNG { maps, rgba_encoded })
    }
}

// A completely flat heightmap
pub struct HeightmapFlat {
    width: u32,
    height: u32,
}

// The heightmap always returns 1... because it's flat
impl Heightmap for HeightmapFlat {
    fn at(&self, _x: u32, _y: u32) -> u32 {
        1
    }

    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

// Flat heightmap just has dimensions
impl HeightmapFlat {
    pub fn new((width, height): (u32, u32)) -> Result<Self, String> {
        // return a reference to save on memory
        Ok(HeightmapFlat { width, height })
    }
}

// PNG based colormap
pub struct ColormapPNG {
    source: RgbaImage,
    lrgb: bool,
}

// Read in a color from X, Y
impl Colormap for ColormapPNG {
    fn at(&self, x: u32, y: u32) -> [u8; 4] {
        if self.lrgb {
            self.source.get_pixel(x, y).0
        } else {
            to_linear_rgb(self.source.get_pixel(x, y).0)
        }
    }

    fn size(&self) -> (u32, u32) {
        (self.source.width(), self.source.height())
    }
}

// Colormap image input
impl ColormapPNG {
    pub fn new(file: impl AsRef<Path>, lrgb: bool) -> Result<Self, String> {
        if let Ok(img) = image::open(&file) {
            Ok(ColormapPNG {
                source: img.to_rgba8(),
                lrgb,
            })
        } else {
            Err(format!("Could not open image {}", file.as_ref().display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    /// Writes `img` as a PNG into the OS temp dir under a name unique to this
    /// process and test, and returns the path. Caller removes the file.
    fn write_png(name: &str, img: &RgbaImage) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "h2brz_map_test_{}_{name}.png",
            std::process::id()
        ));
        img.save(&path).expect("test PNG must be writable");
        path
    }

    #[test]
    fn heightmap_png_decodes_rgba_encoded_pixel_as_big_endian_u32() {
        let mut img = RgbaImage::new(2, 1);
        img.put_pixel(0, 0, Rgba([0x00, 0x01, 0x02, 0x03]));
        img.put_pixel(1, 0, Rgba([0x0A, 0x14, 0x1E, 0x28]));
        let path = write_png("rgba_encoded", &img);

        let map = HeightmapPNG::new(vec![&path], true).expect("heightmap must load");
        // hand-computed: [0x00,0x01,0x02,0x03] big-endian -> 0x00010203 = 66051
        assert_eq!(map.at(0, 0), 66051);
        // [0x0A,0x14,0x1E,0x28] -> 0x0A141E28 = 169_090_600
        assert_eq!(map.at(1, 0), 169_090_600);
        assert_eq!(map.size(), (2, 1));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn heightmap_png_grayscale_mode_reads_red_channel_and_sums_maps() {
        let mut a = RgbaImage::new(1, 2);
        a.put_pixel(0, 0, Rgba([200, 5, 9, 255]));
        a.put_pixel(0, 1, Rgba([7, 90, 90, 255]));
        let mut b = RgbaImage::new(1, 2);
        b.put_pixel(0, 0, Rgba([40, 1, 2, 255]));
        b.put_pixel(0, 1, Rgba([3, 80, 80, 255]));
        let path_a = write_png("gray_a", &a);
        let path_b = write_png("gray_b", &b);

        let single = HeightmapPNG::new(vec![&path_a], false).expect("heightmap must load");
        assert_eq!(single.at(0, 0), 200, "red channel only, not G/B/A");
        assert_eq!(single.at(0, 1), 7);

        let stacked =
            HeightmapPNG::new(vec![&path_a, &path_b], false).expect("heightmaps must load");
        assert_eq!(stacked.at(0, 0), 240, "multiple maps sum their red channels");
        assert_eq!(stacked.at(0, 1), 10);
        assert_eq!(stacked.size(), (1, 2));

        let _ = std::fs::remove_file(path_a);
        let _ = std::fs::remove_file(path_b);
    }

    #[test]
    fn heightmap_png_rejects_empty_input_and_mismatched_sizes() {
        assert!(HeightmapPNG::new(vec![], false).is_err());

        let small = write_png("mismatch_small", &RgbaImage::new(1, 1));
        let large = write_png("mismatch_large", &RgbaImage::new(2, 1));
        let result = HeightmapPNG::new(vec![&small, &large], false);
        assert_eq!(result.err().as_deref(), Some("Mismatched heightmap sizes"));

        let _ = std::fs::remove_file(small);
        let _ = std::fs::remove_file(large);
    }

    #[test]
    fn heightmap_flat_reports_size_and_unit_height_everywhere() {
        let flat = HeightmapFlat::new((7, 9)).expect("flat heightmap is infallible");
        assert_eq!(flat.size(), (7, 9));
        assert_eq!(flat.at(0, 0), 1);
        assert_eq!(flat.at(6, 8), 1);
    }
}
