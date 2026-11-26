//! Per-frame command recording and final outcome readback on `Scene`.

use oathkeep_gpu::replay::record;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::DXGI_PRESENT;
use windows::Win32::System::Threading::{INFINITE, WaitForSingleObject};
use windows::core::Interface;

use crate::d3d::{
    ARGUMENTS, COMMON, FROM, READ, SIZE, TARGET, TO, UAV, aliased, footprint, moved, rows,
    uav_barrier,
};
use crate::scene_init::Scene;

/// A texture the frames do not touch: what each subresource holds, row by
/// row, with the rows' distance in its copyable layout.
#[derive(PartialEq, Debug, Clone)]
pub struct Still {
    pub name: &'static str,
    pub subs: Vec<(usize, Vec<Vec<u8>>)>,
}

/// What the program ends with.
#[derive(PartialEq, Debug)]
pub struct Outcome {
    pub still: Vec<Still>,
    pub picture: Vec<u8>,
    pub image: Vec<u8>,
    pub sum: Vec<u8>,
    pub shared: Vec<u8>,
}

impl Scene {
    pub fn frame(&mut self, n: u32) {
        // SAFETY: open lists of ours, live objects; memory is written while
        // the card does not use it.
        unsafe {
            self.wait();
            if n == 0 {
                self.fill_still();
                self.fill(0, [200, 10, 30, 255]);
                self.fill(8192, [10, 40, 220, 255]);
                let l = self.open(0);
                self.texture_copy(&l, &self.fixed, 0);
                self.texture_copy(&l, &self.other, 8192);
                l.ResourceBarrier(&[moved(&self.fixed, TO, READ), moved(&self.other, TO, READ)]);
                l.Close().unwrap();
                self.send(&[&l]);
                self.wait();
            }
            // The first submission.
            self.show(4000, &self.fixed);
            self.place(0, [-1.0 + 0.1 * n as f32, -1.0, 0.5, 0.5]);
            self.place(256, [0.0, 0.0, 0.5, 0.5]);
            self.give(65536, 6);
            self.give(40, 7 + n);
            let l = self.open(0);
            l.ResourceBarrier(&[moved(&self.target, FROM, TARGET)]);
            let (rtv, dsv) = (
                self.rtvs.GetCPUDescriptorHandleForHeapStart(),
                self.dsvs.GetCPUDescriptorHandleForHeapStart(),
            );
            l.ClearRenderTargetView(rtv, &[0.1 * n as f32, 0.2, 0.3, 1.0], None);
            l.ClearDepthStencilView(dsv, D3D12_CLEAR_FLAG_DEPTH, 1.0, 0, None);
            self.arguments(&l, COMMON);
            self.targets(&l);
            self.quad(&l, 4000, 0, 0);
            self.quad(&l, 4001, 256, 0);
            self.triangle(&l, n);
            // Nothing: the clamp cut it to no vertices.
            self.quad(&l, 4001, 0, 1);
            l.ResourceBarrier(&[moved(&self.scratch, ARGUMENTS, COMMON)]);
            l.Close().unwrap();
            self.send(&[&l]);
            self.wait();
            // The second: other constants and another texture in the same
            // places, and the second texture in the memory of the first.
            self.show(4000, &self.other);
            // Arguments of a mesh draw.
            self.constants
                .1
                .add(1024)
                .cast::<[u32; 3]>()
                .write([1, 1, 1]);
            self.place(0, [-0.9, 0.3, 0.6, 0.6]);
            self.place(256, [0.3, -0.8, 0.6, 0.5]);
            self.give(40, 100 + n);
            self.give(65536, 3);
            self.fill(16384, [30, 200, 10 + 5 * n as u8, 255]);
            let l = self.open(1);
            self.targets(&l);
            self.arguments(&l, COMMON);
            self.quad(&l, 4000, 0, 0);
            l.ResourceBarrier(&[moved(&self.other, READ, TO)]);
            l.ResourceBarrier(&[aliased(&self.other, &self.one)]);
            self.texture_copy(&l, &self.one, 16384);
            l.ResourceBarrier(&[moved(&self.one, TO, READ)]);
            self.arguments(&l, ARGUMENTS);
            self.quad(&l, 4002, 256, 0);
            if let Some((made, signature)) = &self.mesh {
                l.SetGraphicsRootSignature(&made.root);
                l.SetPipelineState(&made.pso);
                let constants = [
                    (-0.3f32).to_bits(),
                    (-0.9f32).to_bits(),
                    0.5f32.to_bits(),
                    0.4f32.to_bits(),
                    4002,
                ];
                l.SetGraphicsRoot32BitConstants(0, 5, constants.as_ptr().cast(), 0);
                l.ExecuteIndirect(
                    signature,
                    1,
                    &self.constants.0,
                    1024,
                    None::<&ID3D12Resource>,
                    0,
                );
            }
            l.ResourceBarrier(&[moved(&self.scratch, ARGUMENTS, COMMON)]);
            // And back, for the next frame.
            l.ResourceBarrier(&[moved(&self.one, READ, TO)]);
            l.ResourceBarrier(&[aliased(&self.one, &self.other)]);
            self.texture_copy(&l, &self.other, 8192);
            l.ResourceBarrier(&[moved(&self.other, TO, READ)]);
            l.Close().unwrap();
            let second = l;
            let l = self.open(2);
            l.SetDescriptorHeaps(&[Some(self.views.clone())]);
            l.SetComputeRootSignature(&self.add.root);
            l.SetPipelineState(&self.add.pso);
            l.SetComputeRootShaderResourceView(0, self.shared.0.GetGPUVirtualAddress());
            let table = self.views.GetGPUDescriptorHandleForHeapStart().ptr + 7 * self.step as u64;
            l.SetComputeRootDescriptorTable(1, D3D12_GPU_DESCRIPTOR_HANDLE { ptr: table });
            l.SetComputeRootUnorderedAccessView(2, self.sum.GetGPUVirtualAddress());
            for _ in 0..3 {
                l.Dispatch(2, 2, 1);
                l.ResourceBarrier(&[uav_barrier()]);
            }
            // The card writes memory the CPU sees.
            l.ResourceBarrier(&[moved(&self.sum, UAV, FROM)]);
            l.CopyBufferRegion(&self.shared.0, 30_000, &self.sum, 0, 1024);
            l.ResourceBarrier(&[moved(&self.target, TARGET, FROM)]);
            l.Close().unwrap();
            self.send(&[&second, &l]);
            self.wait();
            // Another queue fetches a result, as the host layer's relay does.
            let (queue, allocator, list) = &self.copies;
            allocator.Reset().unwrap();
            list.Reset(allocator, None).unwrap();
            list.CopyBufferRegion(&self.back.0, 1 << 18, &self.sum, 0, 1024);
            list.Close().unwrap();
            queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
            self.value += 1;
            queue.Signal(&self.fence, self.value).unwrap();
            self.fence
                .SetEventOnCompletion(self.value, self.event)
                .unwrap();
            WaitForSingleObject(self.event, INFINITE);
            if let Some((swap, _)) = &self.swap {
                swap.Present(0, DXGI_PRESENT(0)).unwrap();
            }
            record::present(&self.target, FROM);
        }
    }

    /// What everything holds now.
    pub fn outcome(&mut self) -> Outcome {
        self.wait();
        let still = self.read_still();
        let l = self.open(0);
        // SAFETY: an open list; live objects.
        unsafe {
            let to = |at: u64, size| D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&self.back.0),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: footprint(at, size),
                },
            };
            let of = |t: &ID3D12Resource| D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(t),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            };
            l.ResourceBarrier(&[moved(&self.image, UAV, FROM)]);
            l.CopyTextureRegion(&to(0, SIZE), 0, 0, 0, &of(&self.target), None);
            l.CopyTextureRegion(&to(1 << 18, 16), 0, 0, 0, &of(&self.image), None);
            l.ResourceBarrier(&[moved(&self.image, FROM, UAV)]);
            l.CopyBufferRegion(&self.back.0, 1 << 19, &self.sum, 0, 1024);
            l.Close().unwrap();
            self.send(&[&l]);
            self.wait();
            let at = |from: usize, n: usize| {
                std::slice::from_raw_parts(self.back.1.add(from), n).to_vec()
            };
            Outcome {
                still,
                picture: at(0, (SIZE * 512) as usize),
                image: rows(&at(1 << 18, 4096)),
                sum: at(1 << 19, 1024),
                shared: std::slice::from_raw_parts(self.shared.1, 70_000).to_vec(),
            }
        }
    }
}
