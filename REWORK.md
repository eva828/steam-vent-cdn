# DownloadPipeline + 単一ファイル `download_file` API リワーク

## 目的

現在の2階層ストリーム API (`DownloadStream` → `FileEntry` → bytes) は複数ファイルのチャンクをすべて混在させて処理しており、デマルチプレクサ (DemuxWorker) やファイルインデックスの管理が必要で複雑。

これを **単一ファイルのバイトストリームを返す `download_file` API** に置き換え、呼び出し側は「1ファイルのダウンロード」だけを意識すればよいようにする。複数ファイルのダウンロードは呼び出し側が `download_file` を並行呼び出しするだけでよい。

内部のチャンク単位の並列ダウンロード・並列デコードは維持する。

---

## 新しい型の設計

### 公開型 (`pub`)

| 型 | 役割 |
|---|---|
| `DownloadPipeline` | 共有リソース (HTTPクライアント, セマフォ, 進捗) を保持。`Clone` 可。`download_file` のエントリポイント |
| `FileByteStream` | `download_file` の戻り値。`filename`, `size` + `Stream<Item = Result<Vec<u8>>>`。`JoinSet` を内包しドロップでワーカーを停止 |

### 内部型 (`pub(crate)` / モジュールプライベート)

| 型 | 役割 | 旧型からの変更 |
|---|---|---|
| `RawChunk` | DL済み暗号化・圧縮チャンク。`chunk_offset`, `compressed_data`, `uncompressed_size`, `crc`, `depot_key` | 旧 `CompressedChunk` から `file_index`, `chunk_index`, `depot` を除去。`depot_key` を直接保持 |
| `DecodedChunk` | 復号+展開済みチャンク。`chunk_offset`, `data` | 旧 `DownloadedChunk` から `file_index`, `chunk_index` を除去 |

### 削除する型

- `ChunkDownloadTask` → スパン内のクロージャキャプチャで十分
- `CompressedChunk` → `RawChunk` に置き換え
- `DownloadedChunk` → `DecodedChunk` に置き換え
- `ReassembledChunk` → 削除。再組立ロジックは `Vec<u8>` を直接出力チャネルに送信
- `FileEntry` → `FileByteStream` が置き換え
- `DownloadStream` → 削除
- `DemuxWorker` → ファイルが1つなのでデマルチ不要
- `DownloadWorker`, `DecodeWorker`, `ReassemblyWorker` → 構造体は削除。ロジックは関数に変換

### 変更しない型

- `ProgressCallback`, `DepotContext`, `DownloadManifest`, `ChunkInfo`, `DownloadItem`, `VersionInfo`

---

## 新しいパイプライン構造

```
DownloadPipeline::download_file(&self, &DownloadItem)
  │
  ├─ spawn_chunk_downloads(http_fetcher, sem, progress, item, download_tx)
  │    └─ チャンクごとに tokio::spawn → セマフォで並行DL制御
  │       → RawChunk を download_tx に送信
  │
  ├─ JoinSet::spawn(run_decode_loop)
  │    └─ download_rx → rayon::spawn で復号+展開 → DecodedChunk を decoded_tx に送信
  │
  ├─ JoinSet::spawn(run_reassembly_loop)
  │    └─ decoded_rx → BTreeMap で順序再組立 → Vec<u8> を output_tx に送信
  │
  └─ return FileByteStream { filename, size, rx: output_rx, _handles }
```

複数ファイルの場合:
```
let pipeline = downloader.create_pipeline(progress);
let mut tasks = JoinSet::new();
for file in &files {
    let pipeline = pipeline.clone();
    tasks.spawn(async move {
        let mut stream = pipeline.download_file(&file)?;
        // stream.next() でバイト列を順次消費
    });
}
```

セマフォが `Arc` で共有されるため、全体の並行DL数は `parallel_download` 以下に制御される。

---

## 実装ステップ

### 1. `src/downloader/models.rs`

- `ChunkDownloadTask`, `CompressedChunk`, `DownloadedChunk`, `ReassembledChunk`, `FileEntry`, `DownloadStream` を削除
- `RawChunk`, `DecodedChunk`, `FileByteStream`, `DownloadPipeline` を新設

### 2. `src/downloader/workers.rs`

- 全ワーカー構造体 + `FileReassemblyState` を削除
- 3つの `pub(crate)` 関数に置き換え:
  - `spawn_chunk_downloads(...)` — チャンクDLのspawn
  - `run_decode_loop(...)` — rayon による復号+展開
  - `run_reassembly_loop(...)` — BTreeMap で順序再組立

### 3. `src/downloader/downloader.rs`

- `CdnDownloader::download()` を削除
- `CdnDownloader::create_pipeline()` を追加
- `DownloadPipeline::download_file()` を実装

### 4. `src/downloader/mod.rs`

- re-export を更新

### 5. `src/lib.rs`

- re-export を更新

### 6. `src/cli/writer.rs`

- `write_stream_to_directory` → `write_file` に置き換え

### 7. `src/cli/mod.rs`

- `run_download_pipeline` を JoinSet + `download_file` ベースに変更

---

## 設計判断

- **ワーカー構造体 → 関数:** 1ファイル用パイプラインでは各ステージがシンプル。pure function の方がテストしやすい
- **`RawChunk` に `depot_key` を直接保持:** `Arc<DepotContext>` を渡す必要がなくなり型が閉じる。鍵は数十バイトなのでクローンコスト無視可
- **decode と reassembly を分離 (2関数):** 責務明確。1関数に統合すると rayon→async の橋渡しと BTreeMap 管理が混在して複雑化
- **`DownloadPipeline` を `CdnDownloader` から分離:** Steam API 呼び出し (`&mut self`) と HTTP チャンクDL (`&self` で並行可) の責務分離
- **`DownloadPipeline` は `Clone`:** 全フィールドが `Arc` / `Clone` なので低コスト。JoinSet の spawn クロージャに渡しやすい
