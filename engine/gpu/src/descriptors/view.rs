//! View descriptions, inline record encoding, and the bounded view table.

#[cfg(windows)]
use super::create;
#[cfg(windows)]
use crate::d3d12::object::This;

/// A shader view or sampler as the title described it (descriptions in the
/// host's layout, kept as words), for the host to build its own from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum View {
    /// Constant buffer: GPU address and size.
    Constant(u64, u32),
    /// Shader resource: resource, description (none: the resource's own).
    Shader(usize, Option<[u32; 10]>),
    /// Unordered access: resource, counter resource, description.
    Unordered(usize, usize, Option<[u32; 10]>),
    Sampler([u32; 13]),
    /// The console's own sampler description: flags, filters per axis,
    /// addressing, LOD and anisotropy settings, border color, then
    /// console-only tuning words.
    ConsoleSampler([u32; 27]),
    /// A view of memory without a resource object: whether it is unordered
    /// access, the resource description, then the view's (the plain one
    /// followed by the memory address and console fields).
    Placed(bool, [u32; 14], [u32; 16]),
}

impl View {
    /// Whether the view reads buffer memory (constant buffers, buffer views
    /// of resources or of memory) or memory the title placed a texture in:
    /// such views get host slots only for draws.
    #[cfg(windows)]
    pub(crate) fn is_buffer(&self) -> bool {
        match self {
            View::Constant(..) | View::Placed(..) => true,
            View::Shader(r, _) | View::Unordered(r, _, _) => {
                crate::d3d12::resource::buffer_bytes(*r as This).is_some()
            }
            View::Sampler(_) | View::ConsoleSampler(_) => false,
        }
    }

    /// The resource objects the view names.
    pub(super) fn resources(&self) -> [usize; 2] {
        match *self {
            View::Shader(r, _) => [r, 0],
            View::Unordered(r, counter, _) => [r, counter],
            _ => [0, 0],
        }
    }

    /// The texture object the view is of, if that texture has no host copy
    /// yet (`crate::d3d12::resource::host_waits`): a slot written with such a
    /// view is left null until the texture is first used.
    #[cfg(windows)]
    pub(crate) fn waiting_texture(&self) -> Option<usize> {
        match *self {
            View::Shader(r, _) | View::Unordered(r, _, _)
                if crate::d3d12::resource::host_waits(r as This) =>
            {
                Some(r)
            }
            _ => None,
        }
    }
}

/// Every distinct view in use; a descriptor holds a tag, an index into it
/// and the entry's generation, which survive titles copying descriptors as
/// bytes. A view of a resource lives as long as the resource: when the
/// title releases it, its entries are freed for reuse under a new
/// generation, so descriptors (and host slots made from them) naming the
/// old one go stale instead of naming whatever is made at the same address
/// next.
///
/// Views without a resource object have nothing that releases them, and a
/// title copies descriptors as bytes, so nothing says when the last
/// descriptor naming an entry is gone: such entries were kept for the run.
/// The title writes some 14,000 constant buffer views a frame,
/// each at the address its constants have that frame: 2,450,139 entries
/// after four minutes of play, 1,047 MB with the index, and 11,000 more
/// every second (run 65). So the two kinds that carry an address are not
/// entries any more: their records name them whole ([`constant_words`],
/// [`placed_words`]), and the table holds only what is bounded: views of
/// resource objects (freed with the resource), samplers (as many as there
/// are sampler states), and the shapes of placed views (their descriptions
/// without the address: as many as the title has kinds of textures and
/// buffers, however often it moves them).
/// `OATHKEEP_VIEW_INLINE=0`: every view is an entry, as before.
pub(super) struct Views {
    /// Each entry's view and generation.
    pub(super) list: Vec<(View, u32)>,
    index: std::collections::BTreeMap<View, u32>,
    /// Entries naming each resource.
    by_resource: std::collections::BTreeMap<usize, Vec<u32>>,
    free: Vec<u32>,
    /// The shapes of views of placed memory, and where each is.
    pub(super) shapes: Vec<Shape>,
    shape_index: std::collections::BTreeMap<Shape, u32>,
}

/// Read far more often than written: every slot a draw reads through a
/// table is looked up here, on several recording threads at once in the
/// world. Readers share it; only a view not seen before (a new table entry
/// or shape) and a released resource take it alone.
pub(super) static VIEWS: std::sync::RwLock<Views> = std::sync::RwLock::new(Views {
    list: Vec::new(),
    index: std::collections::BTreeMap::new(),
    by_resource: std::collections::BTreeMap::new(),
    free: Vec::new(),
    shapes: Vec::new(),
    shape_index: std::collections::BTreeMap::new(),
});

/// Incremented whenever [`forget`] invalidates a table entry's generation.
static VIEWS_FORGOT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(in crate::descriptors) const VIEW_TAG: u32 = u32::from_le_bytes(*b"vVIE");

/// A record is the tag and two words, `i` and `generation` (the host keeps
/// them as one 64-bit key per slot). The top two bits of `generation` say
/// what the other 62 bits are: 0, entry `i` of the table at that
/// generation; [`CONSTANT`], a constant buffer view, whole; [`PLACED`], a
/// view of placed memory: its address and the number of its shape.
pub(super) const KIND_SHIFT: u32 = 30;
/// Table generations stay below the kind bits (and above 0).
const GENERATIONS: u32 = (1 << KIND_SHIFT) - 1;
pub(super) const CONSTANT: u32 = 1;
pub(super) const PLACED: u32 = 2;
/// Bits of a constant buffer view's address in its record: any address a
/// program can have. The 15 bits above them are its size.
pub(super) const CONSTANT_ADDRESS_BITS: u32 = 47;
/// Bits of a placed view's address in its record: placed memory is title
/// memory, which lies below 1 TiB. The 22 bits above them number its shape.
const PLACED_ADDRESS_BITS: u32 = 40;
const SHAPES: usize = 1 << (62 - PLACED_ADDRESS_BITS);

/// A view of placed memory without its address: whether it is unordered
/// access, the resource description, and the view's words but the two that
/// hold the address (10 and 11).
type Shape = (bool, [u32; 14], [u32; 14]);

/// Whether constant buffer views and views of placed memory are named whole
/// in their records ([`Views`]). On by default; `OATHKEEP_VIEW_INLINE=0`
/// makes every view a table entry, as before.
pub(super) fn inline() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("OATHKEEP_VIEW_INLINE").is_none_or(|v| v != "0"))
}

/// Views named whole in their records since the start: constant buffer
/// views, views of placed memory (for the memory line).
static NAMED_WHOLE: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// The two record words of the 62 bits `payload` of kind `kind`.
fn kind_words(kind: u32, payload: u64) -> [u32; 2] {
    [payload as u32, kind << KIND_SHIFT | (payload >> 32) as u32]
}

/// The 62 bits two record words carry beside their kind.
fn payload(i: u32, generation: u32) -> u64 {
    u64::from(generation & GENERATIONS) << 32 | u64::from(i)
}

/// The record words that name the constant buffer view of `size` bytes at
/// `address` whole; `None` if they cannot: an address past 47 bits, or a
/// size of 16 KiB or more that is no multiple of 256 or reaches 4 MiB (the
/// size has 15 bits: bytes below 16 KiB, else units of 256 bytes). Such a
/// view is a table entry.
pub(super) fn constant_words(address: u64, size: u32) -> Option<[u32; 2]> {
    if address >> CONSTANT_ADDRESS_BITS != 0 {
        return None;
    }
    let code = if size < 1 << 14 {
        u64::from(size)
    } else if size.is_multiple_of(256) && size >> 8 < 1 << 14 {
        1 << 14 | u64::from(size >> 8)
    } else {
        return None;
    };
    Some(kind_words(CONSTANT, code << CONSTANT_ADDRESS_BITS | address))
}

/// The constant buffer view [`constant_words`] named.
fn constant_of(i: u32, generation: u32) -> View {
    let payload = payload(i, generation);
    let code = (payload >> CONSTANT_ADDRESS_BITS) as u32;
    let size = if code >> 14 == 0 { code } else { (code & 0x3fff) << 8 };
    View::Constant(payload & ((1 << CONSTANT_ADDRESS_BITS) - 1), size)
}

/// A placed view's shape and its address.
fn shape_of(unordered: bool, resource: &[u32; 14], desc: &[u32; 16]) -> (Shape, u64) {
    let mut rest = [0u32; 14];
    rest[..10].copy_from_slice(&desc[..10]);
    rest[10..].copy_from_slice(&desc[12..]);
    (
        (unordered, *resource, rest),
        u64::from(desc[10]) | u64::from(desc[11]) << 32,
    )
}

/// The record words that name the view of placed memory at `address` with
/// shape number `shape`.
fn placed_words(shape: u32, address: u64) -> [u32; 2] {
    kind_words(PLACED, u64::from(shape) << PLACED_ADDRESS_BITS | address)
}

impl Views {
    /// The number of `shape`, a new one if it is not there yet; `None` when
    /// the shapes are full (the view is then a table entry).
    fn shape(&mut self, shape: Shape) -> Option<u32> {
        if let Some(&at) = self.shape_index.get(&shape) {
            return Some(at);
        }
        if self.shapes.len() >= SHAPES {
            return None;
        }
        let at = self.shapes.len() as u32;
        self.shapes.push(shape);
        self.shape_index.insert(shape, at);
        Some(at)
    }

    /// The view of placed memory [`placed_words`] named.
    fn placed_of(&self, i: u32, generation: u32) -> Option<View> {
        let payload = payload(i, generation);
        let address = payload & ((1 << PLACED_ADDRESS_BITS) - 1);
        let &(unordered, resource, rest) = self.shapes.get((payload >> PLACED_ADDRESS_BITS) as usize)?;
        let mut desc = [0u32; 16];
        desc[..10].copy_from_slice(&rest[..10]);
        (desc[10], desc[11]) = (address as u32, (address >> 32) as u32);
        desc[12..].copy_from_slice(&rest[10..]);
        Some(View::Placed(unordered, resource, desc))
    }

    /// The record words of `view` as a table entry, if it has one.
    fn entry_of(&self, view: &View) -> Option<[u32; 2]> {
        let &i = self.index.get(view)?;
        Some([i, self.list[i as usize].1])
    }

    /// The record words of `view` as a table entry: its entry (a new or a
    /// freed one if the view is not there yet) and that entry's generation.
    fn entry(&mut self, view: View) -> [u32; 2] {
        let i = match self.index.get(&view) {
            Some(&i) => i,
            None => {
                let i = match self.free.pop() {
                    Some(i) => {
                        self.list[i as usize].0 = view.clone();
                        i
                    }
                    None => {
                        self.list.push((view.clone(), 1));
                        self.list.len() as u32 - 1
                    }
                };
                for r in view.resources().into_iter().filter(|&r| r != 0) {
                    self.by_resource.entry(r).or_default().push(i);
                }
                self.index.insert(view, i);
                i
            }
        };
        [i, self.list[i as usize].1]
    }
}

/// The two words that name `view` in a record: the view itself where its
/// kind allows ([`Views`]), else its table entry.
pub(super) fn words_for(view: View) -> [u32; 2] {
    use std::sync::atomic::Ordering::Relaxed;
    if inline() {
        match &view {
            View::Constant(address, size) => {
                if let Some(words) = constant_words(*address, *size) {
                    NAMED_WHOLE[0].fetch_add(1, Relaxed);
                    return words;
                }
            }
            View::Placed(unordered, resource, desc) => {
                let (shape, address) = shape_of(*unordered, resource, desc);
                if address >> PLACED_ADDRESS_BITS == 0 {
                    // Nearly every placed view has a shape seen before.
                    let known = views_read().shape_index.get(&shape).copied();
                    let shape = known.or_else(|| views_write().shape(shape));
                    if let Some(shape) = shape {
                        NAMED_WHOLE[1].fetch_add(1, Relaxed);
                        return placed_words(shape, address);
                    }
                }
            }
            _ => {}
        }
    }
    // A view written before keeps its entry and generation.
    if let Some(words) = views_read().entry_of(&view) {
        return words;
    }
    views_write().entry(view)
}

fn views_read() -> std::sync::RwLockReadGuard<'static, Views> {
    VIEWS.read().unwrap_or_else(|e| e.into_inner())
}

fn views_write() -> std::sync::RwLockWriteGuard<'static, Views> {
    VIEWS.write().unwrap_or_else(|e| e.into_inner())
}

/// For the memory line: the distinct views kept ([`Views`]), those of them
/// free for reuse, and the bytes the table and its index hold for them (an
/// estimate: an entry of the table, and one of the index with its share of
/// a tree node; the shapes likewise); then the entries that are constant
/// buffer views and views of placed memory (those that did not fit a
/// record, or all of them with `OATHKEEP_VIEW_INLINE=0`), the shapes of
/// placed views kept, and the views named whole in their records since the
/// start: constant buffer views, views of placed memory.
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn views_kept() -> [u64; 8] {
    use std::sync::atomic::Ordering::Relaxed;
    let views = views_read();
    let each = size_of::<(View, u32)>() as u64;
    let shape = size_of::<Shape>() as u64;
    let (mut constants, mut placed) = (0, 0);
    for (view, _) in &views.list {
        match view {
            View::Constant(..) => constants += 1,
            View::Placed(..) => placed += 1,
            _ => {}
        }
    }
    [
        views.list.len() as u64,
        views.free.len() as u64,
        views.list.capacity() as u64 * each
            + views.index.len() as u64 * each * 3 / 2
            + views.shapes.capacity() as u64 * shape
            + views.shape_index.len() as u64 * shape * 3 / 2,
        constants,
        placed,
        views.shapes.len() as u64,
        NAMED_WHOLE[0].load(Relaxed),
        NAMED_WHOLE[1].load(Relaxed),
    ]
}

/// The title released `resource`: the views naming it go stale.
pub(crate) fn forget(resource: usize) {
    #[cfg(windows)]
    create::forget_waiting(resource);
    let mut views = views_write();
    let Some(entries) = views.by_resource.remove(&resource) else {
        return;
    };
    let mut forgot_any = false;
    for i in entries {
        let (view, generation) = views.list[i as usize].clone();
        // Entries freed through a view's other resource may hold another
        // view by now.
        if views.index.get(&view) != Some(&i) || !view.resources().contains(&resource) {
            continue;
        }
        views.index.remove(&view);
        // (Below the bits that say a record is no table entry.)
        views.list[i as usize].1 = ((generation + 1) & GENERATIONS).max(1);
        // The view's other resource (a counter) keeps a stale entry, which
        // `forget` of that resource skips.
        views.free.push(i);
        forgot_any = true;
    }
    if forgot_any {
        VIEWS_FORGOT.fetch_add(1, std::sync::atomic::Ordering::Release);
    }
}

/// Monotonic counter bumped whenever [`forget`] invalidates a `VIEWS` entry.
#[cfg(windows)]
pub(crate) fn views_forgot() -> u64 {
    VIEWS_FORGOT.load(std::sync::atomic::Ordering::Acquire)
}

/// The view the record words `i` and `generation` name: the one they hold
/// whole, or the view of entry `i` if `generation` is still its own.
pub(in crate::descriptors) fn view_at(i: u32, generation: u32) -> Option<View> {
    match generation >> KIND_SHIFT {
        0 => {
            let views = views_read();
            let (view, current) = views.list.get(i as usize)?;
            (*current == generation).then(|| view.clone())
        }
        CONSTANT => Some(constant_of(i, generation)),
        PLACED => views_read().placed_of(i, generation),
        _ => None,
    }
}

/// The words of the record at `handle`, if they are a view's.
pub(crate) fn record_at(handle: u64) -> Option<[u32; 3]> {
    if handle == 0 {
        return None;
    }
    // SAFETY: the handle is a descriptor slot the title passes back.
    let words = unsafe { (handle as *const [u32; 3]).read_unaligned() };
    (words[0] == VIEW_TAG).then_some(words)
}

/// The view recorded at `handle`, if one is (and its resource is alive).
pub(crate) fn view(handle: u64) -> Option<View> {
    if handle == 0 {
        return None;
    }
    // SAFETY: the handle is a descriptor slot the title passes back.
    let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
    if tag != VIEW_TAG {
        return None;
    }
    view_at(i, generation)
}

/// Whether the view recorded at `handle` still exists, without cloning it.
#[cfg(windows)]
pub(crate) fn view_exists(handle: u64) -> bool {
    if handle == 0 {
        return false;
    }
    // SAFETY: the handle is a descriptor slot the title passes back.
    let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
    if tag != VIEW_TAG {
        return false;
    }
    match generation >> KIND_SHIFT {
        0 => views_read()
            .list
            .get(i as usize)
            .is_some_and(|(_, current)| *current == generation),
        CONSTANT | PLACED => true,
        _ => false,
    }
}
