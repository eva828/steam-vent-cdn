use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::error::{ManifestError, Result, SteamVentError};
use steam_vent_proto::content_manifest::ContentManifestPayload;

pub trait ProgressCallback: Send + Sync {
    fn on_download_progress(&self, bytes: u64);
    fn on_decode_progress(&self, bytes: u64);
    fn on_write_progress(&self, bytes: u64);
}

#[derive(Debug, Clone)]
pub struct ChunkDownloadTask {
    pub chunk_id: String,
    pub depot: Arc<DepotContext>,
    pub file_index: usize,
    pub chunk_index: usize,
    pub chunk_offset: u64,
    pub compressed_size: u32,
    pub uncompressed_size: u32,
    pub crc: u32,
}

#[derive(Debug, Clone)]
pub struct CompressedChunk {
    pub depot: Arc<DepotContext>,
    pub file_index: usize,
    pub chunk_index: usize,
    pub chunk_offset: u64,
    pub compressed_data: Vec<u8>,
    pub uncompressed_size: u32,
    pub crc: u32,
}

#[derive(Debug, Clone)]
pub struct DownloadedChunk {
    pub file_index: usize,
    pub chunk_index: usize,
    pub chunk_offset: u64,
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
                SteamVentError::Manifest(ManifestError::MissingField("mapping.filename"))
            })?;
            // Normalize Windows-style path separators to Unix-style.
            // Steam manifests may contain backslashes from Windows paths,
            // which would be treated as literal filename characters on Linux.
            let filename = filename.replace('\\', "/");
            let chunks: Vec<ChunkInfo> = mapping
                .chunks
                .into_iter()
                .map(|mut c| -> Result<ChunkInfo> {
                    let sha = c.sha.take().ok_or_else(|| {
                        SteamVentError::Manifest(ManifestError::MissingField("chunk.sha"))
                    })?;

                    let crc = c.crc.take().ok_or_else(|| {
                        SteamVentError::Manifest(ManifestError::MissingField("chunk.crc"))
                    })?;

                    let offset = c.offset.take().ok_or_else(|| {
                        SteamVentError::Manifest(ManifestError::MissingField("chunk.offset"))
                    })?;

                    let uncompressed_size = c.cb_original.take().ok_or_else(|| {
                        SteamVentError::Manifest(ManifestError::MissingField("chunk.cb_original"))
                    })?;

                    let compressed_size = c.cb_compressed.take().ok_or_else(|| {
                        SteamVentError::Manifest(ManifestError::MissingField("chunk.cb_compressed"))
                    })?;

                    Ok(ChunkInfo {
                        sha,
                        crc,
                        offset,
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
    pub offset: u64,
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

/// A chunk that has been downloaded, decoded, and reassembled in the correct
/// order within its file. Consumers receive these in sequential offset order
/// per file, so they can be written (or streamed) directly without buffering.
#[derive(Debug)]
pub struct ReassembledChunk {
    /// Index into the original `DownloadItem` slice.
    pub file_index: usize,
    /// Byte offset within the file.
    pub offset: u64,
    /// Decoded (decrypted + decompressed) data for this chunk.
    pub data: Vec<u8>,
    /// `true` when this is the last chunk of the file.
    pub is_last: bool,
}

/// A stream of [`ReassembledChunk`]s produced by [`CdnDownloader::download`].
///
/// Implements [`futures::Stream`] so it can be used with standard stream
/// combinators (`next()`, `map()`, `for_each()`, etc.).
///
/// Chunks arrive in sequential offset order per file. The consumer is
/// responsible for routing each chunk to the appropriate output (file, network
/// stream, etc.).
pub struct DownloadStream {
    rx: tokio::sync::mpsc::Receiver<Result<ReassembledChunk>>,
    /// Worker handles — kept alive so they are aborted on drop.
    _handles: tokio::task::JoinSet<()>,
}

impl DownloadStream {
    pub(crate) fn new(
        rx: tokio::sync::mpsc::Receiver<Result<ReassembledChunk>>,
        handles: tokio::task::JoinSet<()>,
    ) -> Self {
        Self {
            rx,
            _handles: handles,
        }
    }
}

impl futures::Stream for DownloadStream {
    type Item = Result<ReassembledChunk>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}
