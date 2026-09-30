# ADR-0012: Autostart と複数 Profile

- 状態: Accepted（2026-09-27）

## 決定

- **常に hidden で起動**し、メニューバーに常駐する。ウィンドウを開くのは「未登録のとき」「`--show`」
  「メニューの Open / Clipboard を送る / 設定」「通知クリック」「Finder 等から再度起動（Reopen）」のみ。
  これにより「ログイン時起動かどうか」を判定する必要がなく、自動起動時にウィンドウが出ることがない。
- **Autostart は `SMAppService.mainApp`（macOS 13+ の推奨 API）**。システム設定 > 一般 > ログイン項目に
  表示され、ユーザーは OS 設定・アプリ設定・メニューのどれからでも確認/変更できる。
  - tauri-plugin-autostart は LaunchAgent plist を `~/Library/LaunchAgents` に書く方式で、Apple の現行推奨ではないため使わない。
  - `SMAppService` は .app バンドルとして起動した場合のみ有効（開発時の素のバイナリでは "unavailable" と表示）。
- **自動起動は default プロファイルのみ**。`SMAppService.mainApp` は引数なしで起動するため。
  テスト用プロファイル（test-a 等）は手動 / スクリプトで `--profile` 付き起動する。
- **複数 Profile の分離**: `~/Library/Application Support/dev.tsute.desktop/profiles/<name>/` に
  設定・SQLite・ログ・一時ファイル・オートメーションソケットを分離。Keychain のアカウント名に profile と接続先を含める。
  受信フォルダも default 以外は `~/Downloads/Tsute-<name>` に分ける。
- **識別**: default 以外はメニューバーアイコン横にプロファイル名、ウィンドウタイトルと UI にバッジを表示。
- **二重起動防止**: プロファイルごとの `instance.lock` をファイルロック（`File::try_lock`）。
  同一プロファイルが 2 プロセスで動くと WebSocket・ダウンロードが競合するため。
- Windows では Run キー（HKCU）で自動起動し、データは `%LOCALAPPDATA%\dev.tsute.desktop\profiles\<name>\`（ADR-0016）。
