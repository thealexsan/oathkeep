//! Card-memory accounting snapshot for due bindless texture slots (`OATHKEEP_CARD_ACCOUNTING=1`).

use std::sync::atomic::{AtomicU64, Ordering};

use crate::descriptors::View;
use crate::d3d12::object::This;

use super::super::state::State;

impl State {
    /// The queue's side of the card accounting line
    /// (`crate::d3d12::resource::card_accounting`, which says what the line
    /// holds): the slots that are due, by what their texture has on the
    /// card. Off unless `OATHKEEP_CARD_ACCOUNTING=1`; then at the first pass
    /// that finds the card without room, and every five seconds after it.
    /// Nothing is made, uploaded or counted as used here. For 18,000 due
    /// slots: 18,000 looks at a title slot and at the texture it views, a
    /// lock each, some milliseconds of the thread that records the frame.
    pub(super) fn card_accounting(
        heap: &crate::descriptors::Heap,
        host: &crate::descriptors::host_heap::HostHeap,
        frame: u64,
        room: bool,
    ) {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| {
            std::env::var_os("OATHKEEP_CARD_ACCOUNTING").is_some_and(|v| v == "1")
        }) {
            return;
        }
        // When the next line is due (0: none yet, the card has had room).
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let now = crate::backend::now_ms().max(1);
        let next = NEXT.load(Ordering::Relaxed);
        // (With levels on demand the card may never be without room: the
        // lines then start with the first texture made that way.)
        let by_level = crate::memory::tiles::STATS[4].load(Ordering::Relaxed) > 0;
        let due_now = if next == 0 { !room || by_level } else { now >= next };
        if !due_now
            || NEXT
                .compare_exchange(next, now + 5000, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        // Due slots; of them uploaded before; of them viewing nothing the
        // title fills; and the textures (each once) whose host copy is
        // alive, with their bytes on the card.
        let mut due = [0u64; 5];
        // Textures (each once) that never had a host copy, and ones whose
        // host copy was let go: where their memory is, and its bytes.
        let (mut never, mut gone) = (Vec::new(), Vec::new());
        let mut seen = std::collections::BTreeSet::new();
        for (index, uploaded) in host.texture_slots_all_due(frame) {
            due[0] += 1;
            due[1] += u64::from(uploaded);
            let state = match heap.handle(index).and_then(crate::descriptors::view) {
                Some(View::Placed(false, resource, desc)) if resource[0] != 1 => {
                    let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                    match crate::d3d12::resource::placed_known(address, &resource) {
                        Some(texture) => crate::d3d12::resource::copy_state(texture),
                        // No title texture was made for it: the pass never
                        // got as far as making one.
                        None => Some((
                            0,
                            crate::d3d12::resource::placed_bytes(&resource).unwrap_or(0),
                            address,
                        )),
                    }
                }
                Some(View::Shader(object, _)) if crate::d3d12::resource::title_fills(object as This) => {
                    crate::d3d12::resource::copy_state(object as This)
                }
                _ => None,
            };
            let Some((state, bytes, memory)) = state else {
                due[2] += 1;
                continue;
            };
            if !seen.insert(memory) {
                continue;
            }
            match state {
                2 => {
                    due[3] += 1;
                    due[4] += bytes;
                }
                1 => gone.push((memory, bytes)),
                _ => never.push((memory, bytes)),
            }
        }
        crate::d3d12::resource::card_accounting(room, due, never, gone);
    }
}
