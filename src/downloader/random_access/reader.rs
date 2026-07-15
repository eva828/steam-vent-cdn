use std::collections::HashMap;
use std::io::SeekFrom;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};

use crate::downloader::cdn::DepotContext;
use crate::downloader::cdn::http::CdnFetcher;
use crate::downloader::manifest::{ChunkInfo, ManifestEntry};
use crate::downloader::progress::ProgressCallback;
use crate::downloader::random_access::fetch::{ChunkFetchFuture, build_fetch_future};
use crate::error::{DownloadError, Result};

pub struct CdnRandomAccessReader {
    pub(crate) chunks: Vec<ChunkInfo>,
    pub(crate) depot: Arc<DepotContext>,
    pub(crate) http_fetcher: CdnFetcher,
    pub(crate) total_size: u64,
    pub(crate) position: u64,
    pub(crate) cache: HashMap<usize, Vec<u8>>,
    pub(crate) pending_fetch: Option<ChunkFetchFuture>,
    pub(crate) progress: Arc<dyn ProgressCallback>,
}

impl CdnRandomAccessReader {
    pub(crate) fn new(
        entry: &ManifestEntry,
        http_fetcher: CdnFetcher,
        progress: Arc<dyn ProgressCallback>,
    ) -> Result<Self> {
        let mut chunks = entry.chunks.clone();
        chunks.sort_unstable_by_key(|c| c.offset);

        let total_size = entry.size.unwrap_or_else(|| {
            chunks
                .last()
                .map(|c| c.offset + c.uncompressed_size as u64)
                .unwrap_or(0)
        });

        Self::validate_chunk_coverage(&chunks, total_size)?;

        Ok(Self {
            chunks,
            depot: Arc::clone(&entry.depot),
            http_fetcher,
            total_size,
            position: 0,
            cache: HashMap::new(),
            pending_fetch: None,
            progress,
        })
    }

    fn validate_chunk_coverage(chunks: &[ChunkInfo], total_size: u64) -> Result<()> {
        let gap = chunks
            .first()
            .filter(|c| c.offset > 0)
            .map(|_| 0)
            .or_else(|| {
                chunks.windows(2).find_map(|pair| {
                    let prev_end = pair[0].offset + pair[0].uncompressed_size as u64;
                    (prev_end < pair[1].offset).then_some(prev_end)
                })
            })
            .or_else(|| {
                chunks.last().and_then(|c| {
                    let end = c.offset + c.uncompressed_size as u64;
                    (end < total_size).then_some(end)
                })
            });

        match gap {
            Some(offset) => Err(DownloadError::GapInChunks { offset }.into()),
            None => Ok(()),
        }
    }

    pub(crate) fn chunk_index_for_offset(&self, offset: u64) -> Option<usize> {
        let idx = self
            .chunks
            .partition_point(|c| c.offset <= offset)
            .checked_sub(1)?;

        let chunk = &self.chunks[idx];
        let chunk_end = chunk.offset + chunk.uncompressed_size as u64;
        if offset < chunk_end { Some(idx) } else { None }
    }
}

impl Unpin for CdnRandomAccessReader {}

impl AsyncRead for CdnRandomAccessReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            if self.position >= self.total_size {
                return Poll::Ready(Ok(()));
            }

            if let Some(ref mut fut) = self.pending_fetch {
                match Pin::as_mut(fut).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok((chunk_idx, data))) => {
                        self.cache.insert(chunk_idx, data);
                        self.pending_fetch = None;
                        continue;
                    }
                    Poll::Ready(Err(e)) => {
                        self.pending_fetch = None;
                        return Poll::Ready(Err(std::io::Error::other(e.to_string())));
                    }
                }
            }

            let chunk_idx = match self.chunk_index_for_offset(self.position) {
                Some(idx) => idx,
                None => {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("gap in chunks at offset {}", self.position),
                    )));
                }
            };

            if self.cache.contains_key(&chunk_idx) {
                let chunk = &self.chunks[chunk_idx];
                let offset_in_chunk = (self.position - chunk.offset) as usize;
                let chunk_data = &self.cache[&chunk_idx];
                let available = chunk_data.len() - offset_in_chunk;
                let to_copy = available.min(buf.remaining());
                buf.put_slice(&chunk_data[offset_in_chunk..offset_in_chunk + to_copy]);
                self.position += to_copy as u64;
                return Poll::Ready(Ok(()));
            }

            let chunk = &self.chunks[chunk_idx];
            let fut = build_fetch_future(
                chunk_idx,
                chunk,
                &self.depot,
                &self.http_fetcher,
                &self.progress,
            );
            self.pending_fetch = Some(fut);
        }
    }
}

impl AsyncSeek for CdnRandomAccessReader {
    fn start_seek(mut self: Pin<&mut Self>, pos: SeekFrom) -> std::io::Result<()> {
        let new_pos = match pos {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::End(offset) => self.total_size as i64 + offset,
            SeekFrom::Current(offset) => self.position as i64 + offset,
        };

        if new_pos < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek to a negative position",
            ));
        }

        self.position = new_pos as u64;
        self.pending_fetch = None;
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<u64>> {
        Poll::Ready(Ok(self.position))
    }
}
