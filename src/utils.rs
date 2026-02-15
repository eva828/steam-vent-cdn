use crate::error::{Result, SteamVentError};
use std::io::{Cursor, Read};

pub trait ArchiveExtractor: Send + Sync {
    fn maybe_extract(&self, data: &[u8]) -> Result<Vec<u8>>;
}

pub struct ZipArchiveExtractor;

impl ArchiveExtractor for ZipArchiveExtractor {
    fn maybe_extract(&self, data: &[u8]) -> Result<Vec<u8>> {
        if data.starts_with(b"PK") {
            log::debug!("Unzipping data...");
            let mut zip = zip::ZipArchive::new(Cursor::new(data))
                .map_err(|e| SteamVentError::Manifest(e.to_string()))?;
            let mut file = zip
                .by_index(0)
                .map_err(|e| SteamVentError::Manifest(e.to_string()))?;
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer)
                .map_err(|e| SteamVentError::Io(e))?;
            Ok(buffer)
        } else {
            Ok(data.to_vec())
        }
    }
}
