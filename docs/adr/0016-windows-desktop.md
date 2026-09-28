# ADR-0016: Windows デスクトップ統合（Phase 2）

- 状態: Accepted（2026-09-28）。CI（windows-latest）でビルド・テスト。**Windows 実機での操作確認は未実施**。

## 背景

Phase 1 の macOS 版と同じ `tsute-client-core`（認証・転送・再開）と UI（素の HTML/JS）をそのまま使い、
OS に依存する部分だけを Windows 用に足す（ADR-0001）。PHASE1_REPORT の「Windows 対応時の注意点」を起点にした。
OS 統合は `tsute-os` に閉じ込め、デスクトップ側はなるべく `#[cfg]` を増やさない。

## 決定

### Clipboard（ADR-0003 の Windows 版）

Send Clipboard 押下時に一度だけ `OpenClipboard` して読む（監視はしない）。所有者にはメッセージ専用ウィンドウを使う
（`OpenClipboard(NULL)` のあとの `EmptyClipboard` では `SetClipboardData` が失敗し得るため）。
他アプリが一瞬開いていることがあるので、15ms 間隔で 20 回まで再試行する。

読み取りの優先順:
1. `CF_HDROP`（Explorer でのコピー）。1 件で拡張子が動画 → **Video**、画像 → **Image**（元ファイルのまま）、
   それ以外・複数 → **Files**。Windows には UTType がないので、種類は拡張子の表（`media_type_for_path`）で決める。
2. `CF_UNICODETEXT` → **Text**。CRLF は LF にそろえる（受信側の OS に依らず同じ内容にするため）。
3. 登録形式 `"PNG"`（Office・ブラウザ。アルファ付き）→ **Image**。なければ `CF_DIB` を PNG に変換
   （PrintScreen 等）。CF_DIBV5 は OS が CF_DIB を合成するので読まない。

書き込み（ユーザーが「Clipboard にコピー」を押した時だけ）:
- Text: `CF_UNICODETEXT`。LF は CRLF にする（古いアプリで 1 行につながらないように）。
- Image: `"PNG"` と `CF_DIB`（24bpp、透明部分は白と合成）の両方。CF_DIB しか読まないアプリが多く、
  32bpp のアルファを正しく扱わないアプリでは透明部分が黒く潰れるため。
- Video / Files: `CF_HDROP` + `Preferred DropEffect = COPY`（貼り付け先で「移動」になり受信フォルダから消えないように）。

DIB ⇔ PNG の変換は OS の WIC を使わず純 Rust（`png` crate + 自前の DIB 解析, `crates/os/src/dib.rs`）。
変換ロジックを macOS 上の単体テストでも検証でき、COM の初期化状態に依存しないため。
対応は 1/4/8/16/24/32bpp の BI_RGB と BITFIELDS。RLE・JPEG 埋め込みの DIB は未対応（まれ）。
32bpp BI_RGB の 4 バイト目が全画素 0 のときは不透明とみなす（未使用扱いのアプリが多いため）。

動画の解像度・長さ・サムネイルは Windows では未取得（None）。表示用の補助情報で送信は妨げない。
取るなら Media Foundation / `IShellItemImageFactory`（将来）。Clipboard 上の動画の実データ表現は Windows では一般的でないため扱わない。

### 通知

WinRT の `ToastNotification`。MSIX でないアプリが Toast を出すには AppUserModelID が必要なので、
起動時に `HKCU\Software\Classes\AppUserModelId\dev.tsute.desktop`（DisplayName=つて）を登録し、
`SetCurrentProcessExplicitAppUserModelID` を呼ぶ。クリックは `Activated` イベントで受け、macOS と同じく
該当の受信にフォーカスする（プロセス常駐中のみ。COM Activator は作らない）。表示した通知は直近 20 件を保持する
（破棄すると Activated が呼ばれないため）。本文は macOS と同じく種類と大きさだけ。
ADR-0014 の「未確認印」は Windows でも併用する（通知を OS 設定で切っていても気づけるように）。

### 自動起動（ADR-0012 の Windows 版）

`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` の値 `Tsute` に exe のパス（引数なし = default プロファイル）。
管理者権限が不要で、「設定 > アプリ > スタートアップ」とタスク マネージャーに表示され、ユーザーは OS 側でも無効にできる。
- OS 側で無効にした状態（`Explorer\StartupApproved\Run` の先頭バイトが奇数）は `disabled_by_user` と表示する。
  アプリの設定で明示的にオンにしたときはその記録を消して有効に戻す（ユーザーの明示操作のため）。
- 値が別の場所の exe を指す（移動・再インストール前）なら `not_registered` とみなし、オンにし直すと上書きする。
- タスク スケジューラ・スタートアップ フォルダのショートカットは使わない（前者は権限・設定が重く、後者は Run キーと同等で利点がない）。

### Credential Storage（ADR-0011 の Windows 版）

Windows 資格情報マネージャーの汎用資格情報（`CredWriteW`）。Target 名は `dev.tsute.desktop|<profile>|<base_url>|<endpoint_id>`。
中身は OS が DPAPI でユーザーごとに暗号化する。Persist は `CRED_PERSIST_LOCAL_MACHINE`
（`ENTERPRISE` だと移動プロファイルで他の PC に渡り、同じ Endpoint が 2 台で動くため）。
`FileSecretStore` は Windows でも `--insecure-file-credentials` のときだけ（既定にはしない）。

### データの置き場所

`%LOCALAPPDATA%\dev.tsute.desktop\profiles\<name>\`（PHASE1_REPORT では %APPDATA% としていたが変更）。
Roaming（%APPDATA%）だと設定・DB・転送中データがドメインの移動プロファイルで他の PC に複製され、
Endpoint の同一性が崩れるため。受信フォルダは macOS と同じく `%USERPROFILE%\Downloads\Tsute(-<name>)`。

### 常駐 UI

- 通知領域（タスクトレイ）。Windows の慣習に合わせ、**左クリックでウィンドウを開き、右クリックでメニュー**
  （macOS は左クリックでメニュー）。メニューの構成は macOS と同じ。
- アイコンはテンプレート画像（OS による自動着色）がないため、タスクバーの明暗
  （`Themes\Personalize\SystemUsesLightTheme`）で黒/白を選ぶ。状態が変わるたびに選び直す。
- 通知領域には文字を出せないため、default 以外のプロファイルはツールチップのプロファイル名で区別する。
- リリースビルドは `windows_subsystem = "windows"`（起動時にコンソールを出さない）。

### オートメーション（ADR-0013 の Windows 版）

Unix ソケットの代わりに名前付きパイプ `\\.\pipe\tsute-auto-<プロファイルのパスのハッシュ>`。
パイプ名はこれまでどおり `<profile>/automation.sock.path` に書く（ドライバの探し方を OS で変えないため）。
既定のセキュリティ記述子では書き込み（= コマンド送信）ができるのは作成ユーザー・管理者・SYSTEM のみ。
リモート接続は拒否し、最初のインスタンスは `first_pipe_instance` で他プロセスの先取りを防ぐ。
`--automation` と `TSUTE_AUTOMATION=1` の両方が必要な点は変わらない。Windows 用の E2E ドライバは未作成。

### ビルド・配布

- CI: `windows-latest` で clippy / テスト（`tsute-server-lambda` を除くワークスペース）と、実 Clipboard テスト
  （ランナーは使い捨てなので `#[ignore]` のテストも回す）。資格情報マネージャーの読み書きテストも CI で実行する。
- **配布はフォルダコピー（インストーラーなし）**（2026-09-29 ユーザー判断）。`desktop.yml` は `cargo tauri build --no-bundle`
  で `tsute.exe` を作り、フォルダごと artifact `Tsute-windows` にする。画面は exe に埋め込まれ、データ・資格情報・
  レジストリ（通知用 AUMID・自動起動）はアプリが初回起動時・設定変更時に自分で作るため、インストーラーでしかできない作業がない。
  - VC++ ランタイムは静的リンク（tauri-build の既定）。CI で `dumpbin /dependents` を見て、vcruntime・msvcp・
    WebView2Loader の DLL に依存していないことを確かめる（依存していたら失敗させる）。
  - 前提: WebView2 ランタイム（Windows 11・更新済みの Windows 10 には標準で入っている）。インストーラーがないので自動導入はされない。
  - スタート メニューのショートカットは作られない（必要ならユーザーが作る）。
  - 自動起動は exe のパスを登録するので、フォルダを移動したら設定でオンにし直す（移動前のパスは `not_registered` と表示）。
  - アンインストール = フォルダを削除。ただしフォルダ外に残るもの: `%LOCALAPPDATA%\dev.tsute.desktop`（設定・履歴）、
    資格情報マネージャーの `dev.tsute.desktop|…`、HKCU の `Software\Classes\AppUserModelId\dev.tsute.desktop` と
    Run の `Tsute`（設定でオフにすれば消える）。登録解除（設定の「登録を解除」）を先にすると Endpoint 鍵も消える。
- アイコンは `icons/icon.ico`（`icon.png` から生成。exe のリソースに埋め込まれる）。
- **コード署名なし**（SmartScreen の警告が出る）。配布するなら署名証明書が必要（新たな契約になるのでユーザー判断）。

## 未解決・次の作業

- Windows 実機での確認（Clipboard 各形式・通知の表示とクリック・自動起動・トレイ操作・Explorer からの D&D・資格情報）。
- 起動中に同じプロファイルをもう一度起動したとき（スタート メニューから再度開く等）、macOS の Reopen に当たる
  「既存ウィンドウを開く」ができない（今は二重起動防止で黙って終了する）。既存プロセスへの通知経路が必要。
- 動画のメタデータ・サムネイル、Windows 用の E2E ドライバ、コード署名。
