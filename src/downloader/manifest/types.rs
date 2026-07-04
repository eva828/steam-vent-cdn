use crate::downloader::cdn::DepotContext;
use crate::error::{ManifestError, Result, SteamVentError};
use std::sync::Arc;
use steam_vent_proto::content_manifest::content_manifest_payload::file_mapping::ChunkData;

#[derive(Debug, Clone)]
pub struct ChunkInfo {
    pub sha1: Vec<u8>,
    pub crc: u32,
    pub offset: u64,
    pub uncompressed_size: u32,
    pub compressed_size: u32,
}

impl TryFrom<ChunkData> for ChunkInfo {
    type Error = SteamVentError;

    fn try_from(mut c: ChunkData) -> Result<Self> {
        Ok(ChunkInfo {
            sha1: c.sha.take().ok_or_else(|| {
                SteamVentError::Manifest(ManifestError::MissingField("chunk.sha"))
            })?,
            crc: c.crc.take().ok_or_else(|| {
                SteamVentError::Manifest(ManifestError::MissingField("chunk.crc"))
            })?,
            offset: c.offset.take().ok_or_else(|| {
                SteamVentError::Manifest(ManifestError::MissingField("chunk.offset"))
            })?,
            uncompressed_size: c.cb_original.take().ok_or_else(|| {
                SteamVentError::Manifest(ManifestError::MissingField("chunk.cb_original"))
            })?,
            compressed_size: c.cb_compressed.take().ok_or_else(|| {
                SteamVentError::Manifest(ManifestError::MissingField("chunk.cb_compressed"))
            })?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ManifestEntry {
    pub filename: String,
    pub chunks: Vec<ChunkInfo>,
    pub flags: Option<u32>,
    pub size: Option<u64>,
    pub depot: Arc<DepotContext>,
}

const FLAG_DIRECTORY: u32 = 0x40;

impl ManifestEntry {
    pub fn is_directory(&self) -> bool {
        match self.flags {
            Some(flags) => flags & FLAG_DIRECTORY != 0,
            None => {
                tracing::warn!(
                    "flags field is missing for entry '{}', treating as file",
                    self.filename
                );
                false
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Manifest {
    pub directories: Vec<ManifestEntry>,
    pub files: Vec<ManifestEntry>,
}

impl Manifest {
    pub fn classify(entries: Vec<ManifestEntry>) -> Self {
        let mut directories = Vec::new();
        let mut files = Vec::new();

        for entry in entries {
            if entry.is_directory() {
                directories.push(entry);
            } else {
                files.push(entry);
            }
        }

        tracing::info!(
            "Found {} directories and {} files to download.",
            directories.len(),
            files.len()
        );

        Self { directories, files }
    }
}
