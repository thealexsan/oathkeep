use super::{Bad, Dec, Wire};

/// No object.
pub const NONE: u32 = 0;
/// All subresources.
pub const ALL: u32 = u32::MAX;

/// What a descriptor slot holds: the view last made in it. `res` and
/// `counter` are object ids; `desc` the view's description as D3D12 lays it
/// out (a sampler's is the longest: 52 bytes). In a capture file a constant
/// buffer view's buffer is in `res` and, in place of the address, the
/// offset in it.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Slot {
    pub kind: u8,
    /// Whether a description was given.
    pub described: u8,
    pub pad: u16,
    pub res: u32,
    pub counter: u32,
    pub desc: [u8; 52],
}

/// Kinds of [`Slot`].
pub mod slot {
    pub const EMPTY: u8 = 0;
    pub const CBV: u8 = 1;
    pub const SRV: u8 = 2;
    pub const UAV: u8 = 3;
    pub const SAMPLER: u8 = 4;
    pub const RTV: u8 = 5;
    pub const DSV: u8 = 6;
}

impl Slot {
    pub const EMPTY: Slot = Slot {
        kind: slot::EMPTY,
        described: 0,
        pad: 0,
        res: NONE,
        counter: NONE,
        desc: [0; 52],
    };

    /// A slot of `kind` with the description at `desc` (`size` bytes), if any.
    ///
    /// # Safety
    /// `desc` is null or points at a description of `size` bytes.
    pub unsafe fn new(kind: u8, res: u32, counter: u32, desc: *const u8, size: usize) -> Slot {
        let mut s = Slot {
            kind,
            res,
            counter,
            ..Slot::EMPTY
        };
        if !desc.is_null() {
            s.described = 1;
            // SAFETY: the caller's description, at most 52 bytes of it.
            unsafe { std::ptr::copy_nonoverlapping(desc, s.desc.as_mut_ptr(), size.min(52)) };
        }
        s
    }

    /// A constant buffer view's address (or its offset, in a file).
    pub fn address(&self) -> u64 {
        u64::from_le_bytes([
            self.desc[0],
            self.desc[1],
            self.desc[2],
            self.desc[3],
            self.desc[4],
            self.desc[5],
            self.desc[6],
            self.desc[7],
        ])
    }

    pub fn set_address(&mut self, address: u64) {
        self.desc[..8].copy_from_slice(&address.to_le_bytes());
    }
}

impl Wire for Slot {
    fn put(&self, out: &mut Vec<u8>) {
        out.push(self.kind);
        out.push(self.described);
        self.res.put(out);
        self.counter.put(out);
        // Most descriptions are 40 bytes or less.
        let n = self.desc.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        out.push(n as u8);
        out.extend_from_slice(&self.desc[..n]);
    }
    fn get(from: &mut Dec) -> Result<Self, Bad> {
        let (kind, described) = (u8::get(from)?, u8::get(from)?);
        let (res, counter) = (u32::get(from)?, u32::get(from)?);
        let n = u8::get(from)? as usize;
        if n > 52 {
            return Err(Bad("a view's description"));
        }
        let mut desc = [0; 52];
        desc[..n].copy_from_slice(from.take(n)?);
        Ok(Slot {
            kind,
            described,
            pad: 0,
            res,
            counter,
            desc,
        })
    }
}

wire_enum! {
    /// A GPU address.
    Va {
        0 Null {},
        1 At { res: u32, offset: u64 },
        /// In no buffer the recorder knows.
        2 Unknown { address: u64 },
    }
}

wire_enum! {
    /// One side of a texture copy.
    Loc {
        0 Sub { res: u32, sub: u32 },
        1 Placed { res: u32, offset: u64, format: i32, size: [u32; 3], pitch: u32 },
    }
}

wire_enum! {
    /// One barrier of a `ResourceBarrier` call.
    Barrier {
        0 Transition { flags: i32, res: u32, sub: u32, before: i32, after: i32 },
        1 Alias { flags: i32, before: u32, after: u32 },
        2 Uav { flags: i32, res: u32 },
    }
}

