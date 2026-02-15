use crate::error::{Result, SteamVentError};
use std::io::{Cursor, Read};

pub struct ZipArchiveExtractor<'a> {
    data: &'a [u8],
}

impl<'a> ZipArchiveExtractor<'a> {
    pub fn new(data: &'a [u8]) -> Result<Self> {
        if !data.starts_with(b"PK") {
            return Err(SteamVentError::InvalidData(
                "Data is not a valid ZIP archive".to_string(),
            ));
        }
        Ok(Self { data })
    }

    pub fn extract(&self) -> Result<Vec<u8>> {
        log::debug!("Unzipping data...");
        let mut zip = zip::ZipArchive::new(Cursor::new(self.data))
            .map_err(|e| SteamVentError::Manifest(e.to_string()))?;
        let mut file = zip
            .by_index(0)
            .map_err(|e| SteamVentError::Manifest(e.to_string()))?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)
            .map_err(|e| SteamVentError::Io(e))?;
        Ok(buffer)
    }
}
