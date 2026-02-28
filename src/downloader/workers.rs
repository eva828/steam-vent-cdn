use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::AsyncWriteExt;

use crate::archive::SteamChunk;
use crate::error::{Result, SteamVentError};

use super::http::HttpFetcher;

use super::models::{
    ChunkDownloadTask, CompressedChunk, DownloadItem, DownloadedChunk, ProgressCallback,
};

pub struct DownloadWorker {
    http_fetcher: HttpFetcher,
    download_sem: Arc<tokio::sync::Semaphore>,
    progress: Arc<dyn ProgressCallback>,
    download_tx: tokio::sync::mpsc::Sender<Result<CompressedChunk>>,
}

impl DownloadWorker {
    pub fn new(
        http_fetcher: HttpFetcher,
        parallel_download: usize,
        progress: Arc<dyn ProgressCallback>,
        download_tx: tokio::sync::mpsc::Sender<Result<CompressedChunk>>,
    ) -> Self {
        Self {
            http_fetcher,
            download_sem: Arc::new(tokio::sync::Semaphore::new(parallel_download)),
            progress,
            download_tx,
        }
    }

    pub async fn start(self, tasks: Vec<ChunkDownloadTask>) {
        let download_tx = self.download_tx;

        for (task_index, task) in tasks.into_iter().enumerate() {
            let download_sem = self.download_sem.clone();
            let download_tx = download_tx.clone();
            let http_fetcher = self.http_fetcher.clone();
            let progress = self.progress.clone();

            tokio::spawn(async move {
                let _permit = match download_sem.acquire().await {
                    Ok(permit) => permit,
                    Err(_) => {
                        // Semaphore closed - send error to channel
                        let _ = download_tx
                            .send(Err(SteamVentError::Cdn(
                                crate::error::CdnError::SemaphoreClosed,
                            )))
                            .await;
                        return;
                    }
                };
                let start_server = task_index % task.depot.server_hosts.len().max(1);
                let compressed_data = http_fetcher
                    .download_chunk(
                        &task.depot.server_hosts,
                        task.depot.depot_id,
                        &task.chunk_id,
                        start_server,
                    )
                    .await;

                match compressed_data {
                    Ok(data) => {
                        progress.on_download_progress(task.compressed_size as u64);
                        // Ignore send error (receiver dropped is normal shutdown)
                        let _ = download_tx
                            .send(Ok(CompressedChunk {
                                depot: task.depot,
                                file_index: task.file_index,
                                chunk_index: task.chunk_index,
                                chunk_offset: task.chunk_offset,
                                compressed_data: data,
                                uncompressed_size: task.uncompressed_size,
                                crc: task.crc,
                            }))
                            .await;
                    }
                    Err(e) => {
                        // Ignore send error (receiver dropped is normal shutdown)
                        let _ = download_tx.send(Err(e)).await;
                    }
                }
            });
        }
    }
}

pub struct DecodeWorker {
    decode_tx: tokio::sync::mpsc::Sender<Result<DownloadedChunk>>,
    progress: Arc<dyn ProgressCallback>,
}

impl DecodeWorker {
    pub fn new(
        decode_tx: tokio::sync::mpsc::Sender<Result<DownloadedChunk>>,
        progress: Arc<dyn ProgressCallback>,
    ) -> Self {
        Self {
            decode_tx,
            progress,
        }
    }

    pub async fn start(
        self,
        mut download_rx: tokio::sync::mpsc::Receiver<Result<CompressedChunk>>,
    ) {
        let decode_tx = self.decode_tx;

        while let Some(result) = download_rx.recv().await {
            match result {
                Ok(compressed) => {
                    let decode_tx = decode_tx.clone();
                    let progress = self.progress.clone();

                    rayon::spawn(move || {
                        let depot_key = compressed.depot.depot_key.clone();
                        let compressed_data = compressed.compressed_data;
                        let uncompressed_size = compressed.uncompressed_size;
                        let crc = compressed.crc;

                        let result = SteamChunk::new(compressed_data)
                            .decrypt(depot_key.as_deref())
                            .and_then(|chunk| chunk.decompress_and_verify(uncompressed_size, crc));

                        match result {
                            Ok(chunk) => {
                                let data = chunk.into_vec();
                                let len = data.len() as u64;
                                progress.on_decode_progress(len);
                                if let Err(e) = decode_tx.blocking_send(Ok(DownloadedChunk {
                                    file_index: compressed.file_index,
                                    chunk_index: compressed.chunk_index,
                                    chunk_offset: compressed.chunk_offset,
                                    data,
                                })) {
                                    tracing::error!("Failed to send decoded chunk: {}", e);
                                }
                            }
                            Err(e) => {
                                if let Err(send_err) = decode_tx.blocking_send(Err(e)) {
                                    tracing::error!("Failed to send decode error: {}", send_err);
                                }
                            }
                        }
                    });
                }
                Err(e) => {
                    if let Err(send_err) = decode_tx.send(Err(e)).await {
                        tracing::error!("Failed to send decode stream error: {}", send_err);
                    }
                }
            }
        }
    }
}

/// Tracks the write state for a single file being streamed to disk.
struct FileWriteState {
    file_handle: tokio::fs::File,
    file_path: PathBuf,
    /// Next byte offset we expect to write (i.e., current end of contiguous written region).
    next_offset: u64,
    /// Chunks that arrived out of order, keyed by their byte offset.
    pending_chunks: BTreeMap<u64, Vec<u8>>,
    /// Number of chunks already written to disk.
    chunks_written: usize,
    /// Total number of chunks expected for this file.
    total_chunks: usize,
    /// Running count of bytes written (for progress reporting on completion).
    bytes_written: u64,
}

impl FileWriteState {
    /// Flush any pending chunks that are contiguous from `next_offset`.
    /// Returns the number of chunks flushed.
    async fn flush_pending(&mut self) -> std::io::Result<usize> {
        let mut flushed = 0usize;
        while let Some(entry) = self.pending_chunks.first_entry() {
            if *entry.key() != self.next_offset {
                break;
            }
            let data = entry.remove();
            self.file_handle.write_all(&data).await?;
            self.next_offset += data.len() as u64;
            self.bytes_written += data.len() as u64;
            self.chunks_written += 1;
            flushed += 1;
        }
        Ok(flushed)
    }

    fn is_complete(&self) -> bool {
        self.chunks_written == self.total_chunks
    }
}

pub struct WriteWorker {
    base_path: PathBuf,
    progress: Arc<dyn ProgressCallback>,
    files: Arc<Vec<DownloadItem>>,
}

impl WriteWorker {
    pub fn new(
        base_path: std::path::PathBuf,
        progress: Arc<dyn ProgressCallback>,
        files: Arc<Vec<DownloadItem>>,
    ) -> Self {
        Self {
            base_path,
            progress,
            files,
        }
    }

    /// Create a `FileWriteState` for the given file index, creating the file on disk.
    async fn open_file_state(&self, file_index: usize) -> Result<FileWriteState> {
        let file = &self.files[file_index];
        let file_path = self.base_path.join(&file.filename);

        if let Some(p) = file_path.parent() {
            tokio::fs::create_dir_all(p).await?;
        }

        let file_handle = tokio::fs::File::create(&file_path).await?;

        Ok(FileWriteState {
            file_handle,
            file_path,
            next_offset: 0,
            pending_chunks: BTreeMap::new(),
            chunks_written: 0,
            total_chunks: file.chunks.len(),
            bytes_written: 0,
        })
    }

    pub async fn start(
        self,
        mut decode_rx: tokio::sync::mpsc::Receiver<Result<DownloadedChunk>>,
    ) -> Result<()> {
        let mut file_states: HashMap<usize, FileWriteState> = HashMap::new();

        while let Some(result) = decode_rx.recv().await {
            match result {
                Ok(chunk) => {
                    let data_len = chunk.data.len() as u64;

                    // Lazily open the file on first chunk arrival.
                    if !file_states.contains_key(&chunk.file_index) {
                        let state = self.open_file_state(chunk.file_index).await?;
                        file_states.insert(chunk.file_index, state);
                    }
                    let state = file_states
                        .get_mut(&chunk.file_index)
                        .expect("file state was just inserted");

                    if chunk.chunk_offset == state.next_offset {
                        // This chunk is the next expected one — write immediately.
                        state.file_handle.write_all(&chunk.data).await?;
                        state.next_offset += chunk.data.len() as u64;
                        state.bytes_written += chunk.data.len() as u64;
                        state.chunks_written += 1;

                        // Flush any subsequent pending chunks that are now contiguous.
                        state.flush_pending().await?;
                    } else {
                        // Out-of-order chunk — buffer it.
                        state
                            .pending_chunks
                            .insert(chunk.chunk_offset, chunk.data);
                    }

                    self.progress.on_write_progress(data_len);

                    if state.is_complete() {
                        state.file_handle.flush().await?;
                        let bytes = state.bytes_written as usize;
                        let path = state.file_path.clone();
                        file_states.remove(&chunk.file_index);
                        self.progress.on_file_complete(&path, bytes);
                    }
                }
                Err(e) => {
                    // Clean up any partially written files before returning.
                    for (_, state) in file_states.into_iter() {
                        drop(state.file_handle);
                        if let Err(rm_err) = tokio::fs::remove_file(&state.file_path).await {
                            tracing::warn!(
                                "Failed to remove incomplete file {}: {}",
                                state.file_path.display(),
                                rm_err
                            );
                        }
                    }
                    tracing::error!("Error in decode stream: {}", e);
                    return Err(e);
                }
            }
        }

        // Channel closed — check for any files that didn't receive all chunks.
        if !file_states.is_empty() {
            let mut incomplete_files = Vec::new();
            for (file_index, state) in file_states.into_iter() {
                let filename = self.files[file_index].filename.clone();
                tracing::warn!(
                    "Incomplete file '{}': received {}/{} chunks",
                    filename,
                    state.chunks_written + state.pending_chunks.len(),
                    state.total_chunks
                );
                drop(state.file_handle);
                if let Err(rm_err) = tokio::fs::remove_file(&state.file_path).await {
                    tracing::warn!(
                        "Failed to remove incomplete file {}: {}",
                        state.file_path.display(),
                        rm_err
                    );
                }
                incomplete_files.push((filename, state.chunks_written + state.pending_chunks.len(), state.total_chunks));
            }

            let (filename, received, total) = incomplete_files.into_iter().next().unwrap();
            return Err(SteamVentError::Cdn(
                crate::error::CdnError::IncompleteDownload {
                    filename,
                    received,
                    total,
                },
            ));
        }

        Ok(())
    }
}
