use std::sync::LazyLock;

use crate::modules;

#[derive(Debug, Clone, Copy)]
pub struct HleExport {
    pub name: &'static str,
    pub address: usize,
}

#[derive(Debug)]
pub struct HleModule {
    pub name: &'static str,
    pub dlls: &'static [&'static str],
    pub exports: Vec<HleExport>,
}

impl HleModule {
    pub fn serves(&self, dll: &str) -> bool {
        let dll = dll.to_ascii_lowercase();
        self.dlls
            .iter()
            .any(|pattern| match pattern.strip_suffix('*') {
                Some(prefix) => dll.starts_with(prefix),
                None => dll == *pattern,
            })
    }
}

#[macro_export]
macro_rules! hle_export {
    ($f:ident) => {
        $crate::HleExport {
            name: stringify!($f),
            address: $f as *const () as usize,
        }
    };
}

static REGISTRY: LazyLock<Vec<HleModule>> = LazyLock::new(modules::all);

pub fn registry() -> &'static [HleModule] {
    &REGISTRY
}

pub(crate) fn lookup(dll: &str, name: &str) -> Option<usize> {
    registry()
        .iter()
        .filter(|m| m.serves(dll))
        .flat_map(|m| m.exports.iter())
        .find(|e| e.name == name)
        .map(|e| e.address)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_names_are_unique_per_dll_pattern() {
        for module in registry() {
            let mut names: Vec<_> = module.exports.iter().map(|e| e.name).collect();
            names.sort_unstable();
            let before = names.len();
            names.dedup();
            assert_eq!(
                before,
                names.len(),
                "duplicate export in module {}",
                module.name
            );
        }
    }

    #[test]
    fn api_set_prefix_matching() {
        let m = HleModule {
            name: "t",
            dlls: &["api-ms-win-core-libraryloader-*"],
            exports: vec![],
        };
        assert!(m.serves("API-MS-WIN-CORE-LIBRARYLOADER-L1-2-0.dll"));
        assert!(!m.serves("api-ms-win-core-file-l1-1-0.dll"));
    }
}
