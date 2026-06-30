use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::config::{Config, ServerMode};
use crate::downloader::cdn::DepotContext;
use crate::downloader::cdn::http::HttpFetcher;
use crate::downloader::cdn::servers::{CdnServer, CdnServerList};
use crate::downloader::item::{ChunkInfo, DownloadItem};
use crate::downloader::progress::NoopProgress;
use crate::downloader::random_access::reader::CdnRandomAccessReader;

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
        progress: Arc::new(NoopProgress),
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
        Arc::new(NoopProgress),
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
    reader
        .seek(std::io::SeekFrom::Start(64))
        .await
        .expect("seek failed");

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
        .seek(std::io::SeekFrom::Start(192))
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
    reader
        .seek(std::io::SeekFrom::Start(64))
        .await
        .expect("seek failed");

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
        Arc::new(NoopProgress),
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
        Arc::new(NoopProgress),
    );

    assert_eq!(reader.total_size, 1536 + 256); // 1792
}
