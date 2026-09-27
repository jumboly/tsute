# 実行・テスト方法

## 前提

- Rust（stable, 1.95+）、macOS 13+
- Tauri CLI: `cargo install tauri-cli --version "^2" --locked`
- Lambda ビルド: `uv tool install cargo-lambda`（zig 同梱）
- AWS CLI v2（デプロイ・管理操作時のみ）

## ユニット / 結合テスト（AWS 不要）

```sh
cargo test --workspace
```

- `crates/client-core/tests/e2e_local.rs`: ローカル開発サーバー（`tsute-server-local`）を相手に、
  実 HTTP / WebSocket / presigned URL / ファイル I/O で 2 Endpoint 間の転送を検証。
  送信側・受信側の kill→再起動、改ざん検出、Object Storage 一時障害、WebSocket 再接続を含む。

## ローカル開発サーバー

```sh
cargo run -p tsute-server-local --bin tsute-devserver -- --bind 127.0.0.1:8787 --data-dir target/devserver
# 登録キー発行
curl -s -X POST -H "x-admin-token: $(cat target/devserver/admin-token)" http://127.0.0.1:8787/admin/enrollment-keys
```
