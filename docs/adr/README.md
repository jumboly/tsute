# ADR 一覧

| # | タイトル | 状態 |
|---|---|---|
| [0001](0001-desktop-framework.md) | Desktop framework: Tauri 2 + Rust core + 必要箇所は OS ネイティブ API | Accepted |
| [0002](0002-endpoint-auth.md) | Endpoint 認証: Ed25519 + 一回限り Enrollment Key + IAM 保護の発行経路 | Accepted |
| [0003](0003-clipboard-normalization.md) | Clipboard 形式の正規化（macOS。Windows は 0016） | Accepted |
| [0004](0004-chunked-transfer.md) | 転送: アプリ側 chunk・独立 Object・chunk size/並列数・checksum | Accepted |
| [0005](0005-resume.md) | Resume 方式（サーバー正/ローカル正の分担） | Accepted |
| [0006](0006-dynamodb-model.md) | DynamoDB データモデル（単一テーブル） | Accepted |
| [0007](0007-websocket.md) | WebSocket: 通知はヒント・keepalive・再接続 | Accepted |
| [0008](0008-backend-runtime-and-local-server.md) | Backend 実装言語と、共有ロジック + ローカル開発サーバー | Accepted |
| [0009](0009-iac.md) | IaC: CloudFormation（スタック分割と所有責務） | Accepted |
| [0010](0010-cicd.md) | CI/CD: GitHub Actions + OIDC | Accepted |
| [0011](0011-credential-storage.md) | Credential Storage: Keychain（開発用ファイルストアは明示フラグ時のみ） | Accepted |
| [0012](0012-autostart-and-profiles.md) | Autostart と複数 Profile | Accepted |
| [0013](0013-desktop-e2e-automation.md) | Desktop E2E: アプリ内オートメーション経路 | Accepted |
| [0014](0014-unread-tray-badge.md) | 受信の知らせ方: メニューバーアイコンの未確認印（OS 通知の代替） | Accepted |
| [0015](0015-web-pwa-client.md) | Web / PWA Client のアーキテクチャと Capability Model | Accepted |
| [0016](0016-windows-desktop.md) | Windows デスクトップ統合（Clipboard・通知・自動起動・資格情報・トレイ） | Accepted（実機未確認） |
| [0017](0017-namespace.md) | Namespace による Endpoint の分離（認可境界） | Accepted |
