use std::collections::BTreeMap;
use std::sync::Arc;

use crate::downloader::ProgressCallback;
use crate::downloader::archive::SteamChunk;
use crate::downloader::cdn::http::CdnFetcher;
use crate::downloader::cdn::servers::RoundRobin;
use crate::downloader::manifest::ManifestEntry;
use crate::downloader::sequential::models::{self, DecodedChunk, RawChunk};
use crate::error::{DownloadError, Result, SteamVentError};

pub(crate) fn spawn_chunk_downloads(
    http_fetcher: &CdnFetcher,
    download_sem: &Arc<tokio::sync::Semaphore>,
    progress: &Arc<dyn ProgressCallback>,
    entry: &ManifestEntry,
    download_tx: tokio::sync::mpsc::Sender<Result<RawChunk>>,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
) {
    let depot_id = entry.depot.depot_id;
    let depot_key = entry.depot.depot_key.clone();

    for chunk in entry.chunks.iter() {
        let download_sem = Arc::clone(download_sem);
        let download_tx = download_tx.clone();
        let http_fetcher = http_fetcher.clone();
        let progress = Arc::clone(progress);

        let chunk_id = hex::encode(&chunk.sha1);
        let chunk_offset = chunk.offset;
        let compressed_size = chunk.compressed_size;
        let uncompressed_size = chunk.uncompressed_size;
        let crc = chunk.crc;
        let depot_key = depot_key.clone();
        let depot = Arc::clone(&entry.depot);

        let mut cancel_rx = cancel_rx.clone();

        tokio::spawn(async move {
            let _permit = tokio::select! {
                result = download_sem.acquire() => match result {
                    Ok(permit) => permit,
                    Err(_) => {
                        let _ = download_tx
                            .send(Err(DownloadError::SemaphoreClosed.into()))
                            .await;
                        return;
                    }
                },
                _ = cancel_rx.changed() => return,
            };

            let server_list = RoundRobin::new(&depot.servers);
            let result = tokio::select! {
                r = http_fetcher.download_chunk(
                    &server_list, depot_id, &chunk_id,
                ) => r,
                _ = cancel_rx.changed() => return,
            };

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

pub(crate) async fn run_decrypt_decode_loop(
    mut download_rx: tokio::sync::mpsc::Receiver<Result<RawChunk>>,
    decoded_tx: tokio::sync::mpsc::Sender<Result<DecodedChunk>>,
    progress: Arc<dyn ProgressCallback>,
    cancel_tx: tokio::sync::watch::Sender<bool>,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        let result = tokio::select! {
            result = download_rx.recv() => result,
            _ = cancel_rx.changed() => return,
        };

        match result {
            Some(Ok(raw)) => {
                let decoded_tx = decoded_tx.clone();
                let progress = progress.clone();

                rayon::spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        SteamChunk::new(raw.compressed_data)
                            .decrypt(raw.depot_key.as_deref())
                            .and_then(|chunk| {
                                chunk.decompress_and_verify(raw.uncompressed_size, raw.crc)
                            })
                    }));

                    match result {
                        Ok(Ok(chunk)) => {
                            let data = chunk.into_vec();
                            progress.on_decode_progress(data.len() as u64);
                            if let Err(e) = decoded_tx.blocking_send(Ok(DecodedChunk {
                                chunk_offset: raw.chunk_offset,
                                data,
                            })) {
                                tracing::error!("Failed to send decoded chunk: {}", e);
                            }
                        }
                        Ok(Err(e)) => {
                            if let Err(send_err) = decoded_tx.blocking_send(Err(e)) {
                                tracing::error!("Failed to send decode error: {}", send_err);
                            }
                        }
                        Err(panic_payload) => {
                            let msg = models::panic_msg(panic_payload);
                            if let Err(send_err) =
                                decoded_tx.blocking_send(Err(SteamVentError::InternalPanic(msg)))
                            {
                                tracing::error!("Failed to send panic error: {}", send_err);
                            }
                        }
                    }
                });
            }
            Some(Err(e)) => {
                cancel_tx.send(true).ok();
                if let Err(send_err) = decoded_tx.send(Err(e)).await {
                    tracing::error!("Failed to send decode stream error: {}", send_err);
                }
                return;
            }
            None => break,
        }
    }
}

pub(crate) async fn run_reassembly_loop(
    mut decoded_rx: tokio::sync::mpsc::Receiver<Result<DecodedChunk>>,
    output_tx: tokio::sync::mpsc::Sender<Result<Vec<u8>>>,
    progress: Arc<dyn ProgressCallback>,
    filename: String,
    total_chunks: usize,
    cancel_tx: tokio::sync::watch::Sender<bool>,
) {
    let mut next_offset: u64 = 0;
    let mut pending: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let mut chunks_emitted: usize = 0;

    while let Some(result) = decoded_rx.recv().await {
        match result {
            Ok(chunk) => {
                if chunk.chunk_offset == next_offset {
                    let len = chunk.data.len() as u64;
                    next_offset += len;
                    chunks_emitted += 1;
                    progress.on_write_progress(len);
                    if output_tx.send(Ok(chunk.data)).await.is_err() {
                        tracing::error!("Output channel closed during reassembly");
                        return;
                    }

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
                    pending.insert(chunk.chunk_offset, chunk.data);
                }
            }
            Err(e) => {
                cancel_tx.send(true).ok();
                tracing::error!("Error in decode stream: {}", e);
                let _ = output_tx.send(Err(e)).await;
                return;
            }
        }
    }

    if chunks_emitted < total_chunks {
        let received = chunks_emitted + pending.len();
        tracing::warn!(
            "Incomplete file '{}': received {}/{} chunks",
            filename,
            received,
            total_chunks,
        );
        let _ = output_tx
            .send(Err(DownloadError::IncompleteDownload {
                filename,
                received,
                total: total_chunks,
            }
            .into()))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloader::ProgressCallback;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct CountingProgress {
        write_bytes: AtomicU64,
    }

    impl ProgressCallback for CountingProgress {
        fn on_download_progress(&self, _bytes: u64) {}
        fn on_decode_progress(&self, _bytes: u64) {}
        fn on_write_progress(&self, bytes: u64) {
            self.write_bytes.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    use crate::error::SteamVentError;

    fn chunk(offset: u64, data: Vec<u8>) -> DecodedChunk {
        DecodedChunk {
            chunk_offset: offset,
            data,
        }
    }

    #[tokio::test]
    async fn reassembles_chunks_in_order() {
        let (decoded_tx, decoded_rx) = tokio::sync::mpsc::channel(8);
        let (output_tx, mut output_rx) = tokio::sync::mpsc::channel(8);
        let (cancel_tx, _) = tokio::sync::watch::channel(false);
        let progress = Arc::new(CountingProgress {
            write_bytes: AtomicU64::new(0),
        });

        let handle = tokio::spawn(run_reassembly_loop(
            decoded_rx,
            output_tx,
            Arc::clone(&progress) as Arc<dyn ProgressCallback>,
            "test.dat".into(),
            2,
            cancel_tx,
        ));

        let _ = decoded_tx.send(Ok(chunk(0, b"hello".to_vec()))).await;
        let _ = decoded_tx.send(Ok(chunk(5, b" world".to_vec()))).await;
        drop(decoded_tx);

        let mut parts = Vec::new();
        while let Some(Ok(data)) = output_rx.recv().await {
            parts.push(data);
        }
        handle.await.unwrap();

        assert_eq!(parts, vec![b"hello".to_vec(), b" world".to_vec()]);
    }

    #[tokio::test]
    async fn reassembles_chunks_out_of_order() {
        let (decoded_tx, decoded_rx) = tokio::sync::mpsc::channel(8);
        let (output_tx, mut output_rx) = tokio::sync::mpsc::channel(8);
        let (cancel_tx, _) = tokio::sync::watch::channel(false);
        let progress = Arc::new(CountingProgress {
            write_bytes: AtomicU64::new(0),
        });

        let handle = tokio::spawn(run_reassembly_loop(
            decoded_rx,
            output_tx,
            Arc::clone(&progress) as Arc<dyn ProgressCallback>,
            "reordered.dat".into(),
            3,
            cancel_tx,
        ));

        // Send chunks out of order: data at offsets 10, 6, 0
        // Each chunk: "first " (6 bytes), "mid " (4 bytes), "last" (4 bytes)
        // Contiguous range: [0, 14)
        let _ = decoded_tx.send(Ok(chunk(10, b"last".to_vec()))).await;
        let _ = decoded_tx.send(Ok(chunk(6, b"mid ".to_vec()))).await;
        let _ = decoded_tx.send(Ok(chunk(0, b"first ".to_vec()))).await;
        drop(decoded_tx);

        let mut parts = Vec::new();
        while let Some(Ok(data)) = output_rx.recv().await {
            parts.push(data);
        }
        handle.await.unwrap();

        let concat: Vec<u8> = parts.iter().flat_map(|p| p.iter().copied()).collect();
        assert_eq!(String::from_utf8(concat).unwrap(), "first mid last");
    }

    #[tokio::test]
    async fn error_propagates_and_cancels() {
        let (decoded_tx, decoded_rx) = tokio::sync::mpsc::channel(8);
        let (output_tx, mut output_rx) = tokio::sync::mpsc::channel(8);
        let (cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);
        let progress = Arc::new(CountingProgress {
            write_bytes: AtomicU64::new(0),
        });

        let handle = tokio::spawn(run_reassembly_loop(
            decoded_rx,
            output_tx,
            Arc::clone(&progress) as Arc<dyn ProgressCallback>,
            "err.dat".into(),
            3,
            cancel_tx,
        ));

        let _ = decoded_tx
            .send(Err(SteamVentError::Chunk(
                crate::error::ChunkError::ChecksumMismatch {
                    expected: 100,
                    actual: 200,
                },
            )))
            .await;
        drop(decoded_tx);

        // Cancel signal should be sent
        assert!(cancel_rx.changed().await.is_ok());

        let err = output_rx.recv().await.unwrap().unwrap_err();
        assert!(matches!(
            err,
            SteamVentError::Chunk(crate::error::ChunkError::ChecksumMismatch { .. })
        ));

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn incomplete_download_reports_error() {
        let (decoded_tx, decoded_rx) = tokio::sync::mpsc::channel(8);
        let (output_tx, mut output_rx) = tokio::sync::mpsc::channel(8);
        let (cancel_tx, _) = tokio::sync::watch::channel(false);
        let progress = Arc::new(CountingProgress {
            write_bytes: AtomicU64::new(0),
        });

        let handle = tokio::spawn(run_reassembly_loop(
            decoded_rx,
            output_tx,
            Arc::clone(&progress) as Arc<dyn ProgressCallback>,
            "incomplete.dat".into(),
            5,
            cancel_tx,
        ));

        // Only send 2 of 5 expected chunks
        let _ = decoded_tx.send(Ok(chunk(0, b"aa".to_vec()))).await;
        let _ = decoded_tx.send(Ok(chunk(2, b"bb".to_vec()))).await;
        drop(decoded_tx); // channel closes early

        let mut received_ok = 0;
        while let Some(result) = output_rx.recv().await {
            match result {
                Ok(_) => received_ok += 1,
                Err(e) => {
                    assert!(matches!(
                        e,
                        SteamVentError::Download(DownloadError::IncompleteDownload { .. })
                    ));
                }
            }
        }
        handle.await.unwrap();

        assert_eq!(received_ok, 2);
    }
}
