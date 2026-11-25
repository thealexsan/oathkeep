mod cache;

pub use cache::{Cache, CacheKey};

pub trait Converter: Send + Sync {
    fn id(&self) -> &'static str;
    fn version(&self) -> u32;
    fn convert(&self, input: &[u8]) -> anyhow::Result<Vec<u8>>;
}
