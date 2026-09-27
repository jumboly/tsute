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
