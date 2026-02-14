#!/usr/bin/env python3
"""
tModLoader Workshopモッド管理ユーティリティ

複数のWorkshopアイテムを一括ダウンロード・管理するためのツールセット

使用方法:
    # モッドリストをダウンロード
    python tmod_downloader_utils.py download --config mods.txt --output ./mods

    # 特定のモッドを検索（WebAPIを使用）
    python tmod_downloader_utils.py search --query "calamity"

    # 購読中のモッド一覧を取得
    python tmod_downloader_utils.py subscribed --username myuser
"""

import os
import sys
import json
import argparse
from pathlib import Path
from typing import List, Dict, Optional
import time

# geventパッチ
from gevent import monkey
monkey.patch_all()

from steam.client import SteamClient
from steam.client.cdn import CDNClient
from steam.enums import EResult
from steam.webapi import WebAPI


class TModWorkshopManager:
    """tModLoader Workshop管理クラス"""

    TMODLOADER_APP_ID = 1281930

    def __init__(self, steam_api_key: str = None):
        """
        初期化

        Args:
            steam_api_key: Steam Web APIキー（オプション）
        """
        self.steam_client = SteamClient()
        self.cdn_client = None
        self.api_key = steam_api_key
        self.web_api = None
        self._logged_in = False

    def login_anonymous(self) -> bool:
        """匿名ログイン"""
        print("匿名ユーザーとして接続中...")
        result = self.steam_client.anonymous_login()

        if result == EResult.OK:
            print("匿名ログイン成功！")
            self.cdn_client = CDNClient(self.steam_client)
            self._logged_in = True
            return True
        else:
            print(f"匿名ログイン失敗: {result}")
            return False

    def login(self, username: str, password: str,
              steam_guard_code: str = None,
              email_code: str = None) -> bool:
        """
        Steamアカウントでログイン

        Args:
            username: Steamユーザー名
            password: Steamパスワード
            steam_guard_code: Steam Guardコード（必要な場合）
            email_code: メール認証コード（必要な場合）
        """
        print(f"ユーザー '{username}' として接続中...")

        # Steam Guardコールバックを設定
        def steam_guard_prompt(is_2fa: bool) -> str:
            if is_2fa:
                code = steam_guard_code or input("Steam Guard コードを入力: ")
            else:
                code = email_code or input("メール認証コードを入力: ")
            return code.strip()

        self.steam_client.on('steamguard', steam_guard_prompt)

        result = self.steam_client.login(username, password)

        if result == EResult.OK:
            print("ログイン成功！")
            self.cdn_client = CDNClient(self.steam_client)

            # WebAPIを初期化
            if self.api_key:
                self.web_api = WebAPI(key=self.api_key)

            self._logged_in = True
            return True
        else:
            print(f"ログイン失敗: {result}")
            return False

    def logout(self):
        """ログアウト"""
        if self.steam_client and self._logged_in:
            self.steam_client.logout()
            self._logged_in = False
            print("ログアウトしました。")

    def download_mod(self, workshop_id: int, output_dir: str,
                     file_filter: str = ".tmod",
                     show_progress: bool = True) -> Dict:
        """
        単一のモッドをダウンロード

        Args:
            workshop_id: WorkshopアイテムID
            output_dir: 出力ディレクトリ
            file_filter: ファイル拡張子フィルター
            show_progress: 進捗を表示するか

        Returns:
            ダウンロード結果の辞書
        """
        if not self._logged_in:
            raise RuntimeError("ログインが必要です")

        result = {
            'workshop_id': workshop_id,
            'success': False,
            'files': [],
            'error': None
        }

        try:
            if show_progress:
                print(f"\n[Workshop ID: {workshop_id}]")
                print("  マニフェスト取得中...")

            manifest = self.cdn_client.get_manifest_for_workshop_item(workshop_id)

            if show_progress:
                print(f"  マニフェスト名: {manifest.name}")

            output_path = Path(output_dir)
            output_path.mkdir(parents=True, exist_ok=True)

            # ファイルをフィルタリングしてリスト化（iter_filesを使用）
            all_files = list(manifest.iter_files())
            target_files = [f for f in all_files
                          if not file_filter or f.filename.endswith(file_filter)]

            if show_progress:
                print(f"  対象ファイル: {len(target_files)}")

            for i, file in enumerate(target_files, 1):
                filename = file.filename.replace('\\', '/').lstrip('/')

                if show_progress:
                    size_mb = file.size / (1024 * 1024)
                    print(f"  [{i}/{len(target_files)}] {filename} ({size_mb:.2f} MB)")

                try:
                    file_data = file.read()

                    file_path = output_path / filename
                    file_path.parent.mkdir(parents=True, exist_ok=True)

                    with open(file_path, 'wb') as f:
                        f.write(file_data)

                    result['files'].append({
                        'path': str(file_path),
                        'filename': filename,
                        'size': file.size
                    })

                except Exception as e:
                    print(f"    エラー: {e}")

            result['success'] = len(result['files']) > 0

        except Exception as e:
            result['error'] = str(e)
            print(f"  エラー: {e}")

        return result

    def download_mods_batch(self, workshop_ids: List[int], output_dir: str,
                           file_filter: str = ".tmod",
                           continue_on_error: bool = True) -> List[Dict]:
        """
        複数のモッドを一括ダウンロード

        Args:
            workshop_ids: WorkshopアイテムIDのリスト
            output_dir: 出力ディレクトリ
            file_filter: ファイル拡張子フィルター
            continue_on_error: エラー時に継続するか

        Returns:
            各ダウンロード結果のリスト
        """
        print(f"\n一括ダウンロード開始: {len(workshop_ids)} アイテム")
        print("=" * 50)

        results = []
        total_files = 0

        for i, workshop_id in enumerate(workshop_ids, 1):
            print(f"\n進捗: {i}/{len(workshop_ids)}")

            try:
                result = self.download_mod(
                    workshop_id=workshop_id,
                    output_dir=output_dir,
                    file_filter=file_filter
                )
                results.append(result)
                total_files += len(result['files'])

            except Exception as e:
                results.append({
                    'workshop_id': workshop_id,
                    'success': False,
                    'files': [],
                    'error': str(e)
                })

                if not continue_on_error:
                    break

        print("\n" + "=" * 50)
        print(f"一括ダウンロード完了！")
        print(f"  成功: {sum(1 for r in results if r['success'])}/{len(results)}")
        print(f"  総ファイル数: {total_files}")

        return results

    def get_workshop_item_details(self, workshop_id: int) -> Optional[Dict]:
        """
        Workshopアイテムの詳細を取得

        Args:
            workshop_id: WorkshopアイテムID

        Returns:
            アイテム詳細の辞書
        """
        try:
            # SteamのUM (Universe Message) APIを使用
            result = self.steam_client.send_um_and_wait(
                'PublishedFile.GetDetails#1',
                {
                    'publishedfileids': [workshop_id],
                    'includetags': True,
                    'includeadditionalpreviews': False,
                    'includechildren': False,
                    'includekvcache': False,
                }
            )

            if result and hasattr(result, 'body'):
                details = result.body.publishedfiledetails[0]

                return {
                    'workshop_id': workshop_id,
                    'title': details.title if hasattr(details, 'title') else 'Unknown',
                    'description': details.description if hasattr(details, 'description') else '',
                    'creator_app_id': details.creator_appid if hasattr(details, 'creator_appid') else 0,
                    'consumer_app_id': details.consumer_appid if hasattr(details, 'consumer_appid') else 0,
                    'file_size': details.file_size if hasattr(details, 'file_size') else 0,
                    'filename': details.filename if hasattr(details, 'filename') else '',
                    'creator_steam_id': str(details.creator) if hasattr(details, 'creator') else '',
                    'views': details.views if hasattr(details, 'views') else 0,
                    'subscriptions': details.subscriptions if hasattr(details, 'subscriptions') else 0,
                    'favorited': details.favorited if hasattr(details, 'favorited') else 0,
                }

        except Exception as e:
            print(f"アイテム詳細取得エラー: {e}")

        return None

    def list_mod_files(self, workshop_id: int,
                       file_filter: str = ".tmod") -> List[Dict]:
        """
        モッド内のファイル一覧を取得（ダウンロードなし）

        Args:
            workshop_id: WorkshopアイテムID
            file_filter: ファイル拡張子フィルター

        Returns:
            ファイル情報のリスト
        """
        if not self._logged_in:
            raise RuntimeError("ログインが必要です")

        manifest = self.cdn_client.get_manifest_for_workshop_item(workshop_id)

        files = []
        # iter_files()を使用してファイルを反復処理
        for file in manifest.iter_files():
            if not file_filter or file.filename.endswith(file_filter):
                files.append({
                    'filename': file.filename.replace('\\', '/').lstrip('/'),
                    'size': file.size,
                    'size_human': self._human_readable_size(file.size)
                })

        return files

    @staticmethod
    def _human_readable_size(size: int) -> str:
        """バイトサイズを人間が読める形式に変換"""
        for unit in ['B', 'KB', 'MB', 'GB']:
            if size < 1024:
                return f"{size:.2f} {unit}"
            size /= 1024
        return f"{size:.2f} TB"


def parse_mods_file(filepath: str) -> List[int]:
    """
    モッドリストファイルをパース

    ファイル形式（テキスト）:
        # コメント行
        2824688072
        2905886296
        # Calamity Mod
        2824688072

    Args:
        filepath: モッドリストファイルのパス

    Returns:
        Workshop IDのリスト
    """
    workshop_ids = []

    with open(filepath, 'r', encoding='utf-8') as f:
        for line in f:
            line = line.strip()

            # 空行とコメントをスキップ
            if not line or line.startswith('#'):
                continue

            # 数値を抽出
            try:
                workshop_id = int(line.split()[0])
                workshop_ids.append(workshop_id)
            except ValueError:
                print(f"警告: 無効な行をスキップ: {line}")

    return workshop_ids


def main():
    """メイン関数"""
    parser = argparse.ArgumentParser(
        description='tModLoader Workshopモッド管理ユーティリティ'
    )

    subparsers = parser.add_subparsers(dest='command', help='利用可能なコマンド')

    # download サブコマンド
    download_parser = subparsers.add_parser('download', help='モッドをダウンロード')
    download_parser.add_argument('--config', '-c', type=str,
                                help='モッドリストファイル')
    download_parser.add_argument('--ids', '-i', type=str,
                                help='Workshop ID（カンマ区切り）')
    download_parser.add_argument('--output', '-o', type=str, default='./mods',
                                help='出力ディレクトリ')
    download_parser.add_argument('--filter', '-f', type=str, default='.tmod',
                                help='ファイルフィルター')
    download_parser.add_argument('--username', '-u', type=str,
                                help='Steamユーザー名')
    download_parser.add_argument('--password', '-p', type=str,
                                help='Steamパスワード')

    # list サブコマンド
    list_parser = subparsers.add_parser('list', help='モッドのファイル一覧を表示')
    list_parser.add_argument('--id', '-i', type=int, required=True,
                            help='Workshop ID')
    list_parser.add_argument('--filter', '-f', type=str, default='.tmod',
                            help='ファイルフィルター')
    list_parser.add_argument('--username', '-u', type=str,
                            help='Steamユーザー名')
    list_parser.add_argument('--password', '-p', type=str,
                            help='Steamパスワード')

    # info サブコマンド
    info_parser = subparsers.add_parser('info', help='モッドの詳細情報を表示')
    info_parser.add_argument('--id', '-i', type=int, required=True,
                            help='Workshop ID')
    info_parser.add_argument('--username', '-u', type=str,
                            help='Steamユーザー名')
    info_parser.add_argument('--password', '-p', type=str,
                            help='Steamパスワード')

    args = parser.parse_args()

    if not args.command:
        parser.print_help()
        return

    # マネージャーを初期化
    manager = TModWorkshopManager()

    try:
        # ログイン
        if args.username and args.password:
            if not manager.login(args.username, args.password):
                print("ログイン失敗")
                sys.exit(1)
        else:
            if not manager.login_anonymous():
                print("匿名ログイン失敗")
                sys.exit(1)

        # コマンドを実行
        if args.command == 'download':
            # Workshop IDを収集
            workshop_ids = []

            if args.config:
                workshop_ids.extend(parse_mods_file(args.config))

            if args.ids:
                for id_str in args.ids.split(','):
                    workshop_ids.append(int(id_str.strip()))

            if not workshop_ids:
                print("エラー: Workshop IDを指定してください")
                sys.exit(1)

            # 重複を削除
            workshop_ids = list(set(workshop_ids))

            # ダウンロード実行
            results = manager.download_mods_batch(
                workshop_ids=workshop_ids,
                output_dir=args.output,
                file_filter=args.filter
            )

            # 結果をJSONファイルに保存
            result_file = Path(args.output) / 'download_results.json'
            with open(result_file, 'w', encoding='utf-8') as f:
                json.dump(results, f, ensure_ascii=False, indent=2)
            print(f"\n結果を保存: {result_file}")

        elif args.command == 'list':
            files = manager.list_mod_files(
                workshop_id=args.id,
                file_filter=args.filter
            )

            print(f"\nWorkshop ID: {args.id}")
            print(f"ファイル数: {len(files)}")
            print("-" * 50)

            for file in files:
                print(f"  {file['filename']} ({file['size_human']})")

        elif args.command == 'info':
            details = manager.get_workshop_item_details(args.id)

            if details:
                print(f"\nWorkshop ID: {details['workshop_id']}")
                print(f"タイトル: {details['title']}")
                print(f"ファイルサイズ: {TModWorkshopManager._human_readable_size(details['file_size'])}")
                print(f"閲覧数: {details['views']}")
                print(f"購読数: {details['subscriptions']}")
                print(f"お気に入り: {details['favorited']}")
                print(f"ファイル名: {details['filename']}")

                if details['description']:
                    print(f"\n説明:\n{details['description'][:500]}...")
            else:
                print("アイテム情報を取得できませんでした")

    except KeyboardInterrupt:
        print("\n\nキャンセルされました")
    except Exception as e:
        print(f"\nエラー: {e}")
        import traceback
        traceback.print_exc()
    finally:
        manager.logout()


if __name__ == '__main__':
    main()
