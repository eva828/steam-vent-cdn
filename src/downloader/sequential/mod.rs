pub mod models;
pub(crate) mod workers;

pub use models::{DownloadPipeline, FileByteStream};

use crate::downloader::manifest::ManifestEntry;
use crate::error::Result;
use models::{DecodedChunk, RawChunk};
use std::sync::Arc;
use workers::{run_decode_loop, run_reassembly_loop, spawn_chunk_downloads};

impl DownloadPipeline {
    pub fn download_file(&self, entry: &ManifestEntry) -> Result<FileByteStream> {
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

        let (download_tx, download_rx) =
            tokio::sync::mpsc::channel::<Result<RawChunk>>(download_channel_size);
        let (decoded_tx, decoded_rx) =
            tokio::sync::mpsc::channel::<Result<DecodedChunk>>(decode_channel_size);
        let (output_tx, output_rx) =
            tokio::sync::mpsc::channel::<Result<Vec<u8>>>(output_channel_size);

        let mut handles = tokio::task::JoinSet::new();

        spawn_chunk_downloads(
            &self.http_fetcher,
            &self.download_sem,
            &self.progress,
            entry,
            download_tx,
        );

        let progress_decode = Arc::clone(&self.progress);
        handles.spawn(async move {
            run_decode_loop(download_rx, decoded_tx, progress_decode).await;
        });

        let progress_reassembly = Arc::clone(&self.progress);
        let filename = entry.filename.clone();
        let total_chunks = entry.chunks.len();
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
            entry.filename.clone(),
            entry.size,
            output_rx,
            handles,
        ))
    }
}
