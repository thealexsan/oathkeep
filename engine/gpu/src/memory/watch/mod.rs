//! Which title memory the title wrote since a host copy took it (decision
//! 0007, "Copies of title memory"). The runtime reserves title GPU memory
//! with write watching (`oathkeep-hle` xmem), so the kernel keeps a dirty bit
//! per 4 KiB page; asking for a range's written pages also clears them. Host
//! copies of the same memory can overlap (nested buffers, textures in
//! buffers), so what one asks is kept here for the others: a journal of
//! each page's last write, numbered by the asking.
//!
//! Physical-page views (xmem's sections) cannot be write-watched. There a
//! copy leaves each 64 KiB block it took read-only, and our vectored handler
//! takes the first write to it: it notes the block as written and gives the
//! write access back. The runtime tells us when it maps or unmaps views
//! ([`title_memory_remapped`]), which resets their protection. Blocks found
//! wholly uncommitted are skipped while they remain so; one query can
//! confirm a whole region, including ordinary reservations between views.
//!
//! The title shows one physical page at several addresses, and writes it
//! through one while the card reads it at another. So a block is clean, and
//! known by its bit, only while every view of its page is read-only
//! ([`protect::cover`]): a write through any of them faults, opens them all and takes
//! clean and bit from all of them ([`protect::open`]). A view mapped anew of a page
//! that is guarded elsewhere has the others looked at again
//! ([`views_mapped`]). What we write ourselves is told at one address and
//! counts at all ([`writing`], [`landed_beside`], [`wrote`]). Not so for a
//! page shown in more than [`MANY_VIEWS`] places (what the title shows where
//! it has nothing). `OATHKEEP_VIEW_GUARD=0`: each address for itself, as
//! before.

mod ask;
mod bits;
mod early;
mod fault;
mod guards;
mod journal;
mod mapping;
mod protect;
mod write;

pub(crate) use ask::{
    ask_ahead, ask_ahead_done, ask_once, ask_once_end, asked, has_aliases, nothing_new, once, writes, wrote,
};
#[cfg(feature = "diagnostics")]
pub(crate) use early::{early_asks, slow_asks};
pub(crate) use fault::install;
#[cfg(feature = "diagnostics")]
pub(crate) use guards::asks;
pub(crate) use guards::{
    Beside, guarded, guards_any, light, look_at_holes, nothing_put, opened_for_writing, put_beside,
};
pub use guards::title_memory_allocated;
#[cfg(feature = "diagnostics")]
pub(crate) use journal::lock_waits;
pub(crate) use journal::{Tracked, can_answer_without_asking, once_kept, parts_written, track};
pub(crate) use mapping::{again_after_change, change_over, changes, lost};
#[allow(unused_imports)]
pub use mapping::{
    Reading, Remapping, title_memory_freed, title_memory_reading, title_memory_reading_now, title_memory_remapped,
    title_memory_remapping, title_memory_unmapping,
};
#[cfg(feature = "diagnostics")]
pub(crate) use protect::ALIASES;
pub(crate) use protect::MANY_VIEWS;
pub(crate) use write::{
    WRITTEN, block_published, landed_beside, landed_lately, landing, page_writes, views_mapped,
    written_since_taken,
};
#[allow(unused_imports)]
pub use write::{KernelWrite, kernel_writing, title_memory_splitting, title_memory_written, writing};

/// The unit tracked and copied: 64 KiB, the console's page (the kernel's
/// 4 KiB pages fold into it). Finer units made the checks of the world's
/// big buffers cost more than the copies they saved.
pub(crate) const PAGE: usize = 64 << 10;
/// The kernel's page, the unit of its answers.
const KERNEL_PAGE: usize = 4096;
/// The parts of a page.
const PARTS: usize = PAGE / KERNEL_PAGE;
/// All of them.
const WHOLE: u16 = u16::MAX;
/// Guarded blocks of view memory.
const BLOCK: usize = PAGE;

#[cfg(test)]
mod tests;
