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
use super::servers::RoundRobin;

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
/// use steam_vent_cdn::{DownloadItem, DownloadPipeline};
///
/// async fn read_header(
///     pipeline: &DownloadPipeline,
///     download_item: &DownloadItem,
/// ) -> Result<(), Box<dyn std::error::Error>> {
///     // DownloadPipeline から作成
///     let mut reader = pipeline.create_reader(download_item);
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
                match fut.as_mut().poll(cx) {
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
            let fut = Self::build_fetch_future(
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

// =============================================================================
// Unit Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    use crate::config::ServerMode;
    use crate::downloader::servers::{CdnServer, CdnServerList};

    // ---------------------------------------------------------------------------
    // テスト用のダミー ChunkInfo と DepotContext を構築するヘルパー
    // ---------------------------------------------------------------------------

    /// テスト用の ChunkInfo を作成する。
    /// sha/crc はダミー。uncompressed_size はデータ長として使われる。
    fn make_chunk(offset: u64, size: u32) -> ChunkInfo {
        ChunkInfo {
            sha: vec![0u8; 20],
            crc: 0,
            offset,
            uncompressed_size: size,
            compressed_size: size,
        }
    }

    /// テスト用の DepotContext（実際のネットワークを使わない）
    fn dummy_depot() -> Arc<DepotContext> {
        Arc::new(DepotContext {
            server_hosts: CdnServerList::sorted(
                vec![CdnServer {
                    vhost: "localhost".to_string(),
                    server_type: "SteamCache".to_string(),
                    weighted_load: 0.0,
                    load: 0,
                    priority_class: 0,
                }],
                ServerMode::All,
            ),
            depot_id: 0,
            depot_key: None,
        })
    }

    /// テスト用の HttpFetcher（実際のネットワークは呼ばれない）
    fn dummy_fetcher() -> HttpFetcher {
        HttpFetcher::new(Config::default()).expect("failed to build dummy HttpFetcher")
    }

    /// `CdnRandomAccessReader` をキャッシュ済みのデータで直接構築するヘルパー。
    ///
    /// 実際の HTTP フェッチを行わず、チャンクデータをあらかじめキャッシュに
    /// 注入することで、ネットワークなしでリーダーをテストできる。
    fn make_reader_with_cache(
        chunks: Vec<ChunkInfo>,
        cache: HashMap<usize, Vec<u8>>,
        total_size: u64,
    ) -> CdnRandomAccessReader {
        // 内部状態を直接組み立てる
        // ※ sort は new() の責務なのでここでは明示的にソートして渡す
        let mut sorted_chunks = chunks;
        sorted_chunks.sort_unstable_by_key(|c| c.offset);

        CdnRandomAccessReader {
            total_size,
            chunks: sorted_chunks,
            depot: dummy_depot(),
            http_fetcher: dummy_fetcher(),
            download_sem: Arc::new(tokio::sync::Semaphore::new(1)),
            position: 0,
            cache,
            pending_fetch: None,
            seek_pending: false,
        }
    }

    // ---------------------------------------------------------------------------
    // チャンクソートのテスト
    // ---------------------------------------------------------------------------

    /// チャンクが逆順（offset 降順）で渡されても、
    /// new() 後には昇順にソートされていることを確認する。
    #[test]
    fn test_chunks_are_sorted_on_new() {
        let item = DownloadItem {
            filename: "test.tmod".to_string(),
            size: Some(200),
            flags: None,
            depot: dummy_depot(),
            chunks: vec![
                make_chunk(100, 100), // 意図的に逆順
                make_chunk(0, 100),
            ],
        };

        let reader = CdnRandomAccessReader::new(
            &item,
            dummy_fetcher(),
            Arc::new(tokio::sync::Semaphore::new(1)),
        );

        // ソート後は [offset=0, offset=100] の順になっているはず
        assert_eq!(reader.chunks[0].offset, 0);
        assert_eq!(reader.chunks[1].offset, 100);
    }

    /// 逆順チャンクのリーダーで chunk_index_for_offset が正しく動くことを確認。
    #[test]
    fn test_chunk_index_for_offset_with_sorted_chunks() {
        let chunks = vec![
            make_chunk(0, 1024),
            make_chunk(1024, 1024),
            make_chunk(2048, 512),
        ];
        let total_size = 2048 + 512;
        let reader = make_reader_with_cache(chunks, HashMap::new(), total_size);

        assert_eq!(reader.chunk_index_for_offset(0), Some(0));
        assert_eq!(reader.chunk_index_for_offset(512), Some(0));
        assert_eq!(reader.chunk_index_for_offset(1023), Some(0));
        assert_eq!(reader.chunk_index_for_offset(1024), Some(1));
        assert_eq!(reader.chunk_index_for_offset(2047), Some(1));
        assert_eq!(reader.chunk_index_for_offset(2048), Some(2));
        assert_eq!(reader.chunk_index_for_offset(2559), Some(2));
        // chunk 範囲外
        assert_eq!(reader.chunk_index_for_offset(2560), None);
    }

    // ---------------------------------------------------------------------------
    // read_exact の動作テスト
    // ---------------------------------------------------------------------------

    /// 単一チャンク内での read_exact が成功することを確認。
    #[tokio::test]
    async fn test_read_exact_within_single_chunk() {
        let data: Vec<u8> = (0u8..=255).collect(); // 256 バイト
        let chunks = vec![make_chunk(0, 256)];
        let mut cache = HashMap::new();
        cache.insert(0, data.clone());

        let mut reader = make_reader_with_cache(chunks, cache, 256);

        let mut buf = vec![0u8; 256];
        reader
            .read_exact(&mut buf)
            .await
            .expect("read_exact failed");
        assert_eq!(buf, data);
    }

    /// チャンク境界を跨ぐ read_exact が成功することを確認。
    #[tokio::test]
    async fn test_read_exact_across_chunk_boundary() {
        // chunk 0: offset 0,   size 128 → 0x00..=0x7F
        // chunk 1: offset 128, size 128 → 0x80..=0xFF
        let chunk0_data: Vec<u8> = (0x00u8..0x80).collect();
        let chunk1_data: Vec<u8> = (0x80u8..=0xFF).collect();

        let chunks = vec![make_chunk(0, 128), make_chunk(128, 128)];
        let mut cache = HashMap::new();
        cache.insert(0, chunk0_data.clone());
        cache.insert(1, chunk1_data.clone());

        let mut reader = make_reader_with_cache(chunks, cache, 256);

        // チャンク境界 (offset 64) から始めて 128 バイト読む
        // → chunk 0 の後半 64 バイト + chunk 1 の前半 64 バイト
        reader.seek(SeekFrom::Start(64)).await.expect("seek failed");

        let mut buf = vec![0u8; 128];
        reader
            .read_exact(&mut buf)
            .await
            .expect("read_exact across boundary failed");

        let expected: Vec<u8> = (0x40..0xC0).collect();
        assert_eq!(buf, expected);
    }

    /// seek 後の read_exact が正しく動くことを確認。
    #[tokio::test]
    async fn test_read_exact_after_seek() {
        let data: Vec<u8> = (0u8..=255).collect();
        let chunks = vec![make_chunk(0, 256)];
        let mut cache = HashMap::new();
        cache.insert(0, data.clone());

        let mut reader = make_reader_with_cache(chunks, cache, 256);

        // 末尾 64 バイトにシークして読む
        reader
            .seek(SeekFrom::Start(192))
            .await
            .expect("seek failed");

        let mut buf = vec![0u8; 64];
        reader
            .read_exact(&mut buf)
            .await
            .expect("read_exact after seek failed");
        assert_eq!(buf, data[192..]);
    }

    /// EOF 境界での read が正しく 0 バイトを返すことを確認。
    #[tokio::test]
    async fn test_read_at_eof_returns_zero_bytes() {
        let data = vec![0u8; 64];
        let chunks = vec![make_chunk(0, 64)];
        let mut cache = HashMap::new();
        cache.insert(0, data);

        let mut reader = make_reader_with_cache(chunks, cache, 64);

        // EOF までシーク
        reader.seek(SeekFrom::Start(64)).await.expect("seek failed");

        let mut buf = vec![0u8; 10];
        let n = reader.read(&mut buf).await.expect("read at EOF failed");
        assert_eq!(n, 0, "read at EOF should return 0 bytes");
    }

    /// 逆順チャンクで作った DownloadItem から CdnRandomAccessReader を構築しても
    /// ソート後は正しく読み取れることを確認。
    #[tokio::test]
    async fn test_reversed_chunks_read_correctly() {
        let chunk0_data: Vec<u8> = vec![0x01; 64];
        let chunk1_data: Vec<u8> = vec![0x02; 64];

        // chunk を逆順で DownloadItem に入れる
        let item = DownloadItem {
            filename: "test.tmod".to_string(),
            size: Some(128),
            flags: None,
            depot: dummy_depot(),
            chunks: vec![
                make_chunk(64, 64), // ← 逆順
                make_chunk(0, 64),
            ],
        };

        let mut reader = CdnRandomAccessReader::new(
            &item,
            dummy_fetcher(),
            Arc::new(tokio::sync::Semaphore::new(1)),
        );

        // new() 後のソート順を確認
        assert_eq!(reader.chunks[0].offset, 0);
        assert_eq!(reader.chunks[1].offset, 64);

        // キャッシュを手動注入（ソート後のインデックスに合わせる）
        reader.cache.insert(0, chunk0_data.clone());
        reader.cache.insert(1, chunk1_data.clone());

        // 全 128 バイトを read_exact で読む
        let mut buf = vec![0u8; 128];
        reader
            .read_exact(&mut buf)
            .await
            .expect("read_exact on reversed chunks failed");

        assert_eq!(&buf[..64], chunk0_data.as_slice());
        assert_eq!(&buf[64..], chunk1_data.as_slice());
    }

    // ---------------------------------------------------------------------------
    // total_size の計算テスト
    // ---------------------------------------------------------------------------

    /// item.size が None のとき、チャンクから正しく total_size が計算されることを確認。
    #[test]
    fn test_total_size_computed_from_chunks_when_none() {
        let item = DownloadItem {
            filename: "test.tmod".to_string(),
            size: None, // 明示的に None
            flags: None,
            depot: dummy_depot(),
            chunks: vec![
                make_chunk(0, 1024),
                make_chunk(1024, 512),
                make_chunk(1536, 256),
            ],
        };

        let reader = CdnRandomAccessReader::new(
            &item,
            dummy_fetcher(),
            Arc::new(tokio::sync::Semaphore::new(1)),
        );

        assert_eq!(reader.total_size, 1536 + 256); // 1792
    }
}
