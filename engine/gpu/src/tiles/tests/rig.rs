use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::Interface;

use super::super::tiled::Tiled;
use crate::host::{transition, warp_adapter};

pub(super) const SIDE: u32 = 1024;
pub(super) const LEVELS: u16 = 11;

/// Two views of one texture read at the same place and level: the
/// first as the layer makes it for a texture whose level 0 has no
/// memory, the second as the title asked.
pub(super) const LOOK: &str = r#"
#define RS "DescriptorTable(SRV(t0, numDescriptors = 2)), UAV(u0), StaticSampler(s0, filter = FILTER_MIN_MAG_MIP_POINT)"
Texture2D<float4> clamped : register(t0);
Texture2D<float4> whole : register(t1);
SamplerState nearest : register(s0);
RWByteAddressBuffer words : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void look() {
    float4 a = clamped.SampleLevel(nearest, float2(0.3, 0.3), 0);
    float4 b = whole.SampleLevel(nearest, float2(0.3, 0.3), 0);
    words.Store(0, (uint)(a.x * 255.0 + 0.5));
    words.Store(4, (uint)(b.x * 255.0 + 0.5));
}
"#;

/// Where each level of a texture lies in a buffer of all of them: its
/// place, its rows, the bytes of a row that are the level's own, and
/// the buffer's bytes.
pub(super) struct Laid {
    pub(super) footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    wide: Vec<u64>,
    total: u64,
}

pub(super) struct Rig {
    pub(super) device: ID3D12Device,
    pub(super) queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    event: HANDLE,
    submitted: u64,
    /// The debug layer's messages, if Windows has the layer.
    messages: Option<ID3D12InfoQueue>,
    /// What the test is doing, for the message when it stops.
    pub(super) step: &'static str,
    /// On the card, not on the software adapter.
    pub(super) card: bool,
}

impl Rig {
    /// The software adapter, or the default one with
    /// `OATHKEEP_TILES_TEST_CARD=1`; `None` without one, without tiled
    /// resources of tier 2, or on the card without the debug layer.
    pub(super) fn new() -> Option<Rig> {
        // SAFETY: plain object creation on a device of our own.
        unsafe {
            let card = std::env::var_os("OATHKEEP_TILES_TEST_CARD").is_some_and(|v| v == "1");
            // The debug layer, before the device is made: a call that
            // is not well formed is then refused and named by the
            // runtime, not handed to the driver.
            let mut layer: Option<ID3D12Debug> = None;
            let layered = D3D12GetDebugInterface(&mut layer).is_ok() && layer.is_some();
            layer.iter().for_each(|l| l.EnableDebugLayer());
            if card && !layered {
                eprintln!("no D3D12 debug layer (Windows optional feature \"Graphics Tools\"): not run on the card without it");
                return None;
            }
            let mut device: Option<ID3D12Device> = None;
            if card {
                D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            } else {
                let adapter = warp_adapter().ok()?;
                D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            }
            let device = device?;
            let messages = device.cast::<ID3D12InfoQueue>().ok();
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
            device
                .CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS,
                    (&raw mut options).cast(),
                    size_of_val(&options) as u32,
                )
                .ok()?;
            eprintln!(
                "levels on demand, {}: tiled resources tier {}, debug layer {}",
                if card { "ON THE CARD" } else { "on the software adapter" },
                options.TiledResourcesTier.0,
                if messages.is_some() { "on" } else { "not installed" }
            );
            if options.TiledResourcesTier.0 < 2 {
                return None;
            }
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .ok()?;
            let allocator: ID3D12CommandAllocator =
                device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).ok()?;
            let list: ID3D12GraphicsCommandList = device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                .ok()?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
            let event = CreateEventW(None, false, false, None).ok()?;
            Some(Rig {
                device,
                queue,
                allocator,
                list,
                fence,
                event,
                submitted: 0,
                messages,
                step: "making the device",
                card,
            })
        }
    }

    /// Prints what the debug layer said since the last call; the first
    /// error among it (as `host.rs` `log_messages` reads them).
    fn said(&self) -> Option<String> {
        let queue = self.messages.as_ref()?;
        let mut first_error = None;
        // SAFETY: each message is read into a buffer of the size the
        // queue reports, then the queue is emptied.
        unsafe {
            for i in 0..queue.GetNumStoredMessages() {
                let mut size = 0;
                if queue.GetMessage(i, None, &mut size).is_err() || size == 0 {
                    continue;
                }
                let mut buffer = vec![0u64; size.div_ceil(8)];
                let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
                if queue.GetMessage(i, Some(message), &mut size).is_ok() {
                    let m = &*message;
                    let text = std::slice::from_raw_parts(m.pDescription, m.DescriptionByteLength);
                    let text = String::from_utf8_lossy(text);
                    let text = text.trim_end_matches('\0');
                    eprintln!("D3D12 says (severity {}, id {}): {text}", m.Severity.0, m.ID.0);
                    if m.Severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 && first_error.is_none() {
                        first_error = Some(text.to_owned());
                    }
                }
            }
            queue.ClearStoredMessages();
        }
        first_error
    }

    /// Stops the test, naming the step, if the device is lost or the
    /// debug layer reported an error: nothing more is sent after it.
    pub(super) fn check(&self, what: &str) {
        let error = self.said();
        // SAFETY: a plain query.
        let removed = unsafe { self.device.GetDeviceRemovedReason() };
        assert!(removed.is_ok(), "{}, {what}: the device is lost: {removed:?}", self.step);
        assert!(error.is_none(), "{}, {what}: the debug layer reports an error: {}", self.step, error.unwrap_or_default());
    }

    /// Gives tiles as a submission would, and stops if the mapping
    /// call was refused.
    pub(super) fn give(&self, tiled: &Tiled, texture: &ID3D12Resource, levels: u32) {
        tiled.give_on(&self.queue, &self.device, texture, levels, levels);
        self.check("tiles given");
    }

    /// Records with `record`, runs the list behind what is already on
    /// the queue (tile mappings among it), waits for the card and
    /// opens the list again.
    pub(super) fn run(&mut self, record: impl FnOnce(&ID3D12GraphicsCommandList)) {
        record(&self.list);
        self.submitted += 1;
        // SAFETY: our list, queue and fence; the allocator is reset
        // only after the card finished everything recorded from it.
        unsafe {
            self.list.Close().unwrap();
            let run: ID3D12CommandList = self.list.cast().unwrap();
            self.queue.ExecuteCommandLists(&[Some(run)]);
            self.queue.Signal(&self.fence, self.submitted).unwrap();
            self.fence.SetEventOnCompletion(self.submitted, self.event).unwrap();
            // Never without end: a step is a few copies of a few
            // megabytes. On the card the limit stays under Windows' own
            // two seconds; the software adapter, which compiles its
            // shader on first use and resets nothing, gets longer.
            let limit = if self.card { 1500 } else { 20_000 };
            let waited = WaitForSingleObject(self.event, limit);
            self.check("a submission");
            assert!(
                waited == WAIT_OBJECT_0,
                "{}: a submission did not finish within {limit} ms; stopped before another is sent",
                self.step
            );
            self.allocator.Reset().unwrap();
            self.list.Reset(&self.allocator, None).unwrap();
        }
    }

    pub(super) fn buffer(&self, kind: D3D12_HEAP_TYPE, flags: D3D12_RESOURCE_FLAGS, state: D3D12_RESOURCE_STATES, bytes: u64) -> ID3D12Resource {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: kind,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: bytes,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: flags,
            ..Default::default()
        };
        let mut out: Option<ID3D12Resource> = None;
        // SAFETY: plain buffer creation.
        unsafe {
            self.device
                .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut out)
                .unwrap();
        }
        out.unwrap()
    }

    /// A square texture description of `side` texels and `levels`
    /// levels; `reserved`: in the layout a texture without memory of
    /// its own must have.
    pub(super) fn desc(format: DXGI_FORMAT, side: u32, levels: u16, reserved: bool) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(side),
            Height: side,
            DepthOrArraySize: 1,
            MipLevels: levels,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: if reserved { D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE } else { D3D12_TEXTURE_LAYOUT_UNKNOWN },
            ..Default::default()
        }
    }

    /// A texture without memory, in the state copies into it need, and
    /// how the card cuts it.
    pub(super) fn reserved(&self, desc: &D3D12_RESOURCE_DESC) -> (ID3D12Resource, Tiled) {
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: plain creation; a texture that is no target has no clear value.
        let asked = unsafe {
            self.device
                .CreateReservedResource(desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut made)
        };
        // The layer's reason first, if it refused.
        self.check("a texture without memory made");
        asked.expect("a reserved texture");
        let texture = made.expect("a reserved texture");
        let tiled = Tiled::make(&self.device, &texture, u32::from(desc.MipLevels)).expect("the card says how it cuts the texture");
        (texture, tiled)
    }

    pub(super) fn footprints(&self, desc: &D3D12_RESOURCE_DESC) -> Laid {
        let levels = usize::from(desc.MipLevels);
        let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); levels];
        let mut rows = vec![0u32; levels];
        let mut wide = vec![0u64; levels];
        let mut total = 0u64;
        // SAFETY: outputs for `levels` subresources.
        unsafe {
            self.device.GetCopyableFootprints(
                desc,
                0,
                levels as u32,
                0,
                Some(footprints.as_mut_ptr()),
                Some(rows.as_mut_ptr()),
                Some(wide.as_mut_ptr()),
                Some(&mut total),
            );
        }
        Laid {
            footprints,
            rows,
            wide,
            total,
        }
    }

    /// An upload buffer of `total` bytes in which the bytes of each
    /// level are `byte(level)`.
    pub(super) fn filled(&self, layout: &Laid, byte: impl Fn(usize) -> u8) -> ID3D12Resource {
        let (footprints, rows) = (&layout.footprints, &layout.rows);
        let upload = self.buffer(D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_GENERIC_READ, layout.total);
        // SAFETY: the buffer is `total` bytes; each level's rows lie inside it.
        unsafe {
            let mut data = std::ptr::null_mut();
            upload.Map(0, None, Some(&mut data)).unwrap();
            for (level, f) in footprints.iter().enumerate() {
                let len = rows[level] as usize * f.Footprint.RowPitch as usize;
                std::slice::from_raw_parts_mut(data.cast::<u8>().add(f.Offset as usize), len).fill(byte(level));
            }
            upload.Unmap(0, None);
        }
        upload
    }

    /// Copies every level of `texture` from `upload`, then every level
    /// back, and gives for each level the first sixteen bytes read of
    /// its first row and of the row and column half way (another tile
    /// of a level of several).
    pub(super) fn round_trip(
        &mut self,
        texture: &ID3D12Resource,
        layout: &Laid,
        upload: Option<&ID3D12Resource>,
    ) -> Vec<[Vec<u8>; 2]> {
        let (footprints, rows) = (&layout.footprints, &layout.rows);
        let back = self.buffer(D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST, layout.total);
        self.run(|list| {
            // SAFETY: an open list; the texture and buffers outlive the run.
            unsafe {
                if let Some(upload) = upload {
                    for (level, f) in footprints.iter().enumerate() {
                        list.CopyTextureRegion(&sub(texture, level as u32), 0, 0, 0, &placed(upload, *f), None);
                    }
                }
                list.ResourceBarrier(&[transition(texture, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                for (level, f) in footprints.iter().enumerate() {
                    list.CopyTextureRegion(&placed(&back, *f), 0, 0, 0, &sub(texture, level as u32), None);
                }
                list.ResourceBarrier(&[transition(texture, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST)]);
            }
        });
        let mut read = Vec::new();
        // SAFETY: a read-back buffer of `total` bytes the finished list copied into.
        unsafe {
            let mut data = std::ptr::null_mut();
            back.Map(0, None, Some(&mut data)).unwrap();
            for (level, f) in footprints.iter().enumerate() {
                let pitch = f.Footprint.RowPitch as usize;
                let len = rows[level] as usize * pitch;
                let bytes = std::slice::from_raw_parts(data.cast::<u8>().cast_const().add(f.Offset as usize), len);
                // The bytes of a row that are the level's own (its
                // blocks or texels; the rest of the pitch is padding).
                let own = layout.wide[level] as usize;
                let wide = own.min(len).min(16);
                let first = bytes[..wide].to_vec();
                let middle = (rows[level] as usize / 2) * pitch + ((own / 2) & !15);
                let far = if rows[level] >= 2 && own >= 32 && middle + wide <= len {
                    bytes[middle..middle + wide].to_vec()
                } else {
                    first.clone()
                };
                read.push([first, far]);
            }
            back.Unmap(0, None);
        }
        read
    }
}

pub(super) fn sub(texture: &ID3D12Resource, index: u32) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: index },
    }
}

pub(super) fn placed(buffer: &ID3D12Resource, footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(buffer) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprint },
    }
}

pub(super) fn all(bytes: &[Vec<u8>; 2], want: u8) -> bool {
    bytes.iter().all(|part| part.iter().all(|&b| b == want))
}

/// The levels from `first` on, a bit a level.
pub(super) fn from_level(first: u32) -> u32 {
    (u32::MAX << first) & ((1 << u32::from(LEVELS)) - 1)
}
