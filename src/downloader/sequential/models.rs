use crate::downloader::cdn::HttpFetcher;
use crate::error::Result;
use futures::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

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

impl Stream for FileByteStream {
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
    pub(crate) progress: Arc<dyn crate::downloader::ProgressCallback>,
    pub(crate) parallel_download: usize,
}
