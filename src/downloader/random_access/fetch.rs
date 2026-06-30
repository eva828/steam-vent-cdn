use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::downloader::archive::SteamChunk;
use crate::downloader::cdn::DepotContext;
use crate::downloader::cdn::http::HttpFetcher;
use crate::downloader::cdn::servers::RoundRobin;
use crate::downloader::item::ChunkInfo;
use crate::error::{CdnError, Result, SteamVentError};

/// チャンクフェッチ操作の Future 型エイリアス
pub(crate) type ChunkFetchFuture = Pin<Box<dyn Future<Output = Result<(usize, Vec<u8>)>> + Send>>;

/// 指定チャンクをダウンロード→復号→展開する非同期処理を構築する。
pub(crate) fn build_fetch_future(
    chunk_idx: usize,
    chunk: &ChunkInfo,
    depot: &Arc<DepotContext>,
    http_fetcher: &HttpFetcher,
    download_sem: &Arc<tokio::sync::Semaphore>,
) -> ChunkFetchFuture {
    let chunk_id = hex::encode(&chunk.sha);
    let uncompressed_size = chunk.uncompressed_size;
    let crc = chunk.crc;
    let depot = Arc::clone(depot);
    let depot_key = depot.depot_key.clone();
    let http_fetcher = http_fetcher.clone();
    let download_sem = Arc::clone(download_sem);

    Box::pin(async move {
        // セマフォ取得（他の Reader や DownloadPipeline と帯域を共有）
        let _permit = download_sem
            .acquire()
            .await
            .map_err(|_| SteamVentError::Cdn(CdnError::SemaphoreClosed))?;

        let compressed_data = http_fetcher
            .download_chunk(
                &RoundRobin::new(&depot.server_hosts),
                depot.depot_id,
                &chunk_id,
            )
            .await?;

        // 復号→展開は CPU ヘビーなので rayon で行う
        let (tx, rx) = tokio::sync::oneshot::channel();

        rayon::spawn(move || {
            let result = SteamChunk::new(compressed_data)
                .decrypt(depot_key.as_deref())
                .and_then(|chunk| chunk.decompress_and_verify(uncompressed_size, crc))
                .map(|chunk| (chunk_idx, chunk.into_vec()));
            let _ = tx.send(result);
        });

        rx.await
            .map_err(|_| SteamVentError::Cdn(CdnError::AllServersFailed))?
    })
}
