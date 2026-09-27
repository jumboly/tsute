# 実行・テスト方法

## 前提

- Rust stable（1.95+）、macOS 13+（開発機は macOS 27.0 / arm64 で確認）
- Tauri CLI: `cargo install tauri-cli --version "^2" --locked`
- Lambda ビルド: `uv tool install cargo-lambda`（zig 同梱。`infra/deploy.sh` が自動で PATH に追加）
- AWS CLI v2（デプロイ・管理操作時のみ）

## 1. ユニット / 結合テスト（AWS 不要）

```sh
cargo test --workspace
```

- `crates/client-core/tests/e2e_local.rs`（10 件）: ローカル開発サーバーを相手に、実 HTTP / WebSocket /
  presigned URL / ファイル I/O で 2 Endpoint 間を検証。送信側・受信側の kill→再起動、改ざん検出、
  Object Storage 一時障害、WebSocket 強制切断→再接続、既定 8MiB チャンク。
- `crates/os/tests/media_macos.rs`: 動画メタデータ・サムネイル（AVFoundation）。

## 2. 実 OS Clipboard テスト（ユーザーの Clipboard を上書きするため明示実行）

```sh
cargo test -p tsute-os --test clipboard_macos -- --ignored --test-threads=1
```

Text（pbcopy）/ PNG / TIFF のみ（PNG 正規化）/ 動画 file URL / 動画実データ / 複数ファイル。
実行前のテキストを退避・復元する。

## 3. デスクトップ E2E（同一 Mac で 2 Endpoint）

```sh
cargo build -p tsute-desktop
python3 e2e/run_e2e.py --target local                  # ローカル開発サーバー
python3 e2e/run_e2e.py --target cloud --env test       # デプロイ済み AWS（infra/.build/test.json を使用）
python3 e2e/run_e2e.py --binary target/release/bundle/macos/Tsute.app/Contents/MacOS/tsute   # .app で実行
```

結果は `e2e/out/e2e-<target>.json`。シナリオ: UI からの登録、Enrollment Key 再利用拒否、Endpoint 一覧、
Text（inline / 64KiB 超）、Image、Video（file URL / 実データ）、複数・単一ファイル Drop（確認画面の内容検証）、
64MB 転送中の受信側 kill→再開、送信側 kill→再開、ウィンドウを閉じても常駐・受信継続、Quit。
仕組みは ADR-0013（アプリ内オートメーション経路）。

## 4. 手動で 2 Endpoint を起動する

```sh
cargo run -p tsute-server-local --bin tsute-devserver -- --data-dir target/devserver   # 別ターミナル
curl -s -X POST -H "x-admin-token: $(cat target/devserver/admin-token)" http://127.0.0.1:8787/admin/enrollment-keys
target/debug/tsute --profile test-a --show
target/debug/tsute --profile test-b --show
```

AWS の場合は `scripts/admin.sh <env> issue-key` で Enrollment Key を発行する。

## 5. .app バンドル

```sh
cd apps/desktop && TSUTE_DEFAULT_BASE_URL=https://<APP_BASE_URL> cargo tauri build --bundles app
open target/release/bundle/macos/Tsute.app --args --profile test-a   # 別プロファイルは --args で
```

OS 通知とログイン項目（SMAppService）は .app として起動した場合のみ有効。

## 6. カスタムドメインの設定（環境ごと）

ドメイン名はリポジトリに書かず、`infra/env/<env>.env`（gitignore 済み）にだけ置く。

```sh
infra/request-cert.sh <fqdn>                 # us-east-1 に ACM 証明書を要求
# infra/env/<env>.env に TSUTE_APP_DOMAIN と TSUTE_CERT_ARN を設定
infra/route53-subdomain.sh <env>             # 親ゾーンが CNAME 検証を拒否する場合: Route 53 に委任（NS を表示）
# ユーザーが親ゾーンに NS（または検証用 CNAME + CNAME）を追加 → 証明書が ISSUED になるのを待つ
infra/deploy.sh <env>                        # CloudFront に Alias と証明書を設定
infra/route53-subdomain.sh <env>             # 委任している場合: CloudFront への ALIAS を更新
python3 e2e/run_e2e.py --target cloud --env <env>   # 独自ドメイン経由で E2E
```

## 7. Web / PWA Client（Phase W, ADR-0015）

前提: `pip install playwright && python3 -m playwright install chromium webkit`

```sh
cargo test -p tsute-server-core --test web_rules             # Capability / WS ticket / Push 条件 / SSRF / 所有権
python3 e2e/web_e2e.py --target local                        # Web ↔ Web（Chromium・WebKit、各 13 ステップ）
python3 e2e/web_native_e2e.py --target local                 # Native(.app/debug) ↔ Web（実 OS Clipboard を使う）
python3 e2e/web_e2e.py --target cloud --env test             # デプロイ済み環境
```

- `web_e2e.py`: UI からの Enrollment、Key 再利用拒否、Endpoint 一覧（Web / オンライン表示）、Text の即時受信（WS）
  と明示 Copy、入力だけでは送らないこと、64KiB 超 Text（chunk 転送）、Image（Clipboard 読み込み / Paste、PNG 正規化）、
  閉じている間に送った Text の再 Open 後の回収（同じ Endpoint のまま・未操作ならリロードでも残る）、
  受信できない種類の 422 拒否、Share Target（SW 経由で Preview に入るだけ）、manifest / SW scope、CSP・Cookie 不使用。
  headless Browser の Clipboard は OS から独立している（ユーザーの Clipboard は変わらない）。
- ローカルで手動確認: `cargo run -p tsute-server-local --bin tsute-devserver` → `http://127.0.0.1:8787/app/`
  （127.0.0.1 / localhost は secure context なので Web Crypto・SW・Push が使える）。

### デプロイ（独立）

```sh
infra/vapid.sh <env>          # 初回のみ: VAPID 秘密鍵を SSM SecureString に作成し、API 関数を再起動して読み込ませる（無ければ Web Push は無効）
infra/deploy.sh <env>         # Backend / Edge（/app/* のビヘイビア・App バケット・S3 CORS）
infra/deploy-web.sh <env>     # web/ だけを同期して /app/* を invalidate（Backend・Blog は触らない）
```

### スマホ実機での確認（自動化できない項目。単に Browser で動いただけでは完了にしない）

1. iPhone Safari で `https://<APP_BASE_URL>/app/` → Enrollment Key で登録 → 共有 →「ホーム画面に追加」→ ホーム画面から起動（standalone）
2. ホーム画面の PWA を閉じて再度開き、同じ Endpoint 名のまま使える（再登録を求められない）
3. テキスト欄でキーボードのマイク（音声入力）→ 送信先に Mac を選ぶ → 確認 → Send → Mac で受信・反映
4. Mac から Text / Image を送る → PWA が前面なら即時表示（WS）→ コピー / 保存（共有 → 画像を保存）
5. 設定 →「通知を有効にする」→ PWA を閉じる → Mac から送る → 通知（内容は表示されない）→ タップで開いて表示
6. 機内モード中に Mac から送る → 解除して PWA を開く → 回収される
7. Android Chrome: インストール → 他アプリの共有メニューで「つて」→ Preview に入る → Send
8. Firefox / 古い Browser: Clipboard ボタンが無くても 入力 / 貼り付け / Send が機能する
