# tModLoader Workshop Downloader

tModLoaderの.tmodファイルをSteam WorkshopからダウンロードするRustツールです。

## 概要

このツールはSteam WorkshopからtModLoaderのModファイルをダウンロードするために作成されました。Steam Web APIまたはsteam-ventクレートを使用してSteamサーバーと通信し、Workshopアイテムの情報を取得してファイルをダウンロードします。

## 機能

- 🎮 Steam Workshopから.tmodファイルをダウンロード
- 📋 Modリストファイルによる一括ダウンロード対応
- 📊 ダウンロード進捗の表示
- 🔧 コマンドラインインターフェース

## インストール

### 前提条件

- Rust 1.70以上
- Cargo

### ビルド

```bash
cd tmodloader-downloader
cargo build --release
```

バイナリは `target/release/tmodloader-downloader` に生成されます。

## 使用方法

### 単一のModをダウンロード

```bash
# Published File IDを指定してダウンロード
./tmodloader-downloader --published-file-id 2835404321

# 出力ディレクトリを指定
./tmodloader-downloader --published-file-id 2835404321 --output ./my_mods
```

### 複数のModを一括ダウンロード

```bash
# mod_list.txtファイルを作成
echo "2835404321" > mods.txt
echo "2561094974" >> mods.txt

# リストファイルを指定してダウンロード
./tmodloader-downloader --mod-list mods.txt
```

### 詳細ログを表示

```bash
./tmodloader-downloader --published-file-id 2835404321 --verbose
```

## Published File IDの取得方法

Steam WorkshopのURLからPublished File IDを取得できます。

例: `https://steamcommunity.com/sharedfiles/filedetails/?id=2835404321`

この場合、`2835404321`がPublished File IDです。

## steam-ventクレートについて

`steam-vent`はSteamクライアントプロトコルを使用してSteamサーバーと通信するためのRustライブラリです。

### 基本的な使用方法

```rust
use steam_vent::connection::Connection;
use steam_vent::ServerList;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Steamサーバーを検出
    let server_list = ServerList::discover().await?;

    // 匿名接続
    let connection = Connection::anonymous(&server_list).await?;

    println!("Steamに接続しました！");

    Ok(())
}
```

### 認証を行う場合

```rust
use steam_vent::auth::AuthSession;
use steam_vent::connection::Connection;

async fn authenticate(
    username: &str,
    password: &str,
    steam_guard_code: Option<&str>,
) -> Result<Connection, Box<dyn std::error::Error>> {
    // サーバー検出
    let server_list = ServerList::discover().await?;

    // 認証セッション開始
    let auth_session = AuthSession::start(username, password, &server_list).await?;

    // Steam Guard処理
    let connection = if let Some(code) = steam_guard_code {
        auth_session.authenticate_with_code(code).await?
    } else {
        auth_session.authenticate().await?
    };

    Ok(connection)
}
```

### Workshop関連のプロトコル

`steam-vent-proto`クレートには以下のWorkshop関連モジュールが含まれています：

- `steammessages_workshop_steamclient` - Workshop操作用メッセージ
- `steammessages_publishedfile_steamclient` - 公開ファイル操作用メッセージ

```rust
use steam_vent_proto::steammessages_publishedfile_steamclient::*;

// Workshopファイルの詳細を取得
let request = CPublishedFile_GetDetails_Request {
    publishedfileids: vec![2835404321],
    includetags: true,
    ..Default::default()
};
```

## トラブルシューティング

### ダウンロードが失敗する場合

1. Published File IDが正しいか確認してください
2. ネットワーク接続を確認してください
3. Steam Workshopのサーバーが利用可能か確認してください

### 認証エラー

このツールは基本的に認証なしで動作しますが、一部のWorkshopアイテムは認証が必要な場合があります。その場合は `--api-key` オプションでSteam Web APIキーを指定してください。

## ライセンス

MIT License

## 参考リンク

- [steam-vent ドキュメント](https://docs.rs/steam-vent)
- [steam-vent GitHub](https://codeberg.org/steam-vent/steam-vent)
- [Steam Web API ドキュメント](https://steamcommunity.com/dev/apikey)
- [tModLoader](https://tmodloader.net/)
