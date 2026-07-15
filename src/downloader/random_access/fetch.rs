use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::downloader::archive::SteamChunk;
use crate::downloader::cdn::DepotContext;
use crate::downloader::cdn::http::CdnFetcher;
use crate::downloader::cdn::servers::RoundRobin;
use crate::downloader::manifest::ChunkInfo;
use crate::downloader::progress::ProgressCallback;
use crate::error::{DownloadError, Result};

pub(crate) type ChunkFetchFuture = Pin<Box<dyn Future<Output = Result<(usize, Vec<u8>)>> + Send>>;

pub(crate) fn build_fetch_future(
    chunk_idx: usize,
    chunk: &ChunkInfo,
    depot: &Arc<DepotContext>,
    http_fetcher: &CdnFetcher,
    progress: &Arc<dyn ProgressCallback>,
) -> ChunkFetchFuture {
    let chunk_id = hex::encode(&chunk.sha1);
    let compressed_size = chunk.compressed_size;
    let uncompressed_size = chunk.uncompressed_size;
    let crc = chunk.crc;
    let progress = Arc::clone(progress);
    let depot = Arc::clone(depot);
    let depot_key = depot.depot_key.clone();
    let http_fetcher = http_fetcher.clone();

    Box::pin(async move {
        let compressed_data = http_fetcher
            .download_chunk(&RoundRobin::new(&depot.servers), depot.depot_id, &chunk_id)
            .await?;

        progress.on_download_progress(compressed_size as u64);

        let (tx, rx) = tokio::sync::oneshot::channel();

        rayon::spawn(move || {
            let result = SteamChunk::new(compressed_data)
                .decrypt(depot_key.as_deref())
                .and_then(|chunk| chunk.decompress_and_verify(uncompressed_size, crc))
                .map(|chunk| {
                    let data = chunk.into_vec();
                    progress.on_decode_progress(data.len() as u64);
                    (chunk_idx, data)
                });
            let _ = tx.send(result);
        });

        rx.await.map_err(|_| DownloadError::AllServersFailed)?
    })
}
