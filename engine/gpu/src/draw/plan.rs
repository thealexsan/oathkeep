//! Shader descriptor table plans, inline collections, and record hashing.

use oathkeep_shader::resource::RootIndexedView;
use oathkeep_shader::{ResourceType, RootType, UsedResource};

/// Shader interface metadata shared by every pipeline variant and draw.
pub(crate) struct TablePlan {
    pub(super) groups: Vec<TableGroup>,
    // Direct entries can interleave slots. Preserve their declaration order
    // when recording resource reads, writes, and transitions.
    pub(super) direct: Vec<(usize, usize)>,
}

pub(super) struct TableGroup {
    pub(super) slot: u8,
    pub(super) has_cbv: bool,
    pub(super) has_uav: bool,
    pub(super) max_dword: u32,
    pub(super) entries: Vec<UsedResource>,
    pub(super) indexed: Vec<RootIndexedView>,
    // Descriptor number -> first declaration, matching the bind-time search.
    by_descriptor: Vec<(u32, usize)>,
}

impl TablePlan {
    pub(crate) fn new(used: &[UsedResource], indexed: &[RootIndexedView]) -> Self {
        let mut slots: Vec<_> = used
            .iter()
            .filter(|u| u.root == RootType::Table)
            .map(|u| u.slot)
            .chain(indexed.iter().map(|v| v.table_slot))
            .collect();
        slots.sort_unstable();
        slots.dedup();
        let mut groups: Vec<_> = slots
            .iter()
            .map(|&slot| TableGroup {
                slot,
                has_cbv: false,
                has_uav: false,
                max_dword: 0,
                entries: Vec::new(),
                by_descriptor: Vec::new(),
                indexed: indexed.iter().filter(|v| v.table_slot == slot).copied().collect(),
            })
            .collect();
        let mut direct = Vec::new();
        for &u in used.iter().filter(|u| u.root == RootType::Table) {
            let i = slots
                .binary_search(&u.slot)
                .expect("table slot collected from these declarations");
            let group = &mut groups[i];
            group.has_cbv |= u.kind == ResourceType::Cbv;
            group.has_uav |= u.kind == ResourceType::Uav;
            group.max_dword = group.max_dword.max(u.table_offset_dwords);
            direct.push((i, group.entries.len()));
            group
                .by_descriptor
                .push((u.table_offset_dwords / 8, group.entries.len()));
            group.entries.push(u);
        }
        for group in &mut groups {
            group.by_descriptor.sort_unstable();
            group.by_descriptor.dedup_by_key(|item| item.0);
        }
        Self { groups, direct }
    }
}

impl TableGroup {
    pub(super) fn entry(&self, descriptor: u32) -> Option<&UsedResource> {
        let i = self
            .by_descriptor
            .binary_search_by_key(&descriptor, |item| item.0)
            .ok()?;
        Some(&self.entries[self.by_descriptor[i].1])
    }

    pub(super) fn selected_descriptors(
        &self,
        mut read_constant: impl FnMut(u8, u32) -> u32,
    ) -> Vec<u32> {
        if self.indexed.is_empty() {
            return Vec::new();
        }
        let mut selected: Vec<u32> = self
            .indexed
            .iter()
            .map(|index| {
                let constant = read_constant(index.constant_slot, index.constant_dword);
                // The shader adds i32 byte offsets and shifts by 5.
                index.byte_offset.wrapping_add(constant << index.shift) / 32
            })
            .collect();
        selected.sort_unstable();
        selected.dedup();
        selected
    }

    pub(super) fn needs_snapshot(
        &self,
        heap: &crate::descriptor::Heap,
        host_count: u32,
        table: u64,
        selected: &[u32],
    ) -> bool {
        self.needs_snapshot_by(host_count, table, selected, |end| heap.index(end))
    }

    pub(super) fn needs_snapshot_by(
        &self,
        host_count: u32,
        table: u64,
        selected: &[u32],
        index_of: impl Fn(u64) -> Option<u32>,
    ) -> bool {
        let index_at = |dword: u32| table.checked_add(u64::from(dword) * 4).and_then(&index_of);
        let max_dword = self
            .max_dword
            .max(selected.last().copied().unwrap_or(0).saturating_mul(8));
        match index_at(max_dword) {
            Some(i) => i >= host_count,
            None => {
                self.entries
                    .iter()
                    .any(|u| index_at(u.table_offset_dwords).is_some_and(|i| i >= host_count))
                    || selected
                        .iter()
                        .any(|&k| index_at(k.saturating_mul(8)).is_some_and(|i| i >= host_count))
            }
        }
    }
}

/// A few things, up to `N` of them without memory of the heap: what a draw's
/// tables are gone through with, some six lists a stage.
pub(crate) struct Few<T, const N: usize> {
    few: [T; N],
    len: usize,
    /// All of them, if they are more.
    many: Vec<T>,
}

impl<T: Copy + Default, const N: usize> Default for Few<T, N> {
    fn default() -> Self {
        Few { few: [T::default(); N], len: 0, many: Vec::new() }
    }
}

impl<T: Copy, const N: usize> Few<T, N> {
    pub(super) fn push(&mut self, what: T) {
        if self.many.is_empty() && self.len < N {
            self.few[self.len] = what;
            self.len += 1;
        } else {
            if self.many.is_empty() {
                self.many.extend_from_slice(&self.few);
            }
            self.many.push(what);
        }
    }
}

impl<T, const N: usize> std::ops::Deref for Few<T, N> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        if self.many.is_empty() { &self.few[..self.len] } else { &self.many }
    }
}

impl<T, const N: usize> std::ops::DerefMut for Few<T, N> {
    fn deref_mut(&mut self) -> &mut [T] {
        if self.many.is_empty() { &mut self.few[..self.len] } else { &mut self.many }
    }
}

/// A table made afresh in dynamic slots, in a list ([`super::State::tables`]).
pub(super) struct FreshTable {
    pub(super) first: u32,
    /// The list's epoch ([`crate::host::HostList::epoch`]) at which all its entries that
    /// these readers (a stage's list of what it reads, whether it stores, the
    /// tested depth target) read were brought up to date, and whether that read the
    /// tested depth target.
    pub(super) epoch: u64,
    pub(super) ready: Vec<((usize, usize, bool, usize), bool)>,
}

/// Whether a table a list reads again, with nothing changed in the list
/// since, is left as it is (`OATHKEEP_READY_ONCE=0`: every entry is gone
/// through again at every draw).
pub(super) fn ready_once() -> bool {
    crate::toggled!("OATHKEEP_READY_ONCE")
}

/// A hash of records' words: 64 bits, two multiplications a record. (The
/// standard library's took 3 % of the recording threads' time.)
#[derive(Default)]
pub(super) struct RecordHash(pub(super) u64);

impl RecordHash {
    pub(super) fn add(&mut self, [a, b, c]: [u32; 3]) {
        let fold = |x: u64, y: u64| {
            let p = u128::from(x) * u128::from(y);
            p as u64 ^ (p >> 64) as u64
        };
        let words = u64::from(a) << 32 | u64::from(b);
        let first = fold(self.0 ^ words ^ 0x2d35_8dcc_aa6c_78a5, 0x8bb8_4b93_962e_acc9 ^ u64::from(c));
        self.0 = fold(first ^ 0x4b33_a62e_d433_d4a3, 0x4d5a_2da5_1de1_aa47 ^ self.0.rotate_left(23)) ^ first;
    }
}
