# Phase 1（macOS）完了報告 — 2026-09-27

## 完了条件の達成状況

**同じ Mac 上で 2 つの独立 Endpoint（test-a / test-b）を起動し、実際のアプリ UI 操作で Clipboard とファイルを
AWS（CloudFront → API Gateway / Lambda / DynamoDB / S3）経由で転送できることを確認した。**
`python3 e2e/run_e2e.py --target cloud --env test` → 14/14 PASS。

注意: UI 操作はアプリ内オートメーション経路で実際の DOM ボタンを押して行った（ADR-0013）。
Finder からの実ドラッグ、メニューバーのクリック、OS 通知の表示、実ログイン時の自動起動は手動確認が必要。

## Architecture

```
Desktop (Tauri 2, menu bar resident)            AWS (test: ap-northeast-1 / edge: us-east-1)
 ├ UI: plain HTML/JS (WebView, destroyed on close)   CloudFront (single FQDN)
 ├ tsute-os: NSPasteboard/AVFoundation/UN/SMApp      ├ /        → S3 (blog, OAC)
 └ tsute-client-core ──HTTPS──────────────────────▶ ├ /api/*   → API GW HTTP API → Lambda(api)
      auth / transfer engine / SQLite  ──WSS──────▶ └ /ws      → API GW WebSocket(stage ws) → Lambda(api)
      presigned PUT/GET ─────────────────────────▶ S3 transfers/<id>/<file>/<chunk> (Lifecycle 8d)
                                                    DynamoDB single table (TTL)
 admin: aws lambda invoke (IAM) ─────────────────▶ Lambda(admin): enrollment keys / revoke
```

## 採用技術

Rust（全コンポーネント）、Tauri 2.12、objc2 系（AppKit/AVFoundation/UserNotifications/ServiceManagement）、
security-framework（Keychain）、tokio / reqwest / tokio-tungstenite、rusqlite、ed25519-dalek、
AWS: CloudFront / API Gateway HTTP+WebSocket / Lambda（Rust, arm64）/ DynamoDB / S3、CloudFormation、GitHub Actions + OIDC、
Blog: Astro 7（別リポジトリ ~/src/tsute-blog）。

## ADR 一覧

docs/adr/README.md（0001–0013）。

## 実施したテストと結果

| テスト | 結果 |
|---|---|
| `cargo test --workspace`（結合 10 / server ルール 4 / proto 2 / メディア 1） | PASS |
| 実 OS Clipboard（6 件, `--ignored`） | PASS |
| デスクトップ E2E ローカル（debug / release .app） | 14/14 PASS |
| **デスクトップ E2E クラウド（test 環境）** | **14/14 PASS** |
| .app 統合（Keychain / ログイン項目 / Idle） | PASS（通知は許可されず要手動確認） |
| clippy -D warnings / fmt / cfn-lint | PASS |

クラウド検証で見つけて修正した不具合: 0 バイトファイルの presigned PUT が署名不一致（Content-Length 未送信）。
ローカル開発サーバーも Content-Length を検証するようにして再発を検出できるようにした。

## 未解決事項

1. OS 通知: ad-hoc 署名では macOS が通知を拒否する（UNError code 1）。代替としてメニューバーアイコンの未確認印を実装（ADR-0014）。OS 通知は Developer ID 署名時に有効化される。
2. 手動確認項目: Finder からの実ドラッグ、メニューバー操作、「保存…」ダイアログ、実ログインでの hidden 起動。
3. GitHub リポジトリ未作成のため CI / CD / Blog CI は未実行（OIDC ロールはデプロイ済み）。
4. 配布には Developer ID 署名・公証が必要（現状 ad-hoc）。再ビルド後は Keychain 許可ダイアログが出る。
5. フォルダ Drop 未対応。chunk size / 並列数の実回線ベンチは未実施（既定 8MiB / 4 並列）。
6. 接続先 URL を後から変える（独自ドメインへの移行）と再登録が必要。

## Windows 対応時の注意点

- `tsute-client-core` / `tsute-proto` / Backend はそのまま使える（pread/pwrite は Windows の seek_read/seek_write 実装済み）。
- `tsute-os` に Windows 実装を追加: Clipboard（CF_UNICODETEXT / CF_DIB・PNG / CF_HDROP、動画は CF_HDROP が主）、
  通知（WinRT ToastNotification, AUMID 必須）、自動起動（Startup タスク / Run キー）、Credential Manager（DPAPI）。
- Tray は通知領域。メニュー構成は Windows の慣習に合わせる。app データ dir は %APPDATA%（`default_app_dir` を分岐）。
- オートメーションは Unix ソケットなので名前付きパイプ等に置き換える。E2E は WebDriver（tauri-driver）も選択肢。
- `FileSecretStore` を Windows の既定にしてはならない（現状の非 macOS フォールバックを置き換える）。

## Mac で確認する手順

1. `target/release/bundle/macos/Tsute.app` を開く（test 環境の URL がビルド時に埋め込み済み）。
   2 つ目は `open -n target/release/bundle/macos/Tsute.app --args --profile test-b`。
2. 登録キーを発行: `scripts/admin.sh test issue-key`（10 分・一回限り）→ 登録画面に貼り付けて登録。
3. メニューバーのアイコン → 「つて を開く」。送信先を選び、他アプリでコピー → 「Clipboard を送る…」→ 確認 → 送信。
4. もう一方で履歴の「Clipboard にコピー」→ 貼り付け確認。Finder からファイルをドロップ → 確認画面 → 送信。
5. ウィンドウを閉じてもメニューバーに残り受信できること、メニューの「つて を終了」で終了することを確認。
6. 通知: システム設定 > 通知 > Tsute を許可して、受信時にバナーが出るか確認。

## 外部 DNS 設定

現在は不要（`https://d2a2magy0yujlx.cloudfront.net` で全機能が動作）。独自ドメインにする場合:
1. `infra/request-cert.sh <FQDN>` → 表示された検証用 CNAME を DNS に追加（発行まで待つ）
2. `infra/env/test.env` に `TSUTE_APP_DOMAIN` / `TSUTE_CERT_ARN` を設定して `infra/deploy.sh test`
3. DNS に `<FQDN>  CNAME  d2a2magy0yujlx.cloudfront.net` を追加
（注: 切り替え後は各 Endpoint の再登録が必要）
