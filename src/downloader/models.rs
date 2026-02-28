use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::error::{ManifestError, Result, SteamVentError};
use steam_vent_proto::content_manifest::ContentManifestPayload;

use super::http::HttpFetcher;

pub trait ProgressCallback: Send + Sync {
    fn on_download_progress(&self, bytes: u64);
    fn on_decode_progress(&self, bytes: u64);
    fn on_write_progress(&self, bytes: u64);
}

// ---------------------------------------------------------------------------
// Internal pipeline types (single-file oriented, no file_index needed)
// ---------------------------------------------------------------------------

/// A chunk that has been downloaded from CDN but not yet decrypted/decompressed.
#[derive(Debug)]
pub(crate) struct RawChunk {
    /// Byte offset within the file.
    pub chunk_offset: u64,
    /// Encrypted + compressed data from CDN.
    pub compressed_data: Vec<u8>,
    /// Expected uncompressed size (for verification).
    pub uncompressed_size: u32,
    /// Adler32 CRC (for verification).
    pub crc: u32,
    /// Depot decryption key, cloned from `DepotContext` so the chunk is
    /// self-contained and doesn't carry an `Arc`.
    pub depot_key: Option<Vec<u8>>,
}

/// A chunk that has been decrypted and decompressed, ready for reassembly.
#[derive(Debug)]
pub(crate) struct DecodedChunk {
    /// Byte offset within the file.
    pub chunk_offset: u64,
    /// Fully decoded data.
    pub data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Depot / manifest types (unchanged)
// ---------------------------------------------------------------------------

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
                        SteamVentError::Manifest(ManifestError::MissingField(
                            "chunk.cb_compressed",
                        ))
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

// ---------------------------------------------------------------------------
// Public streaming / pipeline types
// ---------------------------------------------------------------------------

/// A byte stream for a single file produced by
/// [`DownloadPipeline::download_file`].
///
/// Implements [`futures::Stream`] yielding `Result<Vec<u8>>` chunks in
/// sequential offset order.  Consuming the stream gives the complete file
/// contents.
///
/// Dropping this value aborts the background worker tasks that feed the
/// stream.
pub struct FileByteStream {
    /// The relative filename (may contain path separators for subdirectories).
    pub filename: String,
    /// The expected total size in bytes, if known from the manifest.
    pub size: Option<u64>,
    rx: tokio::sync::mpsc::Receiver<Result<Vec<u8>>>,
    /// Worker handles — kept alive so they are aborted on drop.
    _handles: tokio::task::JoinSet<()>,
}

impl FileByteStream {
    pub(crate) fn new(
        filename: String,
        size: Option<u64>,
        rx: tokio::sync::mpsc::Receiver<Result<Vec<u8>>>,
        handles: tokio::task::JoinSet<()>,
    ) -> Self {
        Self {
            filename,
            size,
            rx,
            _handles: handles,
        }
    }
}

impl Unpin for FileByteStream {}

impl futures::Stream for FileByteStream {
    type Item = Result<Vec<u8>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

/// Shared download pipeline that can download individual files.
///
/// Created via [`CdnDownloader::create_pipeline`].  All clones share the
/// same download semaphore, so the total number of concurrent HTTP chunk
/// downloads is bounded by `parallel_download` regardless of how many
/// `download_file` calls are in flight.
///
/// This type is [`Clone`] and [`Send`] + [`Sync`], so it can be shared
/// across tasks freely.
#[derive(Clone)]
pub struct DownloadPipeline {
    pub(crate) http_fetcher: HttpFetcher,
    pub(crate) download_sem: Arc<tokio::sync::Semaphore>,
    pub(crate) progress: Arc<dyn ProgressCallback>,
    pub(crate) parallel_download: usize,
}
