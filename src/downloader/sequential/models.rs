use crate::downloader::cdn::CdnFetcher;
use crate::downloader::progress::ProgressCallback;
use crate::error::{Result, SteamVentError};
use futures::Stream;
use std::any::Any;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

pub(crate) fn panic_msg(payload: Box<dyn Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(msg) => *msg,
        Err(payload) => match payload.downcast::<&str>() {
            Ok(msg) => msg.to_string(),
            Err(_) => "unknown panic".to_string(),
        },
    }
}

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
    handles: tokio::task::JoinSet<()>,
    cancel_tx: tokio::sync::watch::Sender<bool>,
}

impl FileByteStream {
    pub(crate) fn new(
        filename: String,
        size: Option<u64>,
        rx: tokio::sync::mpsc::Receiver<Result<Vec<u8>>>,
        handles: tokio::task::JoinSet<()>,
        cancel_tx: tokio::sync::watch::Sender<bool>,
    ) -> Self {
        Self {
            filename,
            size,
            rx,
            handles,
            cancel_tx,
        }
    }
}

impl Unpin for FileByteStream {}

impl Stream for FileByteStream {
    type Item = Result<Vec<u8>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.handles.poll_join_next(cx) {
                Poll::Ready(Some(Err(join_err))) => {
                    let msg = if join_err.is_panic() {
                        panic_msg(join_err.into_panic())
                    } else {
                        "background task was cancelled".to_string()
                    };
                    self.cancel_tx.send(true).ok();
                    return Poll::Ready(Some(Err(SteamVentError::InternalPanic(msg))));
                }
                Poll::Ready(Some(Ok(()))) => continue,
                Poll::Ready(None) | Poll::Pending => break,
            }
        }
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
