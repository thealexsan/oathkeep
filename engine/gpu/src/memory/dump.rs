//! `OATHKEEP_MEMORY_DUMP=<folder>`: when a file `go` appears there, the arenas
//! as the card holds them are copied out and written (`arena-<n>.bin`), with
//! `pages.txt` (title page, arena, offset) and the structures the walking
//! programs were given: to look at the structures offline.

use windows::Win32::Graphics::Direct3D12::*;

use super::state::Memory;
use super::{ARENA, PAGE};
use crate::host::{HostList, host};

#[derive(Default)]
pub(super) struct Dump {
    walked: std::collections::BTreeSet<u64>,
    /// Views the builder was given to write outside buffers: the first
    /// and the last frame, how often.
    pub(super) built: std::collections::BTreeMap<u64, (u64, u64, u64)>,
    /// The frame it is written at, and the copies.
    due: Option<(u64, Vec<(ID3D12Resource, usize)>)>,
    done: bool,
}

impl Dump {
    pub(super) fn walked(&mut self, addresses: &[u64]) {
        if !self.done && self.walked.len() < 64 {
            self.walked.extend(addresses);
        }
    }
}

fn folder() -> Option<&'static std::path::PathBuf> {
    static FOLDER: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    FOLDER.get_or_init(|| std::env::var_os("OATHKEEP_MEMORY_DUMP").map(Into::into)).as_ref()
}

fn turn(buffer: &ID3D12Resource, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: the same pointer, not counted; the barrier is
                // used while the buffer lives.
                pResource: unsafe { std::mem::transmute_copy(buffer) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

impl Memory {
    /// At a list's first use: the copies are recorded into it.
    pub(super) fn dump(&mut self, list: &HostList, _: &[u64]) {
        let Some(folder) = folder() else { return };
        if let Some((due, _)) = &self.dump.due {
            if self.frame < *due {
                return;
            }
            let copies = self.dump.due.take().map(|d| d.1).unwrap_or_default();
            let mut text = String::new();
            for a in &self.dump.walked {
                text += &format!("walked {a:#x}\n");
            }
            for (a, (first, last, n)) in &self.dump.built {
                text += &format!("built {a:#x} {first} {last} {n}\n");
            }
            for (a, p) in &self.packed {
                text += &format!("packed {a:#x} {} {}\n", p.len, p.frame);
            }
            text += &format!("frame {}\n", self.frame);
            for (page, held) in &self.pages {
                let arena = self.arenas.iter().position(|a| a.0 == held.copy).unwrap_or(usize::MAX);
                let at = held.at - self.arenas[arena].1;
                text += &format!("{:#x} {arena} {at:#x}\n", page * PAGE);
            }
            let _ = std::fs::write(folder.join("pages.txt"), text);
            for (n, (_, data)) in copies.iter().enumerate() {
                // SAFETY: a mapping of an arena's bytes; the copies ran
                // frames ago.
                let bytes = unsafe { std::slice::from_raw_parts(*data as *const u8, ARENA as usize) };
                let _ = std::fs::write(folder.join(format!("arena-{n}.bin")), bytes);
            }
            tracing::info!(arenas = copies.len(), "title memory by address: the arenas are written out");
            return;
        }
        if self.dump.done || !folder.join("go").exists() {
            return;
        }
        self.dump.done = true;
        let Some(host) = host() else { return };
        let mut copies = Vec::new();
        for &(n, _) in &self.arenas {
            let buffer = &self.copies[n].buffer;
            let Some((to, data)) = host.shared_buffer(ARENA) else { break };
            // SAFETY: an open list; live buffers of those sizes. The list
            // has not used the arena yet.
            unsafe {
                list.list.ResourceBarrier(&[turn(buffer, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                list.list.CopyBufferRegion(&to, 0, buffer, 0, ARENA);
                list.list.ResourceBarrier(&[turn(buffer, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON)]);
            }
            list.keep(&to);
            copies.push((to, data as usize));
        }
        self.dump.due = Some((self.frame + 12, copies));
    }
}
