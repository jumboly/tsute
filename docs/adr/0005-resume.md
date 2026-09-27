# ADR-0005: Resume 方式

- 状態: Accepted（2026-09-27）

## 決定

- **アップロード済み chunk の正はサーバー**（DynamoDB の chunk item。HeadObject 照合済みのものだけ）。
  送信側は再開時に `GET /api/transfers/{id}` で不足 chunk を求めて、そこだけ送る。
- **受信済み chunk の正はローカル SQLite**（`incoming_chunks`。fsync 後に記録）。
- 送信側ローカル DB には送信元パスと開始時の size/mtime を保存。再開時に変わっていたら中止（混在防止）。
- 一時的エラー（ネットワーク、5xx、URL 期限切れ 403 等）は指数バックオフで最大 6 回リトライ。
  それでも失敗したら Transfer を Active のまま残し、30 秒後・WebSocket 再接続時・2 分毎の再同期で再開。
- アプリ再起動時は Active な送受信を自動再開。
- 受信側 part ファイルが消えていたらそのファイルの chunk 記録を消して取り直す。
- 最終 checksum 不一致はファイル単位で 1 回だけ全取り直し、再度不一致なら Failed。

## 検証

`crates/client-core/tests/e2e_local.rs`: 送信側 kill→再起動で残りのみ送信、受信側 kill→受信済み chunk を再利用、
Object 改ざん検出、Object Storage 一時障害のリトライ。
