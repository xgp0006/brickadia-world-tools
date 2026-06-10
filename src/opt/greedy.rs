/// BitMask with arbitrary precision using Vec<u128>
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitMask {
    /// Stores bits in little-endian order (bits[0] contains bits 0-127)
    bits: Vec<u128>,
}

impl Default for BitMask {
    fn default() -> Self {
        Self::new()
    }
}

impl BitMask {
    /// Create a new empty bitmask
    pub fn new() -> Self {
        BitMask { bits: vec![0] }
    }

    /// Create a bitmask from a single u128 value (test fixture constructor)
    #[cfg(test)]
    fn from_u128(value: u128) -> Self {
        BitMask { bits: vec![value] }
    }

    /// Create a mask with `count` consecutive ones starting from bit 0
    pub fn ones(count: u32) -> Self {
        if count == 0 {
            return BitMask::new();
        }

        let full_blocks = count / 128;
        let remaining = count % 128;

        let mut bits = vec![u128::MAX; full_blocks as usize];
        if remaining > 0 {
            bits.push((1u128 << remaining) - 1);
        }

        BitMask { bits }
    }

    /// Left shift operation
    pub fn shl(&self, shift: u32) -> Self {
        if shift == 0 {
            return self.clone();
        }

        let block_shift = (shift / 128) as usize;
        let bit_shift = shift % 128;

        if bit_shift == 0 {
            // Simple block shift
            let mut result = vec![0u128; block_shift];
            result.extend_from_slice(&self.bits);
            return BitMask { bits: result };
        }

        // Complex shift with bit spillover
        let mut result = vec![0u128; block_shift + self.bits.len() + 1];
        let right_shift = 128 - bit_shift;

        for (i, &block) in self.bits.iter().enumerate() {
            result[i + block_shift] |= block << bit_shift;
            if i + block_shift + 1 < result.len() {
                result[i + block_shift + 1] |= block >> right_shift;
            }
        }

        // Remove trailing zeros
        while result.len() > 1 && result.last() == Some(&0) {
            result.pop();
        }

        BitMask { bits: result }
    }

    /// Right shift operation
    pub fn shr(&self, shift: u32) -> Self {
        if shift == 0 {
            return self.clone();
        }

        let block_shift = (shift / 128) as usize;
        if block_shift >= self.bits.len() {
            return BitMask::new();
        }

        let bit_shift = shift % 128;

        if bit_shift == 0 {
            // Simple block shift
            return BitMask {
                bits: self.bits[block_shift..].to_vec(),
            };
        }

        // Complex shift with bit spillover: each output block combines the
        // low bits of its source block with the high bits of the next one.
        let left_shift = 128 - bit_shift;
        let src = &self.bits[block_shift..];
        let mut result: Vec<u128> = src
            .iter()
            .enumerate()
            .map(|(i, &block)| {
                let next = src.get(i + 1).copied().unwrap_or(0);
                (block >> bit_shift) | (next << left_shift)
            })
            .collect();

        // Remove trailing zeros
        while result.len() > 1 && result.last() == Some(&0) {
            result.pop();
        }

        BitMask { bits: result }
    }

    /// Bitwise AND operation
    pub fn and(&self, other: &BitMask) -> Self {
        // zip truncates to the shorter operand; missing blocks AND to zero anyway
        let mut result: Vec<u128> = self
            .bits
            .iter()
            .zip(&other.bits)
            .map(|(a, b)| a & b)
            .collect();

        // Remove trailing zeros
        while result.len() > 1 && result.last() == Some(&0) {
            result.pop();
        }

        BitMask { bits: result }
    }

    /// Bitwise XOR operation (mutating)
    pub fn xor_assign(&mut self, other: &BitMask) {
        let max_len = self.bits.len().max(other.bits.len());

        // Extend self if needed
        if self.bits.len() < max_len {
            self.bits.resize(max_len, 0);
        }

        // XOR in place
        for (block, &b) in self.bits.iter_mut().zip(&other.bits) {
            *block ^= b;
        }

        // Remove trailing zeros
        while self.bits.len() > 1 && self.bits.last() == Some(&0) {
            self.bits.pop();
        }
    }

    /// Count trailing zeros starting from a specific bit offset (avoids allocating a shifted BitMask)
    pub fn trailing_zeros_from(&self, offset: u32) -> u32 {
        let block_index = (offset / 128) as usize;
        let bit_offset = offset % 128;

        if block_index >= self.bits.len() {
            return 0;
        }

        // Check first block with offset
        let first_block = self.bits[block_index] >> bit_offset;
        if first_block != 0 {
            return first_block.trailing_zeros();
        }

        let mut count = 128 - bit_offset;

        // Check remaining blocks
        for &block in &self.bits[block_index + 1..] {
            if block != 0 {
                return count + block.trailing_zeros();
            }
            count += 128;
        }

        count
    }

    /// Count trailing ones starting from a specific bit offset (avoids allocating a shifted BitMask)
    pub fn trailing_ones_from(&self, offset: u32) -> u32 {
        let block_index = (offset / 128) as usize;
        let bit_offset = offset % 128;

        if block_index >= self.bits.len() {
            return 0;
        }

        // Check first block with offset
        let first_block = self.bits[block_index] >> bit_offset;
        let mask = u128::MAX >> bit_offset;

        if first_block != mask {
            return first_block.trailing_ones();
        }

        let mut count = 128 - bit_offset;

        // Check remaining blocks
        for &block in &self.bits[block_index + 1..] {
            if block != u128::MAX {
                return count + block.trailing_ones();
            }
            count += 128;
        }

        count
    }

    /// Set a specific bit to 1 (mutating)
    pub fn set_bit(&mut self, bit_index: u32) {
        let block_index = (bit_index / 128) as usize;
        let bit_offset = bit_index % 128;

        // Extend the vector if necessary
        if block_index >= self.bits.len() {
            self.bits.resize(block_index + 1, 0);
        }

        self.bits[block_index] |= 1u128 << bit_offset;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GreedyQuad {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Generate quads from a binary plane represented as BitMask bitmasks
/// Each BitMask in the vector represents a row, with each bit representing a column
/// max_size limits the maximum width/height of quads (in pixels before scaling)
pub fn greedy_mesh_binary_plane(
    mut data: Vec<BitMask>,
    max_x: u32,
    max_y: u32,
    max_size: u32,
) -> Vec<GreedyQuad> {
    let mut greedy_quads = vec![];

    for row in 0..data.len().min(max_x as usize) {
        let mut y = 0;
        while y < max_y {
            // Find first solid bit, skip "air/zeros" - use optimized version
            let trailing_zeros = data[row].trailing_zeros_from(y).min(max_y - y);
            y += trailing_zeros;

            if y >= max_y {
                // Reached top
                break;
            }

            // Count consecutive ones (height) - use optimized version
            // Limit height to max_size
            let h = data[row].trailing_ones_from(y).min(max_y - y).min(max_size);

            if h == 0 {
                // No ones found, skip this position
                y += 1;
                continue;
            }

            // Create mask for the height once
            let h_as_mask = BitMask::ones(h);
            let mask = h_as_mask.shl(y);

            // Grow horizontally (limit to max_size)
            let mut w = 1;
            while row + w < data.len() && w < max_size as usize {
                // Fetch bits spanning height in the next row
                let next_row_bits = data[row + w].shr(y).and(&h_as_mask);

                if next_row_bits != h_as_mask {
                    break; // Can no longer expand
                }

                // Clear the bits we expanded into by XORing with the mask (mutating)
                data[row + w].xor_assign(&mask);

                w += 1;
            }

            greedy_quads.push(GreedyQuad {
                x: row as u32,
                y,
                w: w as u32,
                h,
            });

            // Clear the bits from the current row (mutating)
            data[row].xor_assign(&mask);

            y += h;
        }
    }

    greedy_quads
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bitmask_basic() {
        let mask = BitMask::from_u128(0b1111);
        assert_eq!(mask.trailing_zeros_from(0), 0);
        assert_eq!(mask.trailing_ones_from(0), 4);
    }

    #[test]
    fn test_bitmask_shift_left() {
        let mask = BitMask::from_u128(0b1111);
        let shifted = mask.shl(2);
        // After shifting left by 2: 0b1111 -> 0b111100
        // This has 2 trailing zeros, then 4 ones starting at bit 2
        assert_eq!(shifted.trailing_zeros_from(0), 2);
        assert_eq!(shifted.trailing_ones_from(2), 4);
    }

    #[test]
    fn test_bitmask_shift_right() {
        let mask = BitMask::from_u128(0b111100);
        let shifted = mask.shr(2);
        assert_eq!(shifted.trailing_zeros_from(0), 0);
        assert_eq!(shifted.trailing_ones_from(0), 4);
    }

    #[test]
    fn test_bitmask_ones() {
        let mask = BitMask::ones(5);
        assert_eq!(mask.trailing_zeros_from(0), 0);
        assert_eq!(mask.trailing_ones_from(0), 5);
    }

    #[test]
    fn test_bitmask_and() {
        let a = BitMask::from_u128(0b1111);
        let b = BitMask::from_u128(0b1100);
        let result = a.and(&b);
        assert_eq!(result, BitMask::from_u128(0b1100));
    }

    #[test]
    fn test_bitmask_xor_assign() {
        let mut a = BitMask::from_u128(0b1111);
        a.xor_assign(&BitMask::from_u128(0b1100));
        assert_eq!(a, BitMask::from_u128(0b0011));
    }

    #[test]
    fn test_greedy_mesh_simple() {
        // Create a simple 2x2 solid block
        // Row 0: 0b11 (bits 0-1 set)
        // Row 1: 0b11 (bits 0-1 set)
        let data = vec![BitMask::from_u128(0b11), BitMask::from_u128(0b11)];

        let quads = greedy_mesh_binary_plane(data, 2, 2, 1000);

        // Should produce a single 2x2 quad
        assert_eq!(quads.len(), 1);
        assert_eq!(
            quads[0],
            GreedyQuad {
                x: 0,
                y: 0,
                w: 2,
                h: 2
            }
        );
    }

    #[test]
    fn test_greedy_mesh_separate_quads() {
        // Create two separate quads
        // Row 0: 0b1100 (bits 2-3 set)
        // Row 1: 0b0011 (bits 0-1 set)
        let data = vec![BitMask::from_u128(0b1100), BitMask::from_u128(0b0011)];

        let quads = greedy_mesh_binary_plane(data, 2, 4, 1000);

        // Should produce two separate quads
        assert_eq!(quads.len(), 2);
    }

    /// Count how many quads cover each cell of a w x h grid.
    fn coverage_counts(quads: &[GreedyQuad], w: u32, h: u32) -> Vec<u32> {
        let mut counts = vec![0u32; (w * h) as usize];
        for q in quads {
            for x in q.x..q.x + q.w {
                for y in q.y..q.y + q.h {
                    counts[(x * h + y) as usize] += 1;
                }
            }
        }
        counts
    }

    #[test]
    fn greedy_mesh_tiles_l_shape_exactly_once() {
        // 8x8 plane with an L-shape: rows 0-2 fully set (y 0..8), plus
        // rows 3-7 set only at y 0..2. Quads must cover every set cell
        // exactly once and never touch an unset cell.
        const W: u32 = 8;
        const H: u32 = 8;
        let set = |x: u32, y: u32| -> bool { x < 3 || y < 2 };

        let mut data = vec![BitMask::new(); W as usize];
        for (x, row) in data.iter_mut().enumerate() {
            for y in 0..H {
                if set(x as u32, y) {
                    row.set_bit(y);
                }
            }
        }

        let quads = greedy_mesh_binary_plane(data, W, H, 1000);
        let counts = coverage_counts(&quads, W, H);

        for x in 0..W {
            for y in 0..H {
                let expected = u32::from(set(x, y));
                assert_eq!(
                    counts[(x * H + y) as usize],
                    expected,
                    "cell ({x},{y}) covered {} times, expected {expected}",
                    counts[(x * H + y) as usize]
                );
            }
        }
    }

    #[test]
    fn greedy_mesh_clamps_quads_to_max_size() {
        // A fully solid 16x16 plane with max_size 4 must still tile every
        // cell exactly once, but no quad may exceed 4 in either dimension.
        // Removing the max_size clamp would emit a single 16x16 quad.
        const W: u32 = 16;
        const H: u32 = 16;
        const MAX: u32 = 4;

        let mut data = vec![BitMask::new(); W as usize];
        for row in data.iter_mut() {
            for y in 0..H {
                row.set_bit(y);
            }
        }

        let quads = greedy_mesh_binary_plane(data, W, H, MAX);

        assert!(!quads.is_empty());
        for q in &quads {
            assert!(
                q.w <= MAX && q.h <= MAX,
                "quad {q:?} exceeds max_size {MAX}"
            );
        }

        let counts = coverage_counts(&quads, W, H);
        assert!(
            counts.iter().all(|&c| c == 1),
            "clamped quads must still tile the plane exactly once"
        );
    }
}
