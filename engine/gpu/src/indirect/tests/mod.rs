use super::*;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::{Interface, s};


mod batched;
mod budget;
mod card_blocks;
mod card_probes;
mod card_reads;
mod clamp;
mod window;

const WORKER_CS: &str = r#"
#define RS "UAV(u0)"
RWByteAddressBuffer hits : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() {
uint prev;
hits.InterlockedAdd(0, 1, prev);
}
"#;

pub(super) struct WarpHarness {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    submitted: u64,
    event: HANDLE,
    clamp: Clamp,
    worker: Clamp,
    /// Last, so given up last.
    _turn: std::sync::MutexGuard<'static, ()>,
}

/// The software adapter is one test's at a time. Two tests that compile
/// and run shaders on it at once (it is one device for the process) make
/// `d3d10warp.dll` (10.0.26100) crash now and then: access violations in
/// its own shader compiler, on its own worker threads, in 12% of runs of
/// all tests and 3% of runs of these alone on two threads; none in 100
/// runs one after the other.
pub(crate) fn warp_turn() -> std::sync::MutexGuard<'static, ()> {
    static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
    TURN.lock().unwrap_or_else(|e| e.into_inner())
}

impl WarpHarness {
    pub(super) fn new() -> Option<Self> {
        Self::on(true)
    }

    /// On the software adapter, or the first one (for measurements).
    pub(super) fn on(software: bool) -> Option<Self> {
        let turn = warp_turn();
        // SAFETY: plain DXGI query and D3D12 test harness setup.
        unsafe {
            let factory: IDXGIFactory4 =
                CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
            let adapter: IDXGIAdapter = if software {
                factory.EnumWarpAdapter().ok()?
            } else {
                factory.EnumAdapters(0).ok()?
            };
            let desc = adapter.cast::<IDXGIAdapter1>().ok()?.GetDesc1().ok()?;
            if software != (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0) {
                return None;
            }
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            let device = device?;
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .ok()?;
            let allocator: ID3D12CommandAllocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .ok()?;
            let list: ID3D12GraphicsCommandList = device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                .ok()?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
            let event = CreateEventW(None, false, false, None).ok()?;
            let clamp = make_for(&device).expect("clamp shader must compile on WARP");
            let worker = compile_cs(&device, WORKER_CS).expect("worker shader must compile");
            Some(Self {
                device,
                queue,
                allocator,
                list,
                fence,
                submitted: 0,
                event,
                clamp,
                worker,
                _turn: turn,
            })
        }
    }

    /// CPU-mapped L0 write-back buffer matching `Shadow`'s heap type.
    pub(super) fn shared_buffer(&self, size: u64) -> (ID3D12Resource, *mut u8, u64) {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(4),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain buffer creation and persistent CPU mapping.
        unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut buffer,
                )
                .unwrap();
            let buffer = buffer.unwrap();
            buffer.Map(0, None, Some(&mut data)).unwrap();
            let gpu = buffer.GetGPUVirtualAddress();
            (buffer, data.cast(), gpu)
        }
    }

    /// Device-local `DEFAULT`-heap UAV scratch buffer.
    pub(super) fn default_uav(&self, size: u64) -> ID3D12Resource {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(64 << 10),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: plain device-local buffer creation.
        unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut buffer,
                )
                .unwrap();
        }
        buffer.unwrap()
    }

    pub(super) fn signature(&self, kind: u32, stride: u32) -> ID3D12CommandSignature {
        let arg = D3D12_INDIRECT_ARGUMENT_DESC {
            Type: D3D12_INDIRECT_ARGUMENT_TYPE(kind as i32),
            ..Default::default()
        };
        let desc = D3D12_COMMAND_SIGNATURE_DESC {
            ByteStride: stride,
            NumArgumentDescs: 1,
            pArgumentDescs: &arg,
            NodeMask: 0,
        };
        let mut sig: Option<ID3D12CommandSignature> = None;
        // SAFETY: valid single-argument command signature description.
        unsafe {
            self.device
                .CreateCommandSignature(&desc, None::<&ID3D12RootSignature>, &mut sig)
                .unwrap();
        }
        sig.unwrap()
    }

    pub(super) fn submit_and_wait(&mut self) {
        self.submitted += 1;
        // SAFETY: closing, executing, waiting on fence, and resetting our list.
        unsafe {
            self.list.Close().unwrap();
            let cmd: ID3D12CommandList = self.list.cast().unwrap();
            self.queue.ExecuteCommandLists(&[Some(cmd)]);
            self.queue.Signal(&self.fence, self.submitted).unwrap();
            self.fence
                .SetEventOnCompletion(self.submitted, self.event)
                .unwrap();
            WaitForSingleObject(self.event, INFINITE);
            self.allocator.Reset().unwrap();
            self.list.Reset(&self.allocator, None).unwrap();
        }
    }
}

pub(super) fn compile_cs(device: &ID3D12Device, source: &str) -> windows::core::Result<Clamp> {
    let mut code: Option<ID3DBlob> = None;
    // SAFETY: source and entry names outlive `D3DCompile`.
    unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            s!("worker"),
            None,
            None,
            s!("main"),
            s!("cs_5_1"),
            0,
            0,
            &mut code,
            None,
        )?;
        let code = code.unwrap();
        let bytes = std::slice::from_raw_parts(
            code.GetBufferPointer().cast::<u8>(),
            code.GetBufferSize(),
        );
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
        let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: bytes.as_ptr().cast(),
                BytecodeLength: bytes.len(),
            },
            ..Default::default()
        })?;
        Ok(Clamp { root, pso })
    }
}


#[test]
#[ignore]
fn new_blocks_under_readers() {
    card_blocks::new_blocks_under_readers();
}
