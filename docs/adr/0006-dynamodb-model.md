# ADR-0006: DynamoDB データモデル

- 状態: Accepted（2026-09-27）

単一テーブル `pk`/`sk`、TTL 属性 `ttl`、オンデマンド課金。個人利用で件数が極小のため、
一覧系は固定パーティションの Query で足り、GSI を持たない（コストと複雑さを抑える）。

| pk | sk | 内容 |
|---|---|---|
| `EKEY#<sha256>` | `-` | Enrollment Key（namespace, ttl=10分） |
| `CHAL#<nonce>` | `-` | 認証チャレンジ（endpoint_id, ttl=120秒） |
| `TOKEN#<sha256>` | `-` | アクセストークン（endpoint_id, ttl=1時間） |
| `ENDPOINTS` | `EP#<id>` | Endpoint（name, platform, public_key, created_at, namespace） |
| `CONNS` | `C#<connectionId>` | WebSocket 接続（endpoint_id, connected_at, ttl=3時間） |
| `TRANSFERS` | `T#<id>` | Transfer メタデータ（JSON, state, ttl=7日） |
| `XFER#<id>` | `C#<file>#<index>` | アップロード済み chunk（size, sha256） |
| `XFER#<id>` | `F#<file>` | finalize 済みファイルの sha256 |

- 一回限り操作（Enrollment Key / nonce の消費）は `DeleteItem` + `ConditionExpression`（ttl > now）+ `ReturnValues=ALL_OLD`。
- 状態遷移は `UpdateItem` の条件付き（`state IN (...)`）。
- TTL 削除は遅延するため、読み出し時にも ttl を確認する。
- `namespace` 属性（ADR-0017）が無い項目は `default` とみなす（Namespace 導入前のデータを書き換えない）。
