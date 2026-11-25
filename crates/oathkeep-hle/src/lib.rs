mod host;
pub mod modules;
mod registry;
pub mod runtime;

pub use host::HostPolicy;
pub use oathkeep_gpu::console;
pub use registry::{HleExport, HleModule, registry};

use oathkeep_loader::{ImportResolver, Provenance, Resolution};
use oathkeep_pe::ImportSymbol;

#[derive(Debug, Default)]
pub struct HleResolver {
    pub host: HostPolicy,
}

impl HleResolver {
    pub fn new(host: HostPolicy) -> Self {
        HleResolver { host }
    }

    pub fn classify(&self, dll: &str, symbol: &ImportSymbol) -> Option<Provenance> {
        match self.resolve(dll, symbol) {
            Resolution::Found { provenance, .. } => Some(provenance),
            Resolution::Missing => None,
        }
    }
}

impl ImportResolver for HleResolver {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        if let Some(address) = registry::lookup(dll, &symbol.to_string()) {
            return Resolution::Found {
                address,
                provenance: Provenance::Hle,
            };
        }
        match self.host.resolve(dll, symbol) {
            Some(address) => Resolution::Found {
                address,
                provenance: Provenance::Host,
            },
            None => Resolution::Missing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(n: &str) -> ImportSymbol {
        ImportSymbol::Name {
            hint: 0,
            name: n.into(),
        }
    }

    #[test]
    fn console_api_resolves_to_hle() {
        let r = HleResolver::new(HostPolicy::none());
        assert_eq!(
            r.classify("XGameRuntime.dll", &name("XGameRuntimeInitialize")),
            Some(Provenance::Hle)
        );
    }

    #[test]
    fn strict_policy_reports_missing() {
        let r = HleResolver::new(HostPolicy::none());
        assert_eq!(
            r.classify("api-ms-win-core-synch-l1-1-0.dll", &name("Sleep")),
            None
        );
    }

    #[cfg(windows)]
    #[test]
    fn shared_api_sets_pass_through_to_host() {
        let r = HleResolver::default();
        assert_eq!(
            r.classify("api-ms-win-core-synch-l1-2-0.dll", &name("Sleep")),
            Some(Provenance::Host)
        );
        assert_eq!(
            r.classify("xmem.dll", &name("XMemSetAllocationHooks")),
            None
        );
    }

    #[cfg(windows)]
    #[test]
    fn overrides_win_over_host() {
        let r = HleResolver::default();
        assert_eq!(
            r.classify(
                "api-ms-win-core-libraryloader-l1-2-0.dll",
                &name("GetModuleHandleW")
            ),
            Some(Provenance::Hle)
        );
    }
}
