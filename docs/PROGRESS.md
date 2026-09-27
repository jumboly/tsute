# 進捗

最終更新: 2026-09-27

## 現在地

Phase 1（macOS）実装中。

- 完了: workspace 構成、プロトコル型、バックエンドのドメインロジック、ローカル開発サーバー、
  クライアントコア（認証・WebSocket・chunk 転送・再開）、ローカル結合テスト 9 件（安定して全件成功）。
- PoC 済み: Tauri 2 のメニューバー常駐、複数 profile 同時起動（メニューバーに profile 名表示）。

## 次にやること

1. AWS Lambda アダプタ（DynamoDB / S3 / API Gateway Management API）
2. CloudFormation（backend / edge）と deploy スクリプト、Enrollment Key 発行スクリプト
3. デスクトップアプリ本体（UI、Clipboard、D&D、Keychain、通知、Login Item）
4. 実 AWS デプロイ → 同一 Mac 2 Endpoint の E2E
5. GitHub Actions、Blog リポジトリ

## 既知の問題 / 注意

- AWS CLI のセッションが期限切れ（`aws login` が必要）。デプロイ前にユーザーへ依頼する。
- Accessibility 権限がないため System Events による UI 自動操作は不可（ADR-0013）。
- Xcode 本体は未インストール（Command Line Tools のみ）。Tauri の .app バンドルは CLT で作成可能。

## 判断ログ（ADR 化しない小さなもの）

- ed25519-dalek は 2.x を使用（3.0 は 2026 年リリース直後で rand 0.8 系との互換を優先）。
