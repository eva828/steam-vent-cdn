use std::collections::HashMap;
use std::io::SeekFrom;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};

use crate::downloader::cdn::DepotContext;
use crate::downloader::cdn::http::HttpFetcher;
use crate::downloader::item::{ChunkInfo, DownloadItem};
use crate::downloader::random_access::fetch::{ChunkFetchFuture, build_fetch_future};

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
/// use steam_vent_cdn::{CdnDownloader, DownloadItem};
///
/// async fn read_header(
///     downloader: &CdnDownloader,
///     download_item: &DownloadItem,
/// ) -> Result<(), Box<dyn std::error::Error>> {
///     // CdnDownloader から作成
///     let mut reader = downloader.create_reader(download_item);
///
///     // 先頭1KBだけ読む（ヘッダー取得に十分な量）
///     let mut header = vec![0u8; 1024];
///     reader.read_exact(&mut header).await?;
///
///     // 任意のオフセットにシークして読み取り
///     reader.seek(std::io::SeekFrom::Start(0x1000)).await?;
///     let mut data = vec![0u8; 256];
///     reader.read_exact(&mut data).await?;
///     Ok(())
/// }
/// ```
pub struct CdnRandomAccessReader {
    /// ファイルのチャンクマップ（offset 昇順にソート済み）
    pub(crate) chunks: Vec<ChunkInfo>,
    /// depot 情報（サーバーリスト、depot_id、depot_key）
    pub(crate) depot: Arc<DepotContext>,
    /// ネットワークフェッチャー
    pub(crate) http_fetcher: HttpFetcher,
    /// 同時ダウンロード数を制限するセマフォ（DownloadPipeline と共有）
    pub(crate) download_sem: Arc<tokio::sync::Semaphore>,
    /// ファイル全体のサイズ
    pub(crate) total_size: u64,
    /// 現在の論理的なカーソル位置
    pub(crate) position: u64,
    /// デコード済みチャンクのキャッシュ (chunk_index → decoded bytes)
    pub(crate) cache: HashMap<usize, Vec<u8>>,
    /// 非同期ダウンロード中の pending future
    pub(crate) pending_fetch: Option<ChunkFetchFuture>,
    /// SeekFrom を一時保持する（start_seek → poll_complete の間）
    pub(crate) seek_pending: bool,
}

impl CdnRandomAccessReader {
    /// `DownloadItem` からReaderを構築する。
    ///
    /// チャンクリストは offset 昇順にソートされる。
    /// これにより `chunk_index_for_offset` の二分探索が正しく機能する。
    pub(crate) fn new(
        item: &DownloadItem,
        http_fetcher: HttpFetcher,
        download_sem: Arc<tokio::sync::Semaphore>,
    ) -> Self {
        // Steam マニフェストのチャンクは必ずしも offset 昇順ではないため、
        // ソートしてから格納する。chunk_index_for_offset の二分探索が正しく機能するために必要。
        let mut chunks = item.chunks.clone();
        chunks.sort_unstable_by_key(|c| c.offset);

        let total_size = item.size.unwrap_or_else(|| {
            // サイズが不明な場合、チャンクマップから推定
            // ソート済みなので last() が最大 offset を持つ
            chunks
                .last()
                .map(|c| c.offset + c.uncompressed_size as u64)
                .unwrap_or(0)
        });

        Self {
            chunks,
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
    pub(crate) fn chunk_index_for_offset(&self, offset: u64) -> Option<usize> {
        // 二分探索: offset 以下の最大の chunk.offset を持つチャンクを探す
        let idx = self
            .chunks
            .partition_point(|c| c.offset <= offset)
            .checked_sub(1)?;

        let chunk = &self.chunks[idx];
        let chunk_end = chunk.offset + chunk.uncompressed_size as u64;
        if offset < chunk_end { Some(idx) } else { None }
    }
}

impl Unpin for CdnRandomAccessReader {}

impl AsyncRead for CdnRandomAccessReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // フェッチが完了したらすぐにキャッシュから読み込む。
        // Poll::Pending を挟まずに loop で直接遷移することで、
        // read_exact がタイミング依存でバッファ未充填のまま終了するのを防ぐ。
        loop {
            // EOF チェック
            if self.position >= self.total_size {
                return Poll::Ready(Ok(()));
            }

            // pending_fetch がある場合、それを先にポーリング
            if let Some(ref mut fut) = self.pending_fetch {
                match Pin::as_mut(fut).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok((chunk_idx, data))) => {
                        self.cache.insert(chunk_idx, data);
                        self.pending_fetch = None;
                        continue;
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
            if self.cache.contains_key(&chunk_idx) {
                let chunk = &self.chunks[chunk_idx];
                let offset_in_chunk = (self.position - chunk.offset) as usize;
                let chunk_data = &self.cache[&chunk_idx];
                let available = chunk_data.len() - offset_in_chunk;
                let to_copy = available.min(buf.remaining());
                buf.put_slice(&chunk_data[offset_in_chunk..offset_in_chunk + to_copy]);
                self.position += to_copy as u64;
                return Poll::Ready(Ok(()));
            }

            // キャッシュミス → fetch future を開始してループ先頭に戻る
            let chunk = &self.chunks[chunk_idx];
            let fut = build_fetch_future(
                chunk_idx,
                chunk,
                &self.depot,
                &self.http_fetcher,
                &self.download_sem,
            );
            self.pending_fetch = Some(fut);
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

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
