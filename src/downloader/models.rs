use std::path::Path;
use std::sync::Arc;

use crate::error::{ManifestError, Result, SteamVentError};
use steam_vent_proto::content_manifest::ContentManifestPayload;

pub trait ProgressCallback: Send + Sync {
    fn on_download_progress(&self, bytes: u64);
    fn on_decode_progress(&self, bytes: u64);
    fn on_write_progress(&self, bytes: u64);
    fn on_file_complete(&self, path: &Path, bytes: usize);
}

#[derive(Debug, Clone)]
pub struct ChunkDownloadTask {
    pub chunk_id: String,
    pub depot: Arc<DepotContext>,
    pub file_index: usize,
    pub chunk_index: usize,
    pub compressed_size: u32,
    pub uncompressed_size: u32,
    pub crc: u32,
}

#[derive(Debug, Clone)]
pub struct CompressedChunk {
    pub depot: Arc<DepotContext>,
    pub file_index: usize,
    pub chunk_index: usize,
    pub compressed_data: Vec<u8>,
    pub uncompressed_size: u32,
    pub crc: u32,
}

#[derive(Debug, Clone)]
pub struct DownloadedChunk {
    pub file_index: usize,
    pub chunk_index: usize,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct DepotContext {
    pub server_hosts: Vec<String>,
    pub depot_id: u32,
    pub depot_key: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub(crate) struct DownloadManifest {
    pub manifest: ContentManifestPayload,
    pub depot: Arc<DepotContext>,
}

impl DownloadManifest {
    pub(crate) fn into_download_items(self) -> Result<Vec<DownloadItem>> {
        let mut items = Vec::new();

        for mut mapping in self.manifest.mappings {
            let filename = mapping.filename.take().ok_or_else(|| {
                SteamVentError::Manifest(ManifestError::Custom(
                    "filename is missing in manifest mapping".to_string(),
                ))
            })?;
            let chunks: Vec<ChunkInfo> = mapping
                .chunks
                .into_iter()
                .map(|mut c| -> Result<ChunkInfo> {
                    let sha = c.sha.take().ok_or_else(|| {
                        SteamVentError::Manifest(ManifestError::Custom(
                            "Chunk SHA is missing".to_string(),
                        ))
                    })?;

                    let crc = c.crc.take().ok_or_else(|| {
                        SteamVentError::Manifest(ManifestError::Custom(
                            "Chunk CRC is missing".to_string(),
                        ))
                    })?;

                    let uncompressed_size = c.cb_original.take().ok_or_else(|| {
                        SteamVentError::Manifest(ManifestError::Custom(
                            "Chunk cb_original is missing".to_string(),
                        ))
                    })?;

                    let compressed_size = c.cb_compressed.take().ok_or_else(|| {
                        SteamVentError::Manifest(ManifestError::Custom(
                            "Chunk cb_compressed is missing".to_string(),
                        ))
                    })?;

                    Ok(ChunkInfo {
                        sha,
                        crc,
                        uncompressed_size,
                        compressed_size,
                    })
                })
                .collect::<Result<Vec<_>>>()?;

            items.push(DownloadItem {
                filename,
                chunks,
                flags: mapping.flags.take(),
                size: mapping.size.take(),
                depot: Arc::clone(&self.depot),
            });
        }

        Ok(items)
    }
}

#[derive(Debug, Clone)]
pub struct ChunkInfo {
    pub sha: Vec<u8>,
    pub crc: u32,
    pub uncompressed_size: u32,
    pub compressed_size: u32,
}

#[derive(Debug, Clone)]
pub struct DownloadItem {
    pub filename: String,
    pub chunks: Vec<ChunkInfo>,
    pub flags: Option<u32>,
    pub size: Option<u64>,
    pub depot: Arc<DepotContext>,
}

#[derive(Debug, Clone)]
pub struct VersionInfo {
    pub timestamp: u32,
    pub manifest_id: u64,
    pub description: String,
}
