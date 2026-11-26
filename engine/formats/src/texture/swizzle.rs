use std::sync::OnceLock;

/// A swizzle mode, by its hardware number (addrlib's `ADDR_SW_*` order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Swizzle(pub u32);

impl Swizzle {
    pub const LINEAR: Swizzle = Swizzle(0);
    pub const SW_256B_S: Swizzle = Swizzle(1);
    pub const SW_256B_D: Swizzle = Swizzle(2);
    pub const SW_4KB_S: Swizzle = Swizzle(5);
    pub const SW_4KB_D: Swizzle = Swizzle(6);
    pub const SW_64KB_S: Swizzle = Swizzle(9);
    pub const SW_64KB_D: Swizzle = Swizzle(10);
    pub const SW_64KB_S_T: Swizzle = Swizzle(17);
    pub const SW_64KB_D_T: Swizzle = Swizzle(18);
    pub const SW_4KB_S_X: Swizzle = Swizzle(21);
    pub const SW_4KB_D_X: Swizzle = Swizzle(22);
    pub const SW_4KB_R_X: Swizzle = Swizzle(23);
    pub const SW_64KB_Z_X: Swizzle = Swizzle(24);
    pub const SW_64KB_S_X: Swizzle = Swizzle(25);
    pub const SW_64KB_D_X: Swizzle = Swizzle(26);
    pub const SW_64KB_R_X: Swizzle = Swizzle(27);

    /// log2 of the block size in bytes (0 for linear); `None` for modes
    /// GFX10 does not have.
    pub fn block_log2(self) -> Option<u32> {
        Some(match self.0 {
            0 => 0,
            1 | 2 => 8,
            5 | 6 | 21..=23 => 12,
            9 | 10 | 17 | 18 | 24..=27 => 16,
            _ => return None,
        })
    }

    pub(super) fn standard(self) -> bool {
        matches!(self.0, 1 | 5 | 9 | 17 | 21 | 25)
    }

    pub(super) fn display(self) -> bool {
        matches!(self.0, 2 | 6 | 10 | 18 | 22 | 26)
    }

    pub(super) fn z_order(self) -> bool {
        self.0 == 24
    }

    /// Pipe and bank XOR modes (`_T`, `_X`): their addresses depend on the
    /// GPU's pipe configuration.
    pub fn xor(self) -> bool {
        self.0 >= 16
    }
}


pub(super) type SwizzlePattern = [Vec<u32>; 3];
pub(super) static EMPTY_PATTERN: SwizzlePattern = [const { Vec::new() }; 3];

/// Micro tile (256 bytes) patterns by log2 of the element size: for each
/// address bit, the element coordinate bit it takes (`x0`, `y1`, `z0`...),
/// `--` for the bits inside an element. Thin `S`, thin `D`, thick `S`
/// (also the `D` 3D modes). From addrlib's `GFX10_SW_256_S_RBPLUS`,
/// `GFX10_SW_256_D_RBPLUS` and `GFX10_SW_4K_S3_RBPLUS` patterns; the larger
/// blocks add bits Y, X (thin) or Y, X, Z (thick) in turn.
const MICRO: [[&[u8; 16]; 5]; 3] = [
    [
        b"x0x1x2x3y0y1y2y3",
        b"--x0x1x2y0y1y2x3",
        b"----x0x1y0y1y2x2",
        b"------x0y0y1x1x2",
        b"--------y0y1x0x1",
    ],
    [
        b"x0x1x2y1y0y2x3y3",
        b"--x0x1x2y0y1y2x3",
        b"----x0x1y0y1x2y2",
        b"------x0y0x1x2y1",
        b"--------x0y0x1y1",
    ],
    [
        b"x0x1z0y0z1y1x2z2",
        b"--x0z0y0z1y1x1z2",
        b"----x0y0z0y1x1z1",
        b"------x0z0y0x1z1",
        b"--------z0y0x0z1",
    ],
];

/// For each axis, the address bit each coordinate bit lands on.
pub(super) fn pattern(kind: usize, element_log2: u32, block_log2: u32) -> [Vec<u32>; 3] {
    let mut axes: [Vec<u32>; 3] = Default::default();
    let micro = MICRO[kind][element_log2 as usize];
    for bit in 0..8u32 {
        let (axis, index) = (micro[2 * bit as usize], micro[2 * bit as usize + 1]);
        let axis = match axis {
            b'x' => 0,
            b'y' => 1,
            b'z' => 2,
            _ => continue,
        };
        let index = usize::from(index - b'0');
        let to = &mut axes[axis];
        if to.len() <= index {
            to.resize(index + 1, 0);
        }
        to[index] = bit;
    }
    let order: &[usize] = if kind == 2 { &[1, 0, 2] } else { &[1, 0] };
    for (bit, axis) in (8..block_log2).zip(order.iter().cycle()) {
        axes[*axis].push(bit);
    }
    axes
}

/// Byte offsets inside a block for coordinates 0..2^bits of one axis.
pub(super) fn spread(bits: &[u32]) -> Vec<u32> {
    (0..1u32 << bits.len())
        .map(|c| {
            bits.iter()
                .enumerate()
                .map(|(i, &to)| ((c >> i) & 1) << to)
                .sum()
        })
        .collect()
}

/// Tables depend only on the micro-tile kind, block size and element size.
/// Texture extents, mip placement and array slices only affect the base
/// address, so layouts with the same key can share these immutable tables.
pub(super) fn shared_pattern(kind: usize, element_log2: u32, block_log2: u32) -> &'static SwizzlePattern {
    // Indexed by MICRO kind, block size (256 B, 4 KiB, 64 KiB), then element size.
    static PATTERNS: [[[OnceLock<SwizzlePattern>; 5]; 3]; 3] =
        [const { [const { [const { OnceLock::new() }; 5] }; 3] }; 3];
    let block_index = ((block_log2 - 8) / 4) as usize;
    PATTERNS[kind][block_index][element_log2 as usize]
        .get_or_init(|| pattern(kind, element_log2, block_log2).map(|bits| spread(&bits)))
}
