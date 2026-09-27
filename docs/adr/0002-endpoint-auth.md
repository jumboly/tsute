# ADR-0002: Endpoint 認証

- 状態: Accepted（2026-09-27）

## 決定

1. **Enrollment Key 発行（管理経路）**: 管理者用 Lambda 関数（`tsute-admin`）を AWS IAM 認証で直接 Invoke する
   （`scripts/issue-enrollment-key.sh` = `aws lambda invoke`）。API Gateway には公開しない。
   アプリ独自の管理者パスワードは存在しない。管理者の強さ = AWS アカウントの IAM/MFA/SSO の強さ。
2. **Enrollment Key**: `tsute-ek-` + 160bit 乱数。サーバーは SHA-256 ハッシュのみ保存、TTL 10 分。
   登録時に条件付き削除で消費し、同時使用されても成功は 1 回だけ（一回限り）。
3. **Endpoint 鍵**: 登録時にクライアントで Ed25519 鍵ペアを生成。公開鍵のみサーバーへ。
   秘密鍵は OS Credential Storage（ADR-0011）。共通秘密をアプリに埋め込まない。
4. **認証**: `POST /api/auth/challenge` でサーバー発行 nonce（TTL 120 秒）→
   クライアントが `"tsute-auth-v1\n{endpoint_id}\n{nonce}"` に署名 → `POST /api/auth/token`。
   署名検証**後に** nonce を条件付き削除で消費（リプレイ防止。検証前に消すと第三者が nonce を潰せる）。
5. **アクセストークン**: opaque な 256bit 乱数（`tsute-at-`）、TTL 1 時間。サーバーは SHA-256 のみ保存。
   各リクエストで DynamoDB を 1 回引く。JWT にしない理由: 署名鍵の管理（Secrets Manager 等）が不要になり、
   Endpoint を revoke した瞬間に全トークンを無効化できる。個人利用のリクエスト量ではコストは無視できる。
6. **WebSocket**: `$connect` 時の `Authorization: Bearer` ヘッダで同じトークンを検証（URL に載せない＝ログに残さない）。
7. challenge は未登録 Endpoint にも同形で応答し、ID の存在確認に使えないようにする。

## 検討した代替

- mTLS（クライアント証明書）: API Gateway HTTP API の mTLS はカスタムドメイン必須で CloudFront 経由構成と相性が悪い。
- 全リクエスト署名（HTTP Message Signatures）: 実装が複雑で、presigned URL 取得など頻繁な呼び出しで利点が薄い。
- Cognito: ユーザー管理機能は不要（Sign Up なし、所有者のみ）。
