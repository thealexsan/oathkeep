use super::*;


/// In [`Submit::lists`].
pub const MISSING: u32 = u32::MAX;

wire_struct! {
    /// Contents at the start of the frame, in `contents.bin`: of a buffer,
    /// `len` bytes from `first`; of a texture, `count` subresources from
    /// `first`, laid out as `GetCopyableFootprints` lays them out.
    Content {
        res: u32,
        first: u64,
        count: u32,
        len: u64,
        /// Where in the file; [`ZERO`] if all bytes are zero.
        file: u64,
    }
}

/// In [`Content::file`].
pub const ZERO: u64 = u64::MAX;

wire_struct! {
    /// A frame's picture: a texture as it was when presented (in
    /// `present.bin`).
    Present { submits: u32, res: u32, width: u32, height: u32, format: i32, pitch: u32, file: u64 }
}

wire_struct! {
    Frame {
        adapter: String,
        vendor: u32,
        device: u32,
        /// What the recorder could not record, and how it went.
        notes: Vec<String>,
        heaps: Vec<Heap>,
        resources: Vec<Resource>,
        roots: Vec<(u32, Blob)>,
        codes: Vec<Code>,
        pipelines: Vec<Pipeline>,
        signatures: Vec<Signature>,
        descriptor_heaps: Vec<DescriptorHeap>,
        query_heaps: Vec<QueryHeap>,
        lists: Vec<List>,
        submits: Vec<Submit>,
        contents: Vec<Content>,
        presents: Vec<Present>,
    }
}

impl Frame {
    pub fn write(&self, path: &std::path::Path) -> std::io::Result<()> {
        let mut out = MAGIC.to_vec();
        VERSION.put(&mut out);
        self.put(&mut out);
        std::fs::write(path, out)
    }

    pub fn read(path: &std::path::Path) -> anyhow::Result<Frame> {
        let bytes = std::fs::read(path)?;
        let mut from = Dec::new(&bytes);
        if from.take(8)? != MAGIC {
            return Err(Bad("it is not a frame").into());
        }
        if u32::get(&mut from)? != VERSION {
            return Err(Bad("it was made by another version of the recorder").into());
        }
        let frame = Frame::get(&mut from)?;
        if !from.bytes.is_empty() {
            return Err(Bad("bytes after its end").into());
        }
        Ok(frame)
    }
}

