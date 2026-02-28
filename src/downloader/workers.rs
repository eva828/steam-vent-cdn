use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::archive::SteamChunk;
use crate::error::{Result, SteamVentError};

use super::http::HttpFetcher;

use super::models::{
    ChunkDownloadTask, CompressedChunk, DownloadItem, DownloadedChunk, ProgressCallback,
    ReassembledChunk,
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

/// Tracks the reassembly state for a single file's chunks.
struct FileReassemblyState {
    /// Next byte offset we expect to emit (i.e., current end of contiguous region).
    next_offset: u64,
    /// Chunks that arrived out of order, keyed by their byte offset.
    pending_chunks: BTreeMap<u64, Vec<u8>>,
    /// Number of chunks already emitted.
    chunks_emitted: usize,
    /// Total number of chunks expected for this file.
    total_chunks: usize,
}

impl FileReassemblyState {
    fn is_complete(&self) -> bool {
        self.chunks_emitted == self.total_chunks
    }
}

/// Receives decoded (but potentially out-of-order) chunks and re-emits them
/// in sequential offset order per file as [`ReassembledChunk`]s.
///
/// This worker does **no** file I/O — it only buffers and reorders.
pub(crate) struct ReassemblyWorker {
    reassembly_tx: tokio::sync::mpsc::Sender<Result<ReassembledChunk>>,
    progress: Arc<dyn ProgressCallback>,
    files: Arc<Vec<DownloadItem>>,
}

impl ReassemblyWorker {
    pub fn new(
        reassembly_tx: tokio::sync::mpsc::Sender<Result<ReassembledChunk>>,
        progress: Arc<dyn ProgressCallback>,
        files: Arc<Vec<DownloadItem>>,
    ) -> Self {
        Self {
            reassembly_tx,
            progress,
            files,
        }
    }

    /// Flush any pending chunks that are contiguous from `next_offset` and
    /// send them as `ReassembledChunk`s.
    async fn flush_pending(
        state: &mut FileReassemblyState,
        file_index: usize,
        tx: &tokio::sync::mpsc::Sender<Result<ReassembledChunk>>,
        progress: &dyn ProgressCallback,
    ) -> Result<()> {
        while let Some(entry) = state.pending_chunks.first_entry() {
            if *entry.key() != state.next_offset {
                break;
            }
            let data = entry.remove();
            let len = data.len() as u64;
            state.next_offset += len;
            state.chunks_emitted += 1;
            let is_last = state.is_complete();
            progress.on_write_progress(len);
            if let Err(e) = tx
                .send(Ok(ReassembledChunk {
                    file_index,
                    offset: state.next_offset - len,
                    data,
                    is_last,
                }))
                .await
            {
                tracing::error!("Failed to send reassembled chunk: {}", e);
                return Err(SteamVentError::Io(std::io::Error::other(
                    "reassembly channel closed",
                )));
            }
        }
        Ok(())
    }

    pub async fn start(
        self,
        mut decode_rx: tokio::sync::mpsc::Receiver<Result<DownloadedChunk>>,
    ) {
        let mut file_states: HashMap<usize, FileReassemblyState> = HashMap::new();

        while let Some(result) = decode_rx.recv().await {
            match result {
                Ok(chunk) => {
                    let file_index = chunk.file_index;
                    let data_len = chunk.data.len() as u64;

                    // Lazily initialise state on first chunk arrival.
                    let state = file_states.entry(file_index).or_insert_with(|| {
                        FileReassemblyState {
                            next_offset: 0,
                            pending_chunks: BTreeMap::new(),
                            chunks_emitted: 0,
                            total_chunks: self.files[file_index].chunks.len(),
                        }
                    });

                    if chunk.chunk_offset == state.next_offset {
                        // This chunk is the next expected one — emit immediately.
                        state.next_offset += data_len;
                        state.chunks_emitted += 1;
                        let is_last = state.is_complete();
                        self.progress.on_write_progress(data_len);
                        if let Err(e) = self
                            .reassembly_tx
                            .send(Ok(ReassembledChunk {
                                file_index,
                                offset: chunk.chunk_offset,
                                data: chunk.data,
                                is_last,
                            }))
                            .await
                        {
                            tracing::error!("Failed to send reassembled chunk: {}", e);
                            return;
                        }

                        // Flush any subsequent pending chunks that are now contiguous.
                        if let Err(e) = Self::flush_pending(
                            state,
                            file_index,
                            &self.reassembly_tx,
                            self.progress.as_ref(),
                        )
                        .await
                        {
                            tracing::error!("Flush pending failed: {}", e);
                            return;
                        }
                    } else {
                        // Out-of-order chunk — buffer it.
                        state.pending_chunks.insert(chunk.chunk_offset, chunk.data);
                    }

                    // Remove completed file state to free memory.
                    if file_states
                        .get(&file_index)
                        .map_or(false, |s| s.is_complete())
                    {
                        file_states.remove(&file_index);
                    }
                }
                Err(e) => {
                    tracing::error!("Error in decode stream: {}", e);
                    let _ = self.reassembly_tx.send(Err(e)).await;
                    return;
                }
            }
        }

        // Channel closed — check for any files that didn't receive all chunks.
        if !file_states.is_empty() {
            for (file_index, state) in &file_states {
                let filename = &self.files[*file_index].filename;
                let received = state.chunks_emitted + state.pending_chunks.len();
                tracing::warn!(
                    "Incomplete file '{}': received {}/{} chunks",
                    filename,
                    received,
                    state.total_chunks
                );
            }

            let (file_index, state) = file_states.into_iter().next().unwrap();
            let filename = self.files[file_index].filename.clone();
            let received = state.chunks_emitted + state.pending_chunks.len();
            let _ = self
                .reassembly_tx
                .send(Err(SteamVentError::Cdn(
                    crate::error::CdnError::IncompleteDownload {
                        filename,
                        received,
                        total: state.total_chunks,
                    },
                )))
                .await;
        }
    }
}
