use std::collections::HashMap;
use std::future::Future;
use std::io::SeekFrom;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};

use crate::archive::SteamChunk;
use crate::error::{CdnError, Result, SteamVentError};

use super::http::HttpFetcher;
use super::models::{ChunkInfo, DepotContext, DownloadItem};

/// チャンクフェッチ操作の Future 型エイリアス
type ChunkFetchFuture = Pin<Box<dyn Future<Output = Result<(usize, Vec<u8>)>> + Send>>;

/// CDN上のファイルに対して、オンデマンドでチャンクをフェッチする
/// `AsyncRead + AsyncSeek` ラッパー。
///
/// ファイル全体をダウンロードせずに、`seek()` と `read()` によって
/// アクセスされたバイト範囲を含むチャンクのみをCDNから取得します。
/// tmodファイルのヘッダー解析など、部分的な読み取りに最適です。
///
/// # 使い方
///
/// ```rust,no_run
/// use tokio::io::{AsyncReadExt, AsyncSeekExt};
///
/// // DownloadPipeline から作成
/// let mut reader = pipeline.create_reader(&download_item);
///
/// // 先頭1KBだけ読む（ヘッダー取得に十分な量）
/// let mut header = vec![0u8; 1024];
/// reader.read_exact(&mut header).await?;
///
/// // 任意のオフセットにシークして読み取り
/// reader.seek(std::io::SeekFrom::Start(0x1000)).await?;
/// let mut data = vec![0u8; 256];
/// reader.read_exact(&mut data).await?;
/// ```
pub struct CdnRandomAccessReader {
    /// ファイルのチャンクマップ（offset 昇順にソート済み）
    chunks: Vec<ChunkInfo>,
    /// depot 情報（サーバーリスト、depot_id、depot_key）
    depot: Arc<DepotContext>,
    /// ネットワークフェッチャー
    http_fetcher: HttpFetcher,
    /// 同時ダウンロード数を制限するセマフォ（DownloadPipeline と共有）
    download_sem: Arc<tokio::sync::Semaphore>,
    /// ファイル全体のサイズ
    total_size: u64,
    /// 現在の論理的なカーソル位置
    position: u64,
    /// デコード済みチャンクのキャッシュ (chunk_index → decoded bytes)
    cache: HashMap<usize, Vec<u8>>,
    /// 非同期ダウンロード中の pending future
    pending_fetch: Option<ChunkFetchFuture>,
    /// SeekFrom を一時保持する（start_seek → poll_complete の間）
    seek_pending: bool,
}

impl CdnRandomAccessReader {
    /// `DownloadItem` からReaderを構築する。
    pub(crate) fn new(
        item: &DownloadItem,
        http_fetcher: HttpFetcher,
        download_sem: Arc<tokio::sync::Semaphore>,
    ) -> Self {
        let total_size = item.size.unwrap_or_else(|| {
            // サイズが不明な場合、チャンクマップから推定
            item.chunks
                .iter()
                .map(|c| c.offset + c.uncompressed_size as u64)
                .max()
                .unwrap_or(0)
        });

        Self {
            chunks: item.chunks.clone(),
            depot: Arc::clone(&item.depot),
            http_fetcher,
            download_sem,
            total_size,
            position: 0,
            cache: HashMap::new(),
            pending_fetch: None,
            seek_pending: false,
        }
    }

    /// 指定オフセットが属するチャンクのインデックスを返す。
    /// チャンクは offset 昇順でソートされている前提。
    fn chunk_index_for_offset(&self, offset: u64) -> Option<usize> {
        // 二分探索: offset 以下の最大の chunk.offset を持つチャンクを探す
        let idx = self
            .chunks
            .partition_point(|c| c.offset <= offset)
            .checked_sub(1)?;

        let chunk = &self.chunks[idx];
        let chunk_end = chunk.offset + chunk.uncompressed_size as u64;
        if offset < chunk_end { Some(idx) } else { None }
    }

    /// 指定チャンクをダウンロード→復号→展開する非同期処理を構築する。
    fn build_fetch_future(
        chunk_idx: usize,
        chunk: &ChunkInfo,
        depot: &Arc<DepotContext>,
        http_fetcher: &HttpFetcher,
        download_sem: &Arc<tokio::sync::Semaphore>,
    ) -> ChunkFetchFuture {
        let chunk_id = hex::encode(&chunk.sha);
        let uncompressed_size = chunk.uncompressed_size;
        let crc = chunk.crc;
        let server_hosts = depot.server_hosts.clone();
        let depot_id = depot.depot_id;
        let depot_key = depot.depot_key.clone();
        let http_fetcher = http_fetcher.clone();
        let download_sem = Arc::clone(download_sem);

        Box::pin(async move {
            // セマフォ取得（他の Reader や DownloadPipeline と帯域を共有）
            let _permit = download_sem
                .acquire()
                .await
                .map_err(|_| SteamVentError::Cdn(CdnError::SemaphoreClosed))?;

            let start_server = chunk_idx % server_hosts.len().max(1);
            let compressed_data = http_fetcher
                .download_chunk(&server_hosts, depot_id, &chunk_id, start_server)
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
}

impl Unpin for CdnRandomAccessReader {}

impl AsyncRead for CdnRandomAccessReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // EOF チェック
        if self.position >= self.total_size {
            return Poll::Ready(Ok(()));
        }

        // pending_fetch がある場合、それを先にポーリング
        if let Some(ref mut fut) = self.pending_fetch {
            match fut.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok((chunk_idx, data))) => {
                    self.cache.insert(chunk_idx, data);
                    self.pending_fetch = None;
                    // fall through してキャッシュから読む
                }
                Poll::Ready(Err(e)) => {
                    self.pending_fetch = None;
                    return Poll::Ready(Err(std::io::Error::other(e.to_string())));
                }
            }
        }

        // 現在位置のチャンクインデックスを特定
        let chunk_idx = match self.chunk_index_for_offset(self.position) {
            Some(idx) => idx,
            None => {
                // チャンク範囲外 = EOF
                return Poll::Ready(Ok(()));
            }
        };

        // キャッシュにある？
        if let Some(chunk_data) = self.cache.get(&chunk_idx) {
            let chunk = &self.chunks[chunk_idx];
            let offset_in_chunk = (self.position - chunk.offset) as usize;
            let available = chunk_data.len() - offset_in_chunk;
            let to_copy = available.min(buf.remaining());
            buf.put_slice(&chunk_data[offset_in_chunk..offset_in_chunk + to_copy]);
            self.position += to_copy as u64;
            return Poll::Ready(Ok(()));
        }

        // キャッシュミス → fetch future を開始
        let chunk = &self.chunks[chunk_idx];
        let fut = Self::build_fetch_future(
            chunk_idx,
            chunk,
            &self.depot,
            &self.http_fetcher,
            &self.download_sem,
        );
        self.pending_fetch = Some(fut);

        // 即座にポーリングして受理されなければ Pending
        let fut = self.pending_fetch.as_mut().unwrap();
        match fut.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok((idx, data))) => {
                self.cache.insert(idx, data);
                self.pending_fetch = None;
                // 再帰的に自身を read する代わりに、cx.waker を起こして次の poll で読む
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Ready(Err(e)) => {
                self.pending_fetch = None;
                Poll::Ready(Err(std::io::Error::other(e.to_string())))
            }
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
        self.seek_pending = true;
        // seek 時に進行中のフェッチはキャンセル
        self.pending_fetch = None;
        Ok(())
    }

    fn poll_complete(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<u64>> {
        self.seek_pending = false;
        Poll::Ready(Ok(self.position))
    }
}
