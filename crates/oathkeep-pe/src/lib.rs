mod image;
pub mod implib;
pub mod layout;

pub use image::{
    CodeViewInfo, ExportInfo, ImageInfo, ImportEntry, ImportModule, ImportSymbol, SectionInfo,
    TlsInfo,
};

#[derive(Debug, thiserror::Error)]
pub enum PeError {
    #[error("unsupported image: {0}")]
    Unsupported(String),
    #[error("malformed image: {0}")]
    Malformed(String),
}

impl From<object::read::Error> for PeError {
    fn from(e: object::read::Error) -> Self {
        PeError::Malformed(e.to_string())
    }
}

pub const MACHINE_AMD64: u16 = 0x8664;
