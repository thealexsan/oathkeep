use std::collections::HashMap;
use std::sync::{Arc, Mutex};
#[cfg(windows)]
use std::sync::OnceLock;

/// A shader container a title passed, shared by every pipeline with the
/// same bytes.
pub(crate) struct Shader {
    pub(crate) stage: &'static str,
    /// The first 8 bytes of its BLAKE3, as logged.
    pub(crate) hash: String,
    pub(super) container: Vec<u8>,
    /// Its part in the stand-in for the indirect light.
    #[cfg(windows)]
    pub(crate) ambient: Option<crate::ambient::Role>,
    #[cfg(windows)]
    pub(super) translated: OnceLock<Option<Translated>>,
}

impl std::fmt::Debug for Shader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.stage, self.hash)
    }
}

/// Every container seen, by full content hash.
static SHADERS: Mutex<Option<HashMap<[u8; 32], Arc<Shader>>>> = Mutex::new(None);

/// For the memory lines: containers kept and their bytes, translations
/// kept and the bytes of their host code. None of them is ever let go.
static KEPT: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// What the shader table and the pipeline library keep in memory:
/// containers and their bytes, translations and their bytes, the bytes of
/// the library's file (kept whole while the library lives) and what its
/// last save wrote.
#[cfg(windows)]
#[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
pub(crate) fn kept() -> [u64; 7] {
    use std::sync::atomic::Ordering::Relaxed;
    use super::library;
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

/// The shared container for `bytes`.
pub(super) fn shader(stage: &'static str, bytes: &[u8]) -> Arc<Shader> {
    let full = blake3::hash(bytes);
    let mut shaders = SHADERS.lock().unwrap_or_else(|e| e.into_inner());
    shaders
        .get_or_insert_with(HashMap::new)
        .entry(*full.as_bytes())
        .or_insert_with(|| {
            KEPT[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            KEPT[1].fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::Relaxed);
            Arc::new(Shader {
                stage,
                hash: full.to_hex()[..16].to_owned(),
                container: bytes.to_vec(),
                #[cfg(windows)]
                ambient: (stage == "compute").then(|| crate::ambient::role(bytes)).flatten(),
                #[cfg(windows)]
                translated: OnceLock::new(),
            })
        })
        .clone()
}

/// A translated shader and the console resources its code reads.
#[cfg(windows)]
pub(crate) struct Translated {
    pub(crate) host: oathkeep_shader::emit::HostShader,
    /// A compute program's threads in a group.
    pub(crate) threads: Option<[u32; 3]>,
    pub(crate) used: Vec<oathkeep_shader::UsedResource>,
    pub(crate) tables: crate::draw::TablePlan,
    /// Whether the shader's code can store to buffer memory ([`stores`]);
    /// true when its code is not known.
    pub(crate) stores: bool,
    /// Whether it can store through raw addresses ([`stores_by_address`]).
    pub(crate) stores_by_address: bool,
    /// Whether it stores through a view it picks from the heap by an index,
    /// once asked (`crate::picked`).
    pub(crate) picks: std::sync::atomic::AtomicU8,
}

#[cfg(windows)]
impl Shader {
    /// The translation, made on first use; a shader that does not
    /// translate is logged once.
    pub(crate) fn translated(&self) -> Option<&Translated> {
        self.translated
            .get_or_init(|| {
                let done = translate(&self.container);
                // `OATHKEEP_SHADER_SAVE=<dir>`: each shader as the title gave it (`.cdxil`) and as
                // translated (`.dxil`), by the hash crash reports name it with
                if let Some(dir) = std::env::var_os("OATHKEEP_SHADER_SAVE") {
                    let dir = std::path::PathBuf::from(dir);
                    let _ = std::fs::create_dir_all(&dir);
                    let stem = format!("{}.{}", self.hash, self.stage);
                    let _ = std::fs::write(dir.join(format!("{stem}.cdxil")), &self.container);
                    if let Ok(t) = &done {
                        let _ = std::fs::write(dir.join(format!("{stem}.dxil")), &t.host.dxil);
                    }
                }
                if let Ok(t) = &done {
                    KEPT[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    KEPT[3].fetch_add(t.host.dxil.len() as u64, std::sync::atomic::Ordering::Relaxed);
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
impl Shader {
    /// The console programs of the container; `None` if they are not known.
    pub(crate) fn programs(&self) -> Option<Vec<oathkeep_shader::NativeProgram>> {
        use oathkeep_shader::{Container, FourCC};
        let c = Container::parse(&self.container).ok()?;
        let part = c.part(FourCC::DXIL).or(c.part(FourCC::ACCC))?;
        oathkeep_shader::native_programs(part.data).ok()
    }
}

#[cfg(windows)]
fn translate(container: &[u8]) -> Result<Translated, String> {
    use oathkeep_shader::{Container, FourCC, HwStage};
    // The translation depends on it (a view index past a capped heap).
    crate::descriptor::host::probe_view_limit();
    let c = Container::parse(container).map_err(|e| e.to_string())?;
    let programs = match c.part(FourCC::DXIL).or(c.part(FourCC::ACCC)) {
        Some(part) => oathkeep_shader::native_programs(part.data).map_err(|e| e.to_string())?,
        None => Vec::new(),
    };
    let host = crate::shader_cache::host_shader(container)?;
    let stores = programs.is_empty() || programs.iter().any(stores);
    let threads = programs.iter().find_map(|p| match &p.io {
        oathkeep_shader::native::StageIo::Cs(cs) if cs.threads.iter().all(|&n| n > 0) => Some(cs.threads),
        _ => None,
    });
    let stores_by_address = programs.is_empty() || programs.iter().any(stores_by_address);
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
    let tables = crate::draw::TablePlan::new(&used, &host.root_indexed_views);
    Ok(Translated {
        host,
        threads,
        used,
        tables,
        stores,
        stores_by_address,
        picks: Default::default(),
    })
}

/// Whether a console program can store to buffer memory: it has a buffer,
/// typed-buffer, scalar-memory, flat or global store or atomic, or code
/// that is not all known (an instruction of those kinds outside our table,
/// an indirect jump, code that does not decode). Stores to textures
/// (image instructions) and to the program's own scratch memory do not
/// count: neither reaches a buffer's host copy. After upstream #413,
/// which leaves scalar-memory stores out and counts image stores.
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

/// Whether a console program can store to memory through raw addresses
/// (`crate::memory`): a flat or global store or atomic, or code that does
/// not decode. The programs that only walk ray-tracing structures do not,
/// whatever they store through views.
#[cfg(windows)]
fn stores_by_address(program: &oathkeep_shader::NativeProgram) -> bool {
    use oathkeep_shader::gfx10::{Encoding, decode};
    let Ok(code) = decode(&program.code) else {
        return true;
    };
    code.iter().any(|i| {
        matches!(i.encoding, Encoding::Flat | Encoding::Global)
            && i.name()
                .is_none_or(|n| (n.contains("store") || n.contains("atomic")) && !n.contains("scratch"))
    })
}
