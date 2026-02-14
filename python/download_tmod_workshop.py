#!/usr/bin/env python3
"""
Steam WorkshopからtModLoaderの.tmodファイルをダウンロードするスクリプト

ValvePython/steamライブラリを使用してSteam Workshopから
tModLoaderモッドをダウンロードします。

使用方法:
    python download_tmod_workshop.py --item-id <workshop_item_id> --output <output_directory>

例:
    python download_tmod_workshop.py --item-id 2824688072 --output ./mods
"""

import os
import sys
import argparse
from pathlib import Path

# geventのパッチを適用（steamライブラリの要件）
from gevent import monkey
monkey.patch_all()

from steam.client import SteamClient
from steam.client.cdn import CDNClient
from steam.enums import EResult
from steam.enums.common import EPersonaState


class TModDownloader:
    """tModLoader Workshopモッドダウンローダー"""

    # tModLoaderのSteam App ID
    TMODLOADER_APP_ID = 1281930

    def __init__(self, username: str = None, password: str = None):
        """
        初期化

        Args:
            username: Steamユーザー名（匿名の場合はNone）
            password: Steamパスワード（匿名の場合はNone）
        """
        self.steam_client = SteamClient()
        self.cdn_client = None
        self.username = username
        self.password = password

    def login(self) -> bool:
        """
        Steamにログイン

        Returns:
            ログインが成功したかどうか
        """
        print("Steamに接続中...")

        if self.username and self.password:
            # 認証付きログイン
            print(f"ユーザー '{self.username}' としてログイン中...")
            result = self.steam_client.login(self.username, self.password)

            if result == EResult.OK:
                print("ログイン成功！")
                self.cdn_client = CDNClient(self.steam_client)
                return True
            else:
                print(f"ログイン失敗: {result}")
                return False
        else:
            # 匿名ログイン
            print("匿名ユーザーとしてログイン中...")
            result = self.steam_client.anonymous_login()


            if result == EResult.OK:
                print("匿名ログイン成功！")
                print(f"steam_id: {self.steam_client.steam_id}")
                self.cdn_client = CDNClient(self.steam_client)
                return True
            else:
                print(f"匿名ログイン失敗: {result}")
                return False

    def logout(self):
        """Steamからログアウト"""
        if self.steam_client:
            self.steam_client.logout()
            print("ログアウトしました。")

    def get_workshop_item_info(self, item_id: int) -> dict:
        """
        Workshopアイテムの情報を取得

        Args:
            item_id: WorkshopアイテムID

        Returns:
            アイテム情報の辞書
        """
        # WebAPIを使用してWorkshopアイテムの詳細を取得
        from steam.webapi import WebAPI

        api = WebAPI(key=None)  # APIキーなしで使用

        try:
            # ISteamRemoteStorage/GetPublishedFileDetailsを使用
            result = self.steam_client.send_um_and_wait(
                'PublishedFile.GetDetails#1',
                {'publishedfileids': [item_id], 'includetags': True, 'includeadditionalpreviews': False}
            )

            if result and 'publishedfiledetails' in result.body:
                details = result.body['publishedfiledetails'][0]
                return {
                    'title': details.get('title', 'Unknown'),
                    'description': details.get('description', ''),
                    'creator': details.get('creator', ''),
                    'file_size': details.get('file_size', 0),
                    'filename': details.get('filename', ''),
                }
        except Exception as e:
            print(f"アイテム情報の取得中にエラー: {e}")

        return {}

    def download_workshop_item(self, item_id: int, output_dir: str = "./mods",
                                file_filter: str = ".tmod") -> list:
        """
        Workshopアイテムからファイルをダウンロード

        Args:
            item_id: WorkshopアイテムID
            output_dir: 出力ディレクトリ
            file_filter: ファイルフィルター（拡張子）

        Returns:
            ダウンロードされたファイルのパスのリスト
        """
        if not self.cdn_client:
            raise RuntimeError("ログインしてください。")

        print(f"\nWorkshopアイテム {item_id} のマニフェストを取得中...")

        try:
            # Workshopアイテムのマニフェストを取得
            manifest = self.cdn_client.get_manifest_for_workshop_item(item_id)

            print(f"マニフェスト取得成功！")
            print(f"  アプリID: {manifest.app_id}")
            print(f"  デポットID: {manifest.depot_id}")
            print(f"  マニフェスト名: {manifest.name}")

        except Exception as e:
            print(f"マニフェスト取得エラー: {e}")
            return []

        # 出力ディレクトリを作成
        output_path = Path(output_dir)
        output_path.mkdir(parents=True, exist_ok=True)

        downloaded_files = []

        # .tmodファイルを検索してダウンロード
        print(f"\n'{file_filter}' ファイルを検索中...")

        # iter_files()を使用してファイルを反復処理
        for file in manifest.iter_files():
            filename = file.filename

            # ファイルフィルターで絞り込み
            if file_filter and not filename.endswith(file_filter):
                continue

            print(f"\nダウンロード中: {filename}")
            print(f"  サイズ: {file.size} bytes")

            try:
                # ファイルを読み込み
                file_data = file.read()

                # 出力パスを構築
                # Windowsスタイルのパスセパレータを処理
                relative_path = filename.replace('\\', '/').lstrip('/')
                file_output_path = output_path / relative_path

                # 親ディレクトリを作成
                file_output_path.parent.mkdir(parents=True, exist_ok=True)

                # ファイルを書き込み
                with open(file_output_path, 'wb') as f:
                    f.write(file_data)

                print(f"  保存先: {file_output_path}")
                downloaded_files.append(str(file_output_path))

            except Exception as e:
                print(f"  エラー: {e}")
                continue

        return downloaded_files

    def list_workshop_files(self, item_id: int, file_filter: str = ".tmod") -> list:
        """
        Workshopアイテムのファイル一覧を表示（ダウンロードなし）

        Args:
            item_id: WorkshopアイテムID
            file_filter: ファイルフィルター（拡張子）

        Returns:
            ファイル情報のリスト
        """
        if not self.cdn_client:
            raise RuntimeError("ログインしてください。")

        print(f"\nWorkshopアイテム {item_id} のマニフェストを取得中...")

        try:
            manifest = self.cdn_client.get_manifest_for_workshop_item(item_id)
            print(f"マニフェスト取得成功！")
            print(f"  マニフェスト名: {manifest.name}")

        except Exception as e:
            print(f"マニフェスト取得エラー: {e}")
            return []

        matching_files = []

        print(f"\n'{file_filter}' ファイル一覧:")

        # iter_files()を使用してファイルを反復処理
        for file in manifest.iter_files():
            filename = file.filename

            if file_filter and not filename.endswith(file_filter):
                continue

            # ファイルサイズを人間が読みやすい形式に変換
            size = file.size
            if size < 1024:
                size_str = f"{size} B"
            elif size < 1024 * 1024:
                size_str = f"{size / 1024:.2f} KB"
            else:
                size_str = f"{size / (1024 * 1024):.2f} MB"

            print(f"  - {filename} ({size_str})")
            matching_files.append({
                'filename': filename,
                'size': file.size,
            })

        return matching_files


def main():
    """メイン関数"""
    parser = argparse.ArgumentParser(
        description='Steam WorkshopからtModLoaderの.tmodファイルをダウンロード',
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
使用例:
  # 匿名でWorkshopアイテムのファイル一覧を表示
  python download_tmod_workshop.py --item-id 2824688072 --list

  # .tmodファイルをダウンロード
  python download_tmod_workshop.py --item-id 2824688072 --output ./mods

  # 特定のファイル拡張子でフィルタ
  python download_tmod_workshop.py --item-id 2824688072 --filter .tmod

  # Steamアカウントでログインしてダウンロード
  python download_tmod_workshop.py --item-id 2824688072 --username myuser --password mypass

注意:
  - 一部のWorkshopアイテムは匿名アクセスできない場合があります
  - その場合は--usernameと--passwordオプションを使用してください

tModLoader Workshop: https://steamcommunity.com/app/1281930/workshop/
        """
    )

    parser.add_argument(
        '--item-id', '-i',
        type=int,
        required=True,
        help='WorkshopアイテムID（URLから取得可能）'
    )

    parser.add_argument(
        '--output', '-o',
        type=str,
        default='./mods',
        help='出力ディレクトリ（デフォルト: ./mods）'
    )

    parser.add_argument(
        '--filter', '-f',
        type=str,
        default='.tmod',
        help='ファイルフィルター/拡張子（デフォルト: .tmod）'
    )

    parser.add_argument(
        '--username', '-u',
        type=str,
        default=None,
        help='Steamユーザー名（省略時は匿名ログイン）'
    )

    parser.add_argument(
        '--password', '-p',
        type=str,
        default=None,
        help='Steamパスワード'
    )

    parser.add_argument(
        '--list', '-l',
        action='store_true',
        help='ファイル一覧のみ表示（ダウンロードしない）'
    )

    args = parser.parse_args()

    # ダウンローダーを初期化
    downloader = TModDownloader(
        username=args.username,
        password=args.password
    )

    try:
        # ログイン
        if not downloader.login():
            print("ログインに失敗しました。終了します。")
            sys.exit(1)

        if args.list:
            # ファイル一覧のみ表示
            files = downloader.list_workshop_files(
                item_id=args.item_id,
                file_filter=args.filter
            )
            print(f"\n合計 {len(files)} ファイルが見つかりました。")
        else:
            # ファイルをダウンロード
            downloaded = downloader.download_workshop_item(
                item_id=args.item_id,
                output_dir=args.output,
                file_filter=args.filter
            )
            print(f"\nダウンロード完了！ {len(downloaded)} ファイルを保存しました。")
            print(f"保存先: {args.output}")

    except KeyboardInterrupt:
        print("\n\nキャンセルされました。")
    except Exception as e:
        print(f"\nエラーが発生しました: {e}")
        import traceback
        traceback.print_exc()
    finally:
        # ログアウト
        downloader.logout()


if __name__ == '__main__':
    main()
