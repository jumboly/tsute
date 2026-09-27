# ADR-0008: Backend 実装言語と、共有ロジック + ローカル開発サーバー

- 状態: Accepted（2026-09-27）

## 決定

- Lambda は Rust（`provided.al2023`, arm64/Graviton）。`tsute-proto` の型をクライアントと共有し、
  プロトコルの食い違いをコンパイル時に防ぐ。cold start も小さい。ビルドは `cargo lambda`（zig によるクロスコンパイル）。
- ドメインロジックは `tsute-server-core` に置き、保存先・Object Storage・通知だけを trait で差し替える。
  - AWS: DynamoDB / S3 / API Gateway Management API（`tsute-server-lambda`）
  - ローカル: メモリ / ファイル + HMAC 署名 URL / tokio channel（`tsute-server-local`）
- ローカル開発サーバーは本番と同じ URL 構造（`/api/*`, `/ws`, presigned URL）を 1 プロセスで再現し、
  AWS なしでも実通信の結合テストを回せる。ただし **ローカルだけで Done にしない**（実 AWS でも検証する）。
