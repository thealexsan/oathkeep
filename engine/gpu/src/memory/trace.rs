//! `OATHKEEP_BVH_TRACE=1`: the sizes the builder is asked for (its program of
//! one thread that stores a structure's compacted size) and when title memory
//! has them, and the copies to a place of that size.

use std::collections::HashMap;

use super::BACK;
use crate::host::HostList;

#[derive(Default)]
pub(super) struct Trace {
    /// Frame, where the size goes, the structure, what title memory had.
    asked: Vec<(u64, u64, u64, u64)>,
    /// By structure, what its size was when last seen to change.
    sizes: HashMap<u64, (u64, u64)>,
    said: u32,
}

fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_BVH_TRACE").is_some_and(|v| v == "1"))
}

fn value(at: u64) -> u64 {
    let mut b = [0u8; 8];
    crate::host::copy_mapped(b.as_mut_ptr(), at as *const u8, 8, false);
    u64::from_le_bytes(b)
}

impl Trace {
    fn say(&mut self, text: String) {
        self.said += 1;
        if self.said < 6000 {
            tracing::info!("bvh trace: {text}");
        }
    }

    pub(super) fn look(&mut self, frame: u64) {
        if !on() {
            return;
        }
        let mut gone = Vec::new();
        for i in 0..self.asked.len() {
            let (then, at, structure, was) = self.asked[i];
            let now = value(at);
            if now != was {
                self.sizes.insert(structure, (now, frame));
                gone.push(i);
                self.say(format!("frame {frame}: size of {structure:#x} at {at:#x} asked in {then}: {was:#x} -> {now:#x}"));
            } else if frame > then + 60 {
                gone.push(i);
                self.say(format!("frame {frame}: size of {structure:#x} at {at:#x} asked in {then}: still {was:#x}"));
            }
        }
        for i in gone.into_iter().rev() {
            self.asked.swap_remove(i);
        }
    }

    pub(super) fn dispatch(
        &mut self,
        frame: u64,
        list: &HostList,
        addresses: &[u64],
        written: &[u64],
        constants: &[u32],
        threads: Option<[u32; 3]>,
    ) {
        if !on() {
            return;
        }
        let _ = addresses;
        let list = std::ptr::from_ref(list) as usize & 0xFFFF_FFFF;
        if threads == Some([1, 1, 1])
            && let [lo, hi, mode, ..] = *constants
            && (1..=3).contains(&mode)
            && hi < 0x20
            && let Some(&at) = written.first()
            && crate::shadow::in_buffer(at, 1).is_some_and(|b| b.1 <= BACK)
        {
            let structure = u64::from(lo) | u64::from(hi) << 32;
            let was = value(at);
            self.say(format!("frame {frame} list {list:x}: asks mode {mode} of {structure:#x} to {at:#x}, there {was:#x}"));
            if mode == 1 {
                self.asked.push((frame, at, structure, was));
            }
        } else if threads == Some([32, 1, 1])
            && let [from, to] = *written
            && [from, to].iter().all(|&a| a >= 1 << 32 && crate::shadow::in_buffer(a, 1).is_none())
        {
            let known = self.sizes.get(&from).copied();
            self.say(format!(
                "frame {frame} list {list:x}: copies {from:#x} to {to:#x} constants {:x?}, its size was {known:x?}",
                &constants[..constants.len().min(4)]
            ));
        }
    }
}
