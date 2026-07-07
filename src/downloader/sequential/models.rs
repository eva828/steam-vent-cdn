use crate::downloader::cdn::CdnFetcher;
use crate::downloader::progress::ProgressCallback;
use crate::error::Result;
use futures::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

#[derive(Debug)]
pub(crate) struct RawChunk {
    pub chunk_offset: u64,
    pub compressed_data: Vec<u8>,
    pub uncompressed_size: u32,
    pub crc: u32,
    pub depot_key: Option<Vec<u8>>,
}

#[derive(Debug)]
pub(crate) struct DecodedChunk {
    pub chunk_offset: u64,
    pub data: Vec<u8>,
}

pub struct FileByteStream {
    pub filename: String,
    pub size: Option<u64>,
    rx: tokio::sync::mpsc::Receiver<Result<Vec<u8>>>,
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

#[derive(Clone)]
pub struct DownloadPipeline {
    pub(crate) http_fetcher: CdnFetcher,
    pub(crate) download_sem: Arc<tokio::sync::Semaphore>,
    pub(crate) progress: Arc<dyn ProgressCallback>,
    pub(crate) parallel_download: usize,
}
