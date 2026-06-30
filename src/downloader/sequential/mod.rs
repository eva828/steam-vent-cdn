pub mod models;
pub mod progress;
pub(crate) mod workers;

pub use models::{DownloadPipeline, FileByteStream};
pub use progress::ProgressCallback;

use crate::downloader::item::DownloadItem;
use crate::error::Result;
use models::{DecodedChunk, RawChunk};
use std::sync::Arc;
use workers::{run_decode_loop, run_reassembly_loop, spawn_chunk_downloads};

impl DownloadPipeline {
    /// Download a single file and return a [`FileByteStream`] providing its
    /// bytes in sequential order.
    ///
    /// Internally spawns a 3-stage pipeline (download → decode → reassemble)
    /// dedicated to this file.  The returned stream owns all worker tasks;
    /// dropping it aborts them.
    ///
    /// Multiple `download_file` calls share the same download semaphore, so
    /// the total concurrent HTTP chunk downloads stays bounded.
    pub fn download_file(&self, item: &DownloadItem) -> Result<FileByteStream> {
        let parallel_download = self.parallel_download;

        let download_channel_size = (parallel_download * 2).max(64);
        let logical_cores = match std::thread::available_parallelism() {
            Ok(n) => n.get(),
            Err(e) => {
                tracing::warn!(
                    "Failed to detect CPU parallelism ({}), falling back to 8 cores",
                    e
                );
                8
            }
        };
        let decode_channel_size = (logical_cores * 2).max(32);
        let output_channel_size = decode_channel_size;

        // Stage 1 → 2: raw (encrypted+compressed) chunks
        let (download_tx, download_rx) =
            tokio::sync::mpsc::channel::<Result<RawChunk>>(download_channel_size);
        // Stage 2 → 3: decoded (decrypted+decompressed) chunks
        let (decoded_tx, decoded_rx) =
            tokio::sync::mpsc::channel::<Result<DecodedChunk>>(decode_channel_size);
        // Stage 3 → consumer: reassembled bytes in offset order
        let (output_tx, output_rx) =
            tokio::sync::mpsc::channel::<Result<Vec<u8>>>(output_channel_size);

        let mut handles = tokio::task::JoinSet::new();

        // Stage 1: spawn chunk downloads (returns immediately)
        spawn_chunk_downloads(
            &self.http_fetcher,
            &self.download_sem,
            &self.progress,
            item,
            download_tx,
        );

        // Stage 2: decode worker
        let progress_decode = Arc::clone(&self.progress);
        handles.spawn(async move {
            run_decode_loop(download_rx, decoded_tx, progress_decode).await;
        });

        // Stage 3: reassembly worker
        let progress_reassembly = Arc::clone(&self.progress);
        let filename = item.filename.clone();
        let total_chunks = item.chunks.len();
        handles.spawn(async move {
            run_reassembly_loop(
                decoded_rx,
                output_tx,
                progress_reassembly,
                filename,
                total_chunks,
            )
            .await;
        });

        Ok(FileByteStream::new(
            item.filename.clone(),
            item.size,
            output_rx,
            handles,
        ))
    }
}
