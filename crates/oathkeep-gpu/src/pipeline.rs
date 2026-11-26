#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::HashMap;
use std::mem::offset_of;
use std::sync::{Arc, Mutex, OnceLock};

use crate::object::{self, E_INVALIDARG, HRESULT, Iid, Object, PIPELINE, Table, This, guid};
use crate::resource::PAGEABLE_IIDS;

const PIPELINE_IIDS: [Iid; 4] = [
    guid(
        0x765a30f3,
        0xf624,
        0x4c6f,
        [0xa8, 0x28, 0xac, 0xe9, 0x48, 0x62, 0x24, 0x45],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
pub(crate) struct Bytecode {
    code: *const u8,
    size: usize,
}

#[repr(C)]
pub(crate) struct GraphicsHead {
    root: This,
    stages: [Bytecode; 5],
}

#[repr(C)]
pub(crate) struct ComputeHead {
    root: This,
    stage: Bytecode,
}

#[cfg_attr(test, allow(dead_code))]
enum ContainerStore {
    Empty,
    Inline(Vec<u8>),
    #[cfg(windows)]
    Spilled {
        offset: u64,
        len: usize,
    },
}

#[cfg(windows)]
struct ContainerSpill {
    file: std::fs::File,
    len: u64,
}

#[cfg(windows)]
impl ContainerSpill {
    fn open_at(path: &std::path::Path) -> Option<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
            .open(path)
            .ok()?;
        Some(Self { file, len: 0 })
    }

    fn open_default() -> Option<Self> {
        let dir = crate::shader_cache::cache_dir()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("oathkeep-containers-{}.tmp", std::process::id()));
        Self::open_at(&path)
    }

    fn write(&mut self, bytes: &[u8]) -> Option<u64> {
        use std::io::{Seek, SeekFrom, Write};
        let offset = self.len;
        self.file.seek(SeekFrom::Start(offset)).ok()?;
        self.file.write_all(bytes).ok()?;
        self.len = offset.checked_add(bytes.len() as u64)?;
        Some(offset)
    }

    fn read(&mut self, offset: u64, len: usize) -> Option<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        self.file.seek(SeekFrom::Start(offset)).ok()?;
        let mut buf = vec![0u8; len];
        self.file.read_exact(&mut buf).ok()?;
        Some(buf)
    }
}

#[cfg(windows)]
fn container_spill() -> Option<&'static Mutex<ContainerSpill>> {
    static SPILL: OnceLock<Option<Mutex<ContainerSpill>>> = OnceLock::new();
    SPILL
        .get_or_init(|| {
            if std::env::var_os("OATHKEEP_SPILL_SHADERS").is_some_and(|v| v == "0") {
                return None;
            }
            ContainerSpill::open_default().map(Mutex::new)
        })
        .as_ref()
}

pub(crate) struct Shader {
    pub(crate) stage: &'static str,
    pub(crate) hash: String,
    container_len: usize,
    container: Mutex<ContainerStore>,
    #[cfg(windows)]
    is_spilled: bool,
    #[cfg(windows)]
    pub(crate) ambient: Option<crate::ambient::Role>,
    #[cfg(windows)]
    translated: OnceLock<Option<Translated>>,
}

impl std::fmt::Debug for Shader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.stage, self.hash)
    }
}

static SHADERS: Mutex<Option<HashMap<[u8; 32], Arc<Shader>>>> = Mutex::new(None);

static KEPT: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

#[cfg(windows)]
#[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
pub(crate) fn kept() -> [u64; 7] {
    use std::sync::atomic::Ordering::Relaxed;
    [
        KEPT[0].load(Relaxed),
        KEPT[1].load(Relaxed),
        KEPT[2].load(Relaxed),
        KEPT[3].load(Relaxed),
        library::BLOB.load(Relaxed),
        library::SAVED.load(Relaxed),
        library::TRIMMED_BYTES.load(Relaxed),
    ]
}

#[cfg(windows)]
fn compute_is_spilled(stage: &'static str, bytes: &[u8]) -> bool {
    use oathkeep_shader::Container;
    if stage != "compute" {
        return false;
    }
    let Ok(c) = Container::parse(bytes) else {
        return false;
    };
    oathkeep_shader::native_programs_in_container(&c)
        .ok()
        .is_some_and(|programs| programs.iter().any(oathkeep_shader::spill::is_spilled))
}

fn shader(stage: &'static str, bytes: &[u8]) -> Arc<Shader> {
    let full = blake3::hash(bytes);
    let mut shaders = SHADERS.lock().unwrap_or_else(|e| e.into_inner());
    shaders
        .get_or_insert_with(HashMap::new)
        .entry(*full.as_bytes())
        .or_insert_with(|| {
            KEPT[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            #[cfg(windows)]
            let is_spilled = compute_is_spilled(stage, bytes);
            let store = {
                #[cfg(all(windows, not(test)))]
                {
                    if let Some(spill) = container_spill()
                        && let Some(offset) = spill
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .write(bytes)
                    {
                        ContainerStore::Spilled {
                            offset,
                            len: bytes.len(),
                        }
                    } else {
                        KEPT[1].fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::Relaxed);
                        ContainerStore::Inline(bytes.to_vec())
                    }
                }
                #[cfg(any(not(windows), test))]
                {
                    KEPT[1].fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::Relaxed);
                    ContainerStore::Inline(bytes.to_vec())
                }
            };
            Arc::new(Shader {
                stage,
                hash: full.to_hex()[..16].to_owned(),
                container_len: bytes.len(),
                container: Mutex::new(store),
                #[cfg(windows)]
                is_spilled,
                #[cfg(windows)]
                ambient: (stage == "compute")
                    .then(|| crate::ambient::role(bytes))
                    .flatten(),
                #[cfg(windows)]
                translated: OnceLock::new(),
            })
        })
        .clone()
}

#[cfg(windows)]
pub(crate) struct Translated {
    pub(crate) host: oathkeep_shader::emit::HostShader,
    dxil: Mutex<Vec<u8>>,
    cache_path: Option<std::path::PathBuf>,
    pub(crate) used: Vec<oathkeep_shader::UsedResource>,
    pub(crate) stores: bool,
    pub(crate) bindless_stores: bool,
}

#[cfg(windows)]
impl Translated {
    pub(crate) fn dxil(&self) -> Option<Vec<u8>> {
        {
            let guard = self.dxil.lock().unwrap_or_else(|e| e.into_inner());
            if !guard.is_empty() {
                return Some(guard.clone());
            }
        }
        let path = self.cache_path.as_deref()?;
        crate::shader_cache::load_dxil(path)
    }

    pub(crate) fn evict_dxil(&self) -> usize {
        if self.cache_path.is_none() {
            return 0;
        }
        let Ok(mut guard) = self.dxil.try_lock() else {
            return 0;
        };
        if guard.is_empty() {
            return 0;
        }
        let taken = std::mem::take(&mut *guard);
        let freed = taken.len();
        KEPT[3].fetch_sub(freed as u64, std::sync::atomic::Ordering::Relaxed);
        freed
    }
}

#[cfg(windows)]
impl Shader {
    pub(crate) fn evict_dxil(&self) -> usize {
        self.translated
            .get()
            .and_then(Option::as_ref)
            .map_or(0, Translated::evict_dxil)
    }

    pub(crate) fn translated(&self) -> Option<&Translated> {
        self.translated
            .get_or_init(|| {
                let container = {
                    #[cfg_attr(test, allow(unused_mut))]
                    let mut guard = self.container.lock().unwrap_or_else(|e| e.into_inner());
                    #[cfg(test)]
                    {
                        match &*guard {
                            ContainerStore::Inline(v) => v.clone(),
                            ContainerStore::Spilled { offset, len } => container_spill()
                                .and_then(|s| s.lock().unwrap_or_else(|e| e.into_inner()).read(*offset, *len))
                                .unwrap_or_default(),
                            ContainerStore::Empty => Vec::new(),
                        }
                    }
                    #[cfg(not(test))]
                    {
                        match std::mem::replace(&mut *guard, ContainerStore::Empty) {
                            ContainerStore::Inline(taken) => {
                                KEPT[1].fetch_sub(
                                    taken.len() as u64,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                                taken
                            }
                            ContainerStore::Spilled { offset, len } => container_spill()
                                .and_then(|s| {
                                    s.lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .read(offset, len)
                                })
                                .unwrap_or_default(),
                            ContainerStore::Empty => Vec::new(),
                        }
                    }
                };
                let done = translate(&container);
                if let Some(dir) = std::env::var_os("OATHKEEP_SHADER_SAVE") {
                    let dir = std::path::PathBuf::from(dir);
                    let _ = std::fs::create_dir_all(&dir);
                    let stem = format!("{}.{}", self.hash, self.stage);
                    let _ = std::fs::write(dir.join(format!("{stem}.cdxil")), &container);
                    if let Ok(t) = &done
                        && let Some(dxil) = t.dxil()
                    {
                        let _ = std::fs::write(dir.join(format!("{stem}.dxil")), &dxil);
                    }
                }
                if let Ok(t) = &done {
                    let dxil_len = t.dxil.lock().unwrap_or_else(|e| e.into_inner()).len() as u64;
                    KEPT[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    KEPT[3].fetch_add(dxil_len, std::sync::atomic::Ordering::Relaxed);
                }
                match &done {
                    Ok(t) => tracing::info!(
                        stage = self.stage,
                        hash = %self.hash,
                        user_data = t.host.user_data.len(),
                        samplers = t.host.samplers.len(),
                        "shader translated"
                    ),
                    Err(e) => tracing::warn!(
                        stage = self.stage,
                        hash = %self.hash,
                        error = %e,
                        "shader does not translate; its pipelines' draws are dropped"
                    ),
                }
                done.ok()
            })
            .as_ref()
    }
}

#[cfg(windows)]
fn translate(container: &[u8]) -> Result<Translated, String> {
    use oathkeep_shader::{Container, HwStage};
    crate::descriptor::host::probe_view_limit();
    let c = Container::parse(container).map_err(|e| e.to_string())?;
    let programs = match oathkeep_shader::native_programs_in_container(&c) {
        Ok(p) => p,
        Err(_) => {
            let summary: Vec<String> = c
                .parts
                .iter()
                .map(|p| {
                    let head = p
                        .data
                        .get(..4)
                        .map(|b| format!("{:02x}{:02x}{:02x}{:02x}", b[0], b[1], b[2], b[3]))
                        .unwrap_or_default();
                    let at18 = p
                        .data
                        .get(0x18..0x1c)
                        .map(|b| format!("{:02x}{:02x}{:02x}{:02x}", b[0], b[1], b[2], b[3]))
                        .unwrap_or_default();
                    format!("{}({}B,0x0={},0x18={})", p.fourcc, p.data.len(), head, at18)
                })
                .collect();
            tracing::warn!(parts = ?summary, "container has no native DXIL/XDAT/XBCH part");
            Vec::new()
        }
    };
    let (mut host, cache_path) = crate::shader_cache::host_shader_with_path(container)?;
    let dxil = std::mem::take(&mut host.dxil);
    let stores = programs.is_empty() || programs.iter().any(stores);
    let has_strides = host
        .root_srvs
        .contains(&oathkeep_shader::emit::SLOT_STRIDES);
    let bindless_stores = stores
        && has_strides
        && (programs.is_empty() || programs.iter().any(bindless_stores));
    let used = programs
        .into_iter()
        .find(|p| {
            matches!(
                p.stage,
                HwStage::Vs | HwStage::Ps | HwStage::Ms | HwStage::Cs
            )
        })
        .map(|p| p.used)
        .unwrap_or_default();
    Ok(Translated {
        host,
        dxil: Mutex::new(dxil),
        cache_path,
        used,
        stores,
        bindless_stores,
    })
}

#[cfg(windows)]
pub(crate) fn trim_shaders() -> usize {
    let Ok(guard) = SHADERS.try_lock() else {
        return 0;
    };
    let Some(map) = guard.as_ref() else {
        return 0;
    };
    let mut freed = 0usize;
    for shader in map.values() {
        freed += shader.evict_dxil();
    }
    freed
}

#[cfg(windows)]
struct Builder {
    pipelines: Vec<Arc<make::Pending>>,
    whole: bool,
    off: bool,
}

#[cfg(windows)]
static BUILDER: Mutex<Builder> = Mutex::new(Builder {
    pipelines: Vec::new(),
    whole: false,
    off: false,
});

#[cfg(windows)]
fn of_builder(shaders: &[Arc<Shader>]) -> bool {
    let [shader] = shaders else { return false };
    shader.stage == "compute" && shader.is_spilled
}

#[cfg(windows)]
pub(crate) fn builder_quarantined(this: This) -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    is_builder(this)
        && *ON.get_or_init(|| std::env::var_os("OATHKEEP_BVH_QUARANTINE").is_some_and(|v| v == "1"))
}

#[cfg(windows)]
pub(crate) fn is_builder(this: This) -> bool {
    !this.is_null() && pipeline(this).builder
}

#[cfg(windows)]
pub(crate) fn builder_off(why: &'static str) {
    let mut b = BUILDER.lock().unwrap_or_else(|e| e.into_inner());
    if !b.off {
        b.off = true;
        tracing::warn!(
            why,
            "the ray-tracing structure builder is turned off for the rest of the run: its stages run all or none"
        );
    }
}

#[cfg(windows)]
pub(crate) fn builder_runs() -> bool {
    static ALLOWED: OnceLock<bool> = OnceLock::new();
    if !*ALLOWED.get_or_init(|| std::env::var_os("OATHKEEP_BVH_BUILDER").is_some_and(|v| v == "1"))
    {
        return false;
    }
    let mut b = BUILDER.lock().unwrap_or_else(|e| e.into_inner());
    if b.off {
        return false;
    }
    if b.whole {
        return true;
    }
    let missing = b.pipelines.iter().filter(|p| !make::now(p)).count();
    if missing > 0 {
        b.off = true;
        tracing::warn!(
            missing,
            of = b.pipelines.len(),
            "the ray-tracing structure builder stays off: not all of its programs have a host pipeline, and its stages run all or none"
        );
        return false;
    }
    tracing::info!(
        pipelines = b.pipelines.len(),
        "the ray-tracing structure builder runs: all of its programs have host pipelines"
    );
    b.whole = true;
    true
}

#[cfg(windows)]
fn stores(program: &oathkeep_shader::NativeProgram) -> bool {
    use oathkeep_shader::gfx10::{Encoding, decode};
    let Ok(code) = decode(&program.code) else {
        return true;
    };
    code.iter().any(|i| {
        let name = i.name();
        match i.encoding {
            Encoding::Mubuf
            | Encoding::Mtbuf
            | Encoding::Smem
            | Encoding::Flat
            | Encoding::Global => name.is_none_or(|n| {
                (n.contains("store") || n.contains("atomic")) && !n.contains("scratch")
            }),
            Encoding::Sop1 => name.is_none_or(|n| matches!(n, "s_setpc_b64" | "s_swappc_b64")),
            _ => false,
        }
    })
}

#[cfg(windows)]
fn bindless_stores(program: &oathkeep_shader::NativeProgram) -> bool {
    use oathkeep_shader::RootType;
    use oathkeep_shader::gfx10::{Encoding, decode};
    use oathkeep_shader::resource::{Role, Source};
    let Ok(code) = decode(&program.code) else {
        return true;
    };
    let mut has_buf_store = false;
    for i in &code {
        let name = i.name();
        match i.encoding {
            Encoding::Flat | Encoding::Global => {
                if name.is_none_or(|n| {
                    (n.contains("store") || n.contains("atomic")) && !n.contains("scratch")
                }) {
                    return true;
                }
            }
            Encoding::Sop1 => {
                if name.is_none_or(|n| matches!(n, "s_setpc_b64" | "s_swappc_b64")) {
                    return true;
                }
            }
            Encoding::Mubuf | Encoding::Mtbuf | Encoding::Smem => {
                if name.is_none_or(|n| {
                    (n.contains("store") || n.contains("atomic")) && !n.contains("scratch")
                }) {
                    has_buf_store = true;
                }
            }
            _ => {}
        }
    }
    if !has_buf_store {
        return false;
    }
    let Ok(ir) = oathkeep_shader::spill::build(program) else {
        return true;
    };
    let st = oathkeep_shader::structure::structure(&ir);
    let Ok(res) =
        oathkeep_shader::resource::track(&ir, &program.layout, program.stage, st.as_ref().ok())
    else {
        return true;
    };
    let scalar: HashMap<_, _> = res.scalar.iter().map(|&(site, addr)| (site, addr)).collect();
    let descs: HashMap<_, _> = res
        .descriptors
        .iter()
        .filter(|d| d.role == Role::Resource)
        .map(|d| (d.site, &d.source))
        .collect();
    for (b, block) in ir.blocks.iter().enumerate() {
        for (idx, inst) in block.insts.iter().enumerate() {
            if !matches!(
                inst.machine.encoding,
                Encoding::Mubuf | Encoding::Mtbuf | Encoding::Smem
            ) {
                continue;
            }
            let name = inst.machine.name();
            if !name.is_none_or(|n| {
                (n.contains("store") || n.contains("atomic")) && !n.contains("scratch")
            }) {
                continue;
            }
            let Some(source) = descs.get(&(b, idx)) else {
                return true;
            };
            for leaf in source.leaves() {
                match leaf {
                    Source::Loaded { load, .. } => {
                        let Some(addr) = scalar.get(load) else {
                            return true;
                        };
                        let Some(param) = program.layout.get(addr.param) else {
                            return true;
                        };
                        if param.root != RootType::Table {
                            return true;
                        }
                        if addr.dynamic.is_some()
                            && !res
                                .root_indexed_views
                                .iter()
                                .any(|ix| ix.table_slot == param.slot)
                        {
                            return true;
                        }
                    }
                    Source::Built { address, .. } => {
                        let Some(param) = program.layout.get(address.param) else {
                            return true;
                        };
                        if !matches!(param.root, RootType::Descriptor | RootType::Table) {
                            return true;
                        }
                    }
                    Source::Constant(..) | Source::ImmediateSampler { .. } => {}
                    _ => return true,
                }
            }
        }
    }
    false
}

#[repr(C)]
struct Pipeline {
    object: Object,
    packet: [u32; 8],
    shaders: Vec<Arc<Shader>>,
    bound: std::sync::atomic::AtomicBool,
    #[cfg(windows)]
    host: Arc<make::Pending>,
    #[cfg(windows)]
    builder: bool,
}
const _: () = assert!(offset_of!(Pipeline, packet) == 0x10);

const GRAPHICS_STAGES: [&str; 5] = ["vertex", "pixel", "domain", "hull", "geometry"];

fn pipeline<'a>(this: This) -> &'a Pipeline {
    // SAFETY: the title passes one of our pipelines.
    unsafe { &*this.cast::<Pipeline>() }
}

pub(crate) fn bound(this: This) {
    if this.is_null() {
        return;
    }
    let p = pipeline(this);
    if p.bound.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let frame = crate::frame::current();
    for shader in &p.shaders {
        tracing::info!(pipeline = ?this, frame, stage = shader.stage, size = shader.container_len, hash = %shader.hash, "first bind");
    }
}

#[cfg(windows)]
pub(crate) fn host_pipeline<'a>(this: This) -> Option<&'a host::HostPipeline> {
    if this.is_null() {
        return None;
    }
    make::ready(&pipeline(this).host)
}

#[cfg(windows)]
pub(crate) fn making() -> u64 {
    make::waiting()
}

#[cfg(all(windows, feature = "diagnostics"))]
static DROPS: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

#[cfg(all(windows, feature = "diagnostics"))]
fn parse_skip_hashes(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|hash| !hash.is_empty())
        .filter_map(|hash| {
            if hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                Some(hash.to_ascii_lowercase())
            } else {
                tracing::warn!(
                    hash,
                    "OATHKEEP_SKIP_PIPELINES expects 16-digit shader hashes"
                );
                None
            }
        })
        .collect()
}

#[cfg(all(windows, feature = "diagnostics"))]
fn skipped_shader(p: &Pipeline) -> Option<&str> {
    static HASHES: OnceLock<Vec<String>> = OnceLock::new();
    let hashes = HASHES.get_or_init(|| {
        let value = std::env::var("OATHKEEP_SKIP_PIPELINES").unwrap_or_default();
        parse_skip_hashes(&value)
    });
    if hashes.is_empty() {
        return None;
    }
    p.shaders
        .iter()
        .find(|shader| hashes.contains(&shader.hash))
        .map(|shader| shader.hash.as_str())
}

#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn is_skipped(this: This) -> bool {
    !this.is_null() && skipped_shader(pipeline(this)).is_some()
}

#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn note_drop(this: This) {
    let why = if this.is_null() {
        Some("no pipeline set".to_owned())
    } else {
        let p = pipeline(this);
        skipped_shader(p)
            .map(|hash| format!("skipped shader {hash}"))
            .or_else(|| make::missing(&p.host))
    };
    if let Some(why) = why {
        note_why(why);
    }
}

#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn note_why(why: String) {
    crate::capture::op(0, || format!("DROPPED {why}"));
    let mut drops = DROPS.lock().unwrap_or_else(|e| e.into_inner());
    *drops
        .get_or_insert_with(HashMap::new)
        .entry(why)
        .or_default() += 1;
}

#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn take_drops() -> Vec<(String, u64)> {
    let drops = DROPS.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut drops: Vec<(String, u64)> = drops.into_iter().flatten().collect();
    drops.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    drops
}

fn copy(stage: &'static str, code: &Bytecode) -> Option<Arc<Shader>> {
    if code.code.is_null() || code.size == 0 {
        return None;
    }
    // SAFETY: the title passes `size` bytes of shader code.
    Some(shader(stage, unsafe {
        std::slice::from_raw_parts(code.code, code.size)
    }))
}

fn hand_out(
    shaders: Vec<Arc<Shader>>,
    #[cfg(windows)] graphics: Option<Box<host::GraphicsState>>,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    tracing::trace!(?shaders, "pipeline state");
    #[cfg(windows)]
    let host = Arc::new(make::Pending::new(shaders.clone(), graphics));
    #[cfg(windows)]
    make::early(&host);
    #[cfg(windows)]
    let builder = of_builder(&shaders);
    #[cfg(windows)]
    if builder {
        let mut b = BUILDER.lock().unwrap_or_else(|e| e.into_inner());
        b.pipelines.push(host.clone());
        b.whole = false;
    }
    let pipeline = Box::new(Pipeline {
        object: Object::new(&FUNCTIONS),
        packet: [0; 8],
        #[cfg(windows)]
        host,
        #[cfg(windows)]
        builder,
        shaders,
        bound: std::sync::atomic::AtomicBool::new(false),
    });
    object::hand_out(PIPELINE, pipeline, iid, &PIPELINE_IIDS, out)
}

pub(crate) extern "system" fn create_graphics(
    _: This,
    desc: *const GraphicsHead,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    let stages = GRAPHICS_STAGES.iter().zip(&desc.stages);
    let shaders = stages.filter_map(|(s, c)| copy(s, c)).collect();
    hand_out(
        shaders,
        #[cfg(windows)]
        None,
        iid,
        out,
    )
}

pub(crate) extern "system" fn create_compute(
    _: This,
    desc: *const ComputeHead,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    hand_out(
        copy("compute", &desc.stage).into_iter().collect(),
        #[cfg(windows)]
        None,
        iid,
        out,
    )
}

pub(crate) extern "system" fn create_compute_x(
    this: This,
    desc: *const ComputeHead,
    _extensions: u32,
    _extended: *const u8,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    create_compute(this, desc, iid, out)
}

#[repr(C)]
pub(crate) struct StreamDesc {
    size: usize,
    stream: *const u8,
}

fn subobject(kind: u32) -> Option<(usize, Option<&'static str>)> {
    let stage = |name| Some((24, Some(name)));
    match kind {
        1 => stage("vertex"),
        2 => stage("pixel"),
        3 => stage("domain"),
        4 => stage("hull"),
        5 => stage("geometry"),
        6 => stage("compute"),
        23 => stage("amplification"),
        24 => stage("mesh"),
        0 => Some((16, None)),
        12 | 19 => Some((24, None)),
        7 | 15 => Some((40, None)),
        8 => Some((336, None)),
        10 => Some((48, None)),
        11 => Some((56, None)),
        17 => Some((16, None)),
        21 => Some((64, None)),
        22 => Some((32, None)),
        9 | 13 | 14 | 16 | 18 | 20 => Some((8, None)),
        _ => None,
    }
}

pub(crate) extern "system" fn create_stream(
    _: This,
    desc: *const StreamDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    if desc.stream.is_null() {
        return E_INVALIDARG;
    }
    let mut shaders = Vec::new();
    #[cfg(windows)]
    let mut graphics = host::GraphicsState::default();
    let mut at = 0;
    while at + 8 <= desc.size {
        // SAFETY: inside the stream the title passes.
        let kind = unsafe { desc.stream.add(at).cast::<u32>().read_unaligned() };
        let Some((size, stage)) = subobject(kind) else {
            tracing::warn!(
                kind,
                at,
                "pipeline subobject of unknown size; the rest is ignored"
            );
            break;
        };
        if let Some(stage) = stage {
            // SAFETY: a shader subobject has its bytecode at +8.
            let code = unsafe { &*desc.stream.add(at + 8).cast::<Bytecode>() };
            shaders.extend(copy(stage, code));
        }
        #[cfg(windows)]
        // SAFETY: a whole subobject of this kind lies at `at`.
        unsafe {
            graphics.read(kind, desc.stream.add(at))
        };
        at += size;
    }
    hand_out(
        shaders,
        #[cfg(windows)]
        Some(Box::new(graphics)),
        iid,
        out,
    )
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = object::is(iid, &PIPELINE_IIDS).then_some(this);
    object::answer(PIPELINE, iid, found, out)
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Pipeline>(this)
}

static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<PIPELINE, 24>());
    f[0] = query as *const ();
    f[2] = release as *const ();
    f[7] = object::parent_device as *const ();
    Table(f)
};

#[cfg(windows)]
pub(crate) mod host;
#[cfg(windows)]
mod library;
#[cfg(windows)]
mod make;

#[cfg(windows)]
pub(crate) fn trim_library() {
    library::trim();
}

#[cfg(windows)]
pub(crate) fn library_removal_note() -> String {
    library::removal_note()
}

#[cfg(windows)]
pub fn use_pipeline_cache(dir: std::path::PathBuf) {
    library::use_pipeline_cache(dir);
}

#[cfg(windows)]
pub fn save_pipeline_cache() {
    library::save(true);
}

#[cfg(test)]
mod tests {
    #[cfg(all(windows, feature = "diagnostics"))]
    #[test]
    fn skip_hashes_accepts_full_hashes_only() {
        assert!(super::parse_skip_hashes(" , ").is_empty());
        assert_eq!(
            super::parse_skip_hashes(" 0123456789ABCDEF, 0123456789abcde, xyz, 0011223344556677,"),
            ["0123456789abcdef", "0011223344556677"]
        );
    }

    use super::*;
    use crate::object::{S_OK, slot};

    fn shaders(p: This) -> Vec<(&'static str, Vec<u8>)> {
        pipeline(p)
            .shaders
            .iter()
            .map(|s| {
                let bytes = match &*s.container.lock().unwrap_or_else(|e| e.into_inner()) {
                    ContainerStore::Inline(v) => v.clone(),
                    _ => Vec::new(),
                };
                (s.stage, bytes)
            })
            .collect()
    }

    #[cfg(windows)]
    #[test]
    fn container_spill_round_trips_and_deletes_on_close() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("containers-test.tmp");
        let mut spill = ContainerSpill::open_at(&path).expect("open spill file");
        let a = b"shader container alpha";
        let b = b"shader container beta with more bytes";
        let off_a = spill.write(a).expect("write a");
        let off_b = spill.write(b).expect("write b");
        assert_eq!(spill.read(off_a, a.len()).as_deref(), Some(&a[..]));
        assert_eq!(spill.read(off_b, b.len()).as_deref(), Some(&b[..]));
        drop(spill);
        assert!(!path.exists(), "delete-on-close removes spill file on drop");
    }

    #[test]
    fn pipelines_keep_their_shaders() {
        let (vs, ps) = ([1u8, 2, 3], [4u8; 5]);
        let none = || Bytecode {
            code: std::ptr::null(),
            size: 0,
        };
        let desc = GraphicsHead {
            root: std::ptr::null(),
            stages: [
                Bytecode {
                    code: vs.as_ptr(),
                    size: vs.len(),
                },
                Bytecode {
                    code: ps.as_ptr(),
                    size: ps.len(),
                },
                none(),
                none(),
                none(),
            ],
        };
        let mut p = std::ptr::null();
        assert_eq!(
            create_graphics(std::ptr::null(), &desc, &PIPELINE_IIDS[0], &mut p),
            S_OK
        );
        assert_eq!(
            shaders(p),
            [("vertex", vs.to_vec()), ("pixel", ps.to_vec())]
        );
        let mut q = std::ptr::null();
        create_graphics(std::ptr::null(), &desc, &PIPELINE_IIDS[0], &mut q);
        assert!(Arc::ptr_eq(
            &pipeline(p).shaders[1],
            &pipeline(q).shaders[1]
        ));
        assert_eq!(slot::<extern "system" fn(This) -> u32>(p, 0x10)(p), 0);
    }

    #[test]
    fn streams_yield_their_shaders() {
        let (ms, ps) = ([9u8; 7], [4u8; 5]);
        let mut stream = vec![0u8; 0x2b0];
        let mut put = |at: usize, v: u64| stream[at..at + 8].copy_from_slice(&v.to_le_bytes());
        for (at, kind) in [
            (0x10, 24),
            (0x28, 2),
            (0x40, 3),
            (0x58, 4),
            (0x70, 5),
            (0x88, 8),
            (0x1d8, 9),
            (0x1e0, 10),
            (0x210, 21),
            (0x250, 17),
            (0x260, 12),
            (0x278, 14),
            (0x280, 15),
            (0x2a8, 16),
        ] {
            put(at, kind);
        }
        put(0x18, ms.as_ptr() as u64);
        put(0x20, ms.len() as u64);
        put(0x30, ps.as_ptr() as u64);
        put(0x38, ps.len() as u64);
        put(0x278, 14 | 3 << 32);
        put(0x280, 15 | 28 << 32);
        put(0x2a0, 1 << 32);
        let desc = StreamDesc {
            size: stream.len(),
            stream: stream.as_ptr(),
        };
        let mut p = std::ptr::null();
        let hr = create_stream(std::ptr::null(), &desc, &PIPELINE_IIDS[0], &mut p);
        assert_eq!(hr, S_OK);
        assert_eq!(shaders(p), [("mesh", ms.to_vec()), ("pixel", ps.to_vec())]);
        #[cfg(windows)]
        {
            let guard = pipeline(p)
                .host
                .graphics
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let g = guard.as_deref().unwrap();
            assert_eq!(g.topology.0, 3);
            assert_eq!(
                (g.targets.NumRenderTargets, g.targets.RTFormats[0].0),
                (1, 28)
            );
        }
    }
}
