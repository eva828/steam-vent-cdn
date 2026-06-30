use std::collections::BTreeMap;
use std::sync::Arc;

use crate::downloader::ProgressCallback;
use crate::downloader::archive::SteamChunk;
use crate::downloader::cdn::http::HttpFetcher;
use crate::downloader::cdn::servers::RoundRobin;
use crate::downloader::item::DownloadItem;
use crate::downloader::sequential::models::{DecodedChunk, RawChunk};
use crate::error::{Result, SteamVentError};

/// Spawn one tokio task per chunk to download it from CDN.
///
/// All spawned tasks share `download_sem` to limit the total number of
/// concurrent HTTP requests.  The function returns immediately after
/// spawning; the last task to finish will drop the final clone of
/// `download_tx`, closing the channel.
pub(crate) fn spawn_chunk_downloads(
    http_fetcher: &HttpFetcher,
    download_sem: &Arc<tokio::sync::Semaphore>,
    progress: &Arc<dyn ProgressCallback>,
    item: &DownloadItem,
    download_tx: tokio::sync::mpsc::Sender<Result<RawChunk>>,
) {
    let depot_id = item.depot.depot_id;
    let depot_key = item.depot.depot_key.clone();

    for chunk in item.chunks.iter() {
        let download_sem = Arc::clone(download_sem);
        let download_tx = download_tx.clone();
        let http_fetcher = http_fetcher.clone();
        let progress = Arc::clone(progress);

        let chunk_id = hex::encode(&chunk.sha);
        let chunk_offset = chunk.offset;
        let compressed_size = chunk.compressed_size;
        let uncompressed_size = chunk.uncompressed_size;
        let crc = chunk.crc;
        let depot_key = depot_key.clone();
        let depot = Arc::clone(&item.depot);

        tokio::spawn(async move {
            let _permit = match download_sem.acquire().await {
                Ok(permit) => permit,
                Err(_) => {
                    let _ = download_tx
                        .send(Err(SteamVentError::Cdn(
                            crate::error::CdnError::SemaphoreClosed,
                        )))
                        .await;
                    return;
                }
            };

            let result = http_fetcher
                .download_chunk(&RoundRobin::new(&depot.server_hosts), depot_id, &chunk_id)
                .await;

            match result {
                Ok(data) => {
                    progress.on_download_progress(compressed_size as u64);
                    let _ = download_tx
                        .send(Ok(RawChunk {
                            chunk_offset,
                            compressed_data: data,
                            uncompressed_size,
                            crc,
                            depot_key,
                        }))
                        .await;
                }
                Err(e) => {
                    let _ = download_tx.send(Err(e)).await;
                }
            }
        });
    }
}

/// Receive downloaded [`RawChunk`]s, decrypt + decompress them on the rayon
/// thread pool, and forward the results as [`DecodedChunk`]s.
pub(crate) async fn run_decode_loop(
    mut download_rx: tokio::sync::mpsc::Receiver<Result<RawChunk>>,
    decoded_tx: tokio::sync::mpsc::Sender<Result<DecodedChunk>>,
    progress: Arc<dyn ProgressCallback>,
) {
    while let Some(result) = download_rx.recv().await {
        match result {
            Ok(raw) => {
                let decoded_tx = decoded_tx.clone();
                let progress = progress.clone();

                rayon::spawn(move || {
                    let result = SteamChunk::new(raw.compressed_data)
                        .decrypt(raw.depot_key.as_deref())
                        .and_then(|chunk| {
                            chunk.decompress_and_verify(raw.uncompressed_size, raw.crc)
                        });

                    match result {
                        Ok(chunk) => {
                            let data = chunk.into_vec();
                            progress.on_decode_progress(data.len() as u64);
                            if let Err(e) = decoded_tx.blocking_send(Ok(DecodedChunk {
                                chunk_offset: raw.chunk_offset,
                                data,
                            })) {
                                tracing::error!("Failed to send decoded chunk: {}", e);
                            }
                        }
                        Err(e) => {
                            if let Err(send_err) = decoded_tx.blocking_send(Err(e)) {
                                tracing::error!("Failed to send decode error: {}", send_err);
                            }
                        }
                    }
                });
            }
            Err(e) => {
                if let Err(send_err) = decoded_tx.send(Err(e)).await {
                    tracing::error!("Failed to send decode stream error: {}", send_err);
                }
            }
        }
    }
}

/// Receive decoded (but potentially out-of-order) [`DecodedChunk`]s and
/// re-emit their data in sequential offset order as `Vec<u8>` directly to
/// the output channel.
///
/// Uses a [`BTreeMap`] to buffer out-of-order chunks and flushes them as
/// soon as they become contiguous.
pub(crate) async fn run_reassembly_loop(
    mut decoded_rx: tokio::sync::mpsc::Receiver<Result<DecodedChunk>>,
    output_tx: tokio::sync::mpsc::Sender<Result<Vec<u8>>>,
    progress: Arc<dyn ProgressCallback>,
    filename: String,
    total_chunks: usize,
) {
    let mut next_offset: u64 = 0;
    let mut pending: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let mut chunks_emitted: usize = 0;

    while let Some(result) = decoded_rx.recv().await {
        match result {
            Ok(chunk) => {
                if chunk.chunk_offset == next_offset {
                    // This chunk is the next expected one — emit immediately.
                    let len = chunk.data.len() as u64;
                    next_offset += len;
                    chunks_emitted += 1;
                    progress.on_write_progress(len);
                    if output_tx.send(Ok(chunk.data)).await.is_err() {
                        tracing::error!("Output channel closed during reassembly");
                        return;
                    }

                    // Flush any subsequent pending chunks that are now contiguous.
                    while let Some(entry) = pending.first_entry() {
                        if *entry.key() != next_offset {
                            break;
                        }
                        let data = entry.remove();
                        let len = data.len() as u64;
                        next_offset += len;
                        chunks_emitted += 1;
                        progress.on_write_progress(len);
                        if output_tx.send(Ok(data)).await.is_err() {
                            tracing::error!("Output channel closed during reassembly flush");
                            return;
                        }
                    }
                } else {
                    // Out-of-order chunk — buffer it.
                    pending.insert(chunk.chunk_offset, chunk.data);
                }
            }
            Err(e) => {
                tracing::error!("Error in decode stream: {}", e);
                let _ = output_tx.send(Err(e)).await;
                return;
            }
        }
    }

    // Channel closed — check for incomplete file.
    if chunks_emitted < total_chunks {
        let received = chunks_emitted + pending.len();
        tracing::warn!(
            "Incomplete file '{}': received {}/{} chunks",
            filename,
            received,
            total_chunks,
        );
        let _ = output_tx
            .send(Err(SteamVentError::Cdn(
                crate::error::CdnError::IncompleteDownload {
                    filename,
                    received,
                    total: total_chunks,
                },
            )))
            .await;
    }
}
