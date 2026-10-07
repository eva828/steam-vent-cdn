# PLAN: アプリ本体（depot）ダウンロード対応

## 目的

`steam-vent-cdn` は現在 Workshop アイテム専用だが、名前が示すとおり Steam CDN から
アプリ本体（depot コンテンツ）もダウンロードできるようにする。

## 決定事項

- 名前: `steam-vent-cdn` を維持
- CLI: サブコマンド（`item` / `app`）
- VDF: 自前のバイナリ KeyValues リーダを実装（新規依存なし）
- 対象: 無料アプリ（匿名）+ 所有アプリ（ログイン）、public ブランチのみ
- 出力: `output/<APP_ID>/<BUILD_ID>/<DEPOT_ID>/`
- depot 選別: `--os <windows|macos|linux|all>`（既定: 実行中 OS）
- DLC / 共有 depot: 含める（public manifest が解決できないものは警告してスキップ）
- 版指定: `--manifest <DEPOT_ID>:<MANIFEST_ID>`（複数指定可）

## CLI

```
steam-vent-cdn [共通オプション] item <APP_ID> <ITEM_ID> [--list-versions|--timestamp|--manifest-id|--slug|--item-id]
steam-vent-cdn [共通オプション] app  <APP_ID> [--os <OS>] [--manifest <DEPOT_ID>:<MANIFEST_ID> ...]
```

共通オプション: `-o/--output`, `-u/--username`, `-p/--password`, `--parallel-download`,
`--server-mode`, `-v/--verbose`（`global = true` でサブコマンド前後どちらでも指定可）

破壊的変更のため README / examples を更新する。

## 実装

1. `src/downloader/vdf.rs`（新規）
   - 生バイナリ KeyValues のリーダ。型バイト 0x00-0x08 に対応。
   - 未知の型 / 切り詰め / 未消費の末尾データはエラー（フォールバックしない）。
   - 実測で PICS はテキスト VDF を返したため、テキスト VDF パーサも実装し、
     先頭バイトでテキスト/バイナリを自動判定する（末尾の NUL 終端も許容）。
2. `src/error.rs`
   - `VdfError` / `AppError` を追加し、`SteamVentError` に `#[from]` で組み込む。
3. `src/downloader/app_info.rs`（新規）
   - `CMsgClientPICSProductInfoRequest` を `job_with_retry` で送信し、`apps[].buffer` を
     VDF 解析して depot 一覧を得る。
   - `AppInfo { app_id, name, build_id, depots }` /
     `AppDepot { depot_id, manifest_id, size, oslist, dlc_app_id, shared_install }`
   - 取得元: `common.name`, `depots.branches.public.buildid`,
     `depots.<id>.manifests.public.gid/size`, `config.oslist`, `dlcappid`, `sharedinstall`
4. `src/downloader/manifest/fetch.rs`
   - `depot_id = app_id` 固定を撤廃し、`fetch_manifest_by_id(manifest_id, app_id, depot_id)`
     として depot を明示。Workshop 側は従来どおり `consumer_appid` を両方に渡す。
5. `src/cli/pipeline.rs`
   - `run_app_download_pipeline`: depot 選別 → manifest 取得 → depot 鍵 → チャンク DL。
6. `src/cli/args.rs` / `src/main.rs` / `src/cli/mod.rs`
   - サブコマンド分岐。
7. `examples/app_download.rs` + `Cargo.toml` 登録、`README.md` 更新。

## テスト

- `vdf.rs`: ネスト / 各型 / 壊れた入力でエラー
- `app_info.rs`: VDF フィクスチャ → `AppInfo`、oslist フィルタ、manifest 欠落 depot の扱い
- 既存テストが回帰しないこと

## 検証

- `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test`
- 匿名: 無料アプリで実ダウンロード確認
- ログイン: 所有アプリ（ユーザー環境）
- リリース前に依存の脆弱性スキャンを再実行

## 留意

- PICS `buffer` の実際のルートキーは実データで確認する。
- 未所有 DLC は public manifest が無くスキップされる（警告ログ）。
- `encryptedmanifests`（パスワード付きブランチ）は対象外。

## 追加: `--layout steam`（Steam クライアント互換）

- `--layout <depot|steam>`（既定 `depot`）、`--shared-depots <skip|auto>`（既定 `skip`）
- 出力: `steamapps/common/<INSTALLDIR>/`、`steamapps/workshop/content/<APP_ID>/<ITEM_ID>/`
- ACF: `appmanifest_<APP_ID>.acf` / `appworkshop_<APP_ID>.acf` を生成（既存ファイルはマージ）
- depot は mount order で上書きマージし、衝突は警告
- 共有 depot は既定でスキップし、取得用コマンドを表示。`auto` で所有アプリを自動取得
  （`depotfromapp` から解決し、所有アプリの ACF も生成）
- 実測で PICS はテキスト VDF を返すため、テキスト/バイナリを自動判定
