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
pub(crate) struct ReassembledChunk {
    /// Index into the original `DownloadItem` slice.
    pub file_index: usize,
    /// Decoded (decrypted + decompressed) data for this chunk.
    pub data: Vec<u8>,
    /// `true` when this is the last chunk of the file.
    pub is_last: bool,
}

/// A single file from a download, providing metadata and a byte stream.
///
/// Implements [`futures::Stream`] yielding `Result<Vec<u8>>` chunks in
/// sequential order.  Consuming the stream gives the complete file contents.
pub struct FileEntry {
    /// The relative filename (may contain path separators for subdirectories).
    pub filename: String,
    /// The expected total size in bytes, if known from the manifest.
    pub size: Option<u64>,
    rx: tokio::sync::mpsc::Receiver<Result<Vec<u8>>>,
}

impl FileEntry {
    pub(crate) fn new(
        filename: String,
        size: Option<u64>,
        rx: tokio::sync::mpsc::Receiver<Result<Vec<u8>>>,
    ) -> Self {
        Self { filename, size, rx }
    }
}

impl Unpin for FileEntry {}

impl futures::Stream for FileEntry {
    type Item = Result<Vec<u8>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

/// A stream of [`FileEntry`]s produced by [`CdnDownloader::download`].
///
/// Implements [`futures::Stream`] yielding one [`FileEntry`] per file being
/// downloaded.  Each `FileEntry` is itself a byte stream.
///
/// **Important:** `FileEntry`s should be consumed concurrently (e.g. by
/// spawning a task per entry) because chunks from different files arrive
/// interleaved from the CDN.
pub struct DownloadStream {
    rx: tokio::sync::mpsc::Receiver<Result<FileEntry>>,
    /// Worker handles — kept alive so they are aborted on drop.
    _handles: tokio::task::JoinSet<()>,
}

impl DownloadStream {
    pub(crate) fn new(
        rx: tokio::sync::mpsc::Receiver<Result<FileEntry>>,
        handles: tokio::task::JoinSet<()>,
    ) -> Self {
        Self {
            rx,
            _handles: handles,
        }
    }
}

impl Unpin for DownloadStream {}

impl futures::Stream for DownloadStream {
    type Item = Result<FileEntry>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}
