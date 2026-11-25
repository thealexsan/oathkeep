use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

const GIB: usize = 1 << 30;
const MIB: usize = 1 << 20;
const ALIGN: usize = 2 * MIB;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Console {
    #[default]
    SeriesX,
    SeriesS,
}

impl Console {
    pub const fn title_memory(self) -> (usize, usize) {
        match self {
            Console::SeriesX => (10 * GIB, 7 * GIB / 2),
            Console::SeriesS => (0, 7_776 << 20),
        }
    }

    pub fn reported_title_memory(self) -> (usize, usize) {
        static OVERRIDE: OnceLock<Option<TitleMemoryOverride>> = OnceLock::new();
        let over = *OVERRIDE.get_or_init(|| {
            let raw = std::env::var("OATHKEEP_TITLE_MEMORY_MB").ok()?;
            let parsed = parse_title_memory_mb(&raw);
            if let Some(ov) = parsed {
                let (gpu, std) = ov.apply(current());
                tracing::info!(
                    gpu_optimal_mb = gpu >> 20,
                    standard_mb = std >> 20,
                    total_mb = (gpu + std) >> 20,
                    "OATHKEEP_TITLE_MEMORY_MB override active"
                );
            } else if !raw.trim().is_empty() {
                tracing::warn!(
                    value = %raw,
                    "ignoring invalid OATHKEEP_TITLE_MEMORY_MB (expected total MB or gpu_mb,std_mb)"
                );
            }
            parsed
        });
        match over {
            Some(ov) => ov.apply(self),
            None => self.title_memory(),
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Console::SeriesX => "Xbox Series X",
            Console::SeriesS => "Xbox Series S",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TitleMemoryOverride {
    TotalMb(usize),
    SplitMb(usize, usize),
}

impl TitleMemoryOverride {
    fn apply(self, console: Console) -> (usize, usize) {
        let align = |bytes: usize| (bytes / ALIGN) * ALIGN;
        match self {
            TitleMemoryOverride::SplitMb(gpu_mb, std_mb) => {
                (align(gpu_mb.saturating_mul(MIB)), align(std_mb.saturating_mul(MIB)))
            }
            TitleMemoryOverride::TotalMb(total_mb) => {
                let total = align(total_mb.saturating_mul(MIB));
                match console {
                    Console::SeriesS => (0, total),
                    Console::SeriesX => {
                        let (base_gpu, base_std) = Console::SeriesX.title_memory();
                        let base_total = base_gpu + base_std;
                        let gpu = align(((total as u128 * base_gpu as u128) / base_total as u128) as usize);
                        (gpu, total.saturating_sub(gpu))
                    }
                }
            }
        }
    }
}

fn parse_title_memory_mb(raw: &str) -> Option<TitleMemoryOverride> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Some((gpu, std)) = s.split_once(',') {
        let gpu_mb = gpu.trim().parse::<usize>().ok()?;
        let std_mb = std.trim().parse::<usize>().ok()?;
        if gpu_mb + std_mb < 1024 {
            return None;
        }
        return Some(TitleMemoryOverride::SplitMb(gpu_mb, std_mb));
    }
    let total_mb = s.parse::<usize>().ok()?;
    if total_mb < 1024 {
        return None;
    }
    Some(TitleMemoryOverride::TotalMb(total_mb))
}

static SERIES_S: AtomicBool = AtomicBool::new(false);

pub fn set(console: Console) {
    SERIES_S.store(console == Console::SeriesS, Ordering::Relaxed);
}

pub fn current() -> Console {
    if SERIES_S.load(Ordering::Relaxed) {
        Console::SeriesS
    } else {
        Console::SeriesX
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_memory_override_parses_total_and_split() {
        let total = parse_title_memory_mb("9216").expect("total mb");
        let (gpu, std) = total.apply(Console::SeriesX);
        assert_eq!(gpu + std, 9216 * MIB);
        assert_eq!(gpu % ALIGN, 0);
        assert_eq!(std % ALIGN, 0);
        assert!(gpu > std);

        let (s_gpu, s_std) = total.apply(Console::SeriesS);
        assert_eq!((s_gpu, s_std), (0, 9216 * MIB));

        let split = parse_title_memory_mb("6144, 3072").expect("split mb");
        assert_eq!(split.apply(Console::SeriesX), (6144 * MIB, 3072 * MIB));

        assert!(parse_title_memory_mb("").is_none());
        assert!(parse_title_memory_mb("512").is_none());
        assert!(parse_title_memory_mb("bad").is_none());
    }
}
