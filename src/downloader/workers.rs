use std::collections::HashMap;
use std::sync::Arc;

use crate::cdn::CdnClient;
use crate::error::Result;

use super::models::{
    ChunkDownloadTask, CompressedChunk, DownloadItem, DownloadedChunk, ProgressCallback,
};

pub struct DownloadWorker {
    cdn_client: CdnClient,
    download_sem: Arc<tokio::sync::Semaphore>,
    progress: Arc<dyn ProgressCallback>,
    download_tx: tokio::sync::mpsc::Sender<Result<CompressedChunk>>,
}

impl DownloadWorker {
    pub fn new(
        cdn_client: CdnClient,
        parallel_download: usize,
        progress: Arc<dyn ProgressCallback>,
        download_tx: tokio::sync::mpsc::Sender<Result<CompressedChunk>>,
    ) -> Self {
        Self {
            cdn_client,
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
            let cdn_client = self.cdn_client.clone();
            let progress = self.progress.clone();

            tokio::spawn(async move {
                let _permit = download_sem.acquire().await.unwrap();
                let start_server = task_index % task.depot.server_hosts.len().max(1);
                let compressed_data = cdn_client
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
                        let _ = download_tx
                            .send(Ok(CompressedChunk {
                                depot: task.depot,
                                file_index: task.file_index,
                                chunk_index: task.chunk_index,
                                compressed_data: data,
                                uncompressed_size: task.uncompressed_size,
                                crc: task.crc,
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

                        let decrypted =
                            CdnClient::decrypt_chunk(&compressed_data, depot_key.as_deref());

                        let decrypted = match decrypted {
                            Ok(data) => data,
                            Err(e) => {
                                let _ = decode_tx.blocking_send(Err(e));
                                return;
                            }
                        };

                        let uncompressed_size = compressed.uncompressed_size;
                        let crc = compressed.crc;

                        let decompressed =
                            CdnClient::decompress_and_verify(&decrypted, uncompressed_size, crc);

                        match decompressed {
                            Ok(data) => {
                                let len = data.len() as u64;
                                progress.on_decode_progress(len);
                                progress.on_write_progress(len);
                                let _ = decode_tx.blocking_send(Ok(DownloadedChunk {
                                    file_index: compressed.file_index,
                                    chunk_index: compressed.chunk_index,
                                    data,
                                }));
                            }
                            Err(e) => {
                                let _ = decode_tx.blocking_send(Err(e));
                            }
                        }
                    });
                }
                Err(e) => {
                    let _ = decode_tx.send(Err(e)).await;
                }
            }
        }
    }
}

pub struct WriteWorker {
    base_path: std::path::PathBuf,
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

    pub async fn start(
        self,
        mut decode_rx: tokio::sync::mpsc::Receiver<Result<DownloadedChunk>>,
    ) -> Result<()> {
        let mut chunk_buffers: HashMap<usize, Vec<(usize, Vec<u8>)>> = HashMap::new();

        while let Some(result) = decode_rx.recv().await {
            match result {
                Ok(chunk) => {
                    let buffers = chunk_buffers.entry(chunk.file_index).or_default();
                    buffers.push((chunk.chunk_index, chunk.data));

                    let file = &self.files[chunk.file_index];
                    let total_chunks = file.chunks.len();

                    if buffers.len() == total_chunks {
                        let file_path = self.base_path.join(&file.filename);
                        let mut file_data: Vec<(usize, Vec<u8>)> =
                            chunk_buffers.remove(&chunk.file_index).unwrap();
                        file_data.sort_by_key(|(idx, _)| *idx);

                        let mut full_data = Vec::new();
                        for (_, data) in file_data {
                            full_data.extend(data);
                        }

                        if let Some(p) = file_path.parent() {
                            tokio::fs::create_dir_all(p).await?;
                        }
                        tokio::fs::write(&file_path, &full_data).await?;
                        self.progress.on_file_complete(&file_path, full_data.len());
                    }
                }
                Err(e) => {
                    tracing::error!("Error in decode stream: {}", e);
                    return Err(e);
                }
            }
        }

        for (file_index, mut file_data) in chunk_buffers.into_iter() {
            if !file_data.is_empty() {
                let file = &self.files[file_index];
                let file_path = self.base_path.join(&file.filename);
                file_data.sort_by_key(|(idx, _)| *idx);

                let mut full_data = Vec::new();
                for (_, data) in file_data {
                    full_data.extend(data);
                }

                if let Some(p) = file_path.parent() {
                    tokio::fs::create_dir_all(p).await?;
                }
                let mut file_handle = tokio::fs::File::create(&file_path).await?;
                use tokio::io::AsyncWriteExt;
                file_handle.write_all(&full_data).await?;
                self.progress.on_file_complete(&file_path, full_data.len());
            }
        }

        Ok(())
    }
}
