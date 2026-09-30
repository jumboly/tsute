自分の端末どうしで Clipboard の内容やファイルを、明示的な操作で受け渡すアプリです。
使い始める前に、自分の AWS に Backend をデプロイし、Enrollment Key を発行してください（[README](https://github.com/jumboly/tsute#readme)）。
このアプリには接続先 URL が埋め込まれていません。登録画面で、自分の環境の `APP_BASE_URL` を入力します。

## ダウンロード

| ファイル | 対象 |
|---|---|
| `Tsute-{{VERSION}}-macos-arm64.zip` | macOS（Apple Silicon）。Intel Mac では動きません |
| `Tsute-{{VERSION}}-windows-x64.zip` | Windows 10 / 11（x64）。WebView2 ランタイムが必要です |
| `SHA256SUMS.txt` | 各ファイルの SHA-256 |

iPhone / Android は、ブラウザで `<APP_BASE_URL>/app/` を開いて使います（ダウンロードは不要です）。

## インストール

このリリースのアプリは、配布用の署名をしていません（Mac は ad-hoc 署名、Windows は署名なし）。そのため、初回起動時に OS の警告が出ます。

**macOS**
1. zip を展開し、`Tsute.app` を「アプリケーション」フォルダに移します。
2. 開こうとすると「開けません」と表示されます。「システム設定」→「プライバシーとセキュリティ」を開き、`Tsute` の「このまま開く」を押します。
   ターミナルが使える場合は、`xattr -dr com.apple.quarantine /Applications/Tsute.app` を実行しても開けるようになります。
3. 起動するとメニューバーに常駐します。初回は Keychain へのアクセス許可を求められるので、「常に許可」を選びます。

**Windows**
1. zip を展開し、フォルダを `%LOCALAPPDATA%\Programs\tsute\` などに置いて `tsute.exe` を実行します。インストーラーはありません。
2. SmartScreen の警告が出たら、「詳細情報」→「実行」を押します。
3. 通知領域に常駐します。

## 変更点
