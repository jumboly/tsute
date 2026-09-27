# ADR-0015: Web / PWA Client のアーキテクチャと Capability Model

- 状態: **Proposed**（2026-09-27）。Web / PWA Phase の着手時に「着手時に再確認する事項」を調べ、
  結果を反映して Accepted にする。着手順（Windows / Web のどちらが先か）はユーザー判断。
- 要件原文: `docs/requirements/web-pwa.md`

## 背景

- Phase 1（macOS）は完了済み。Web / PWA は Phase 1 の完了条件に含めず、独立した Phase（以下 Phase W）として扱う。
- Web は Native の代替ではなく「スマホ → PC へ Text（特に OS キーボードの音声入力）」「Browser の Text/Image」
  「未インストール環境からの一時利用」が主目的。Browser の制約は回避せず Capability 差として表現する。
- 既存の前提: Ed25519 challenge/response + opaque トークン（ADR-0002）、WebSocket は通知ヒントで
  状態の正は HTTP/DynamoDB（ADR-0007）、単一 FQDN の CloudFront path routing（ADR-0009）。

## 決定

### 1. 配置と独立デプロイ

- Web クライアントは **このリポジトリの `web/`** に置く（Blog とは別）。プロトコル型（`crates/proto`）を
  共有してクライアント/サーバーの食い違いを防ぐため。型は proto から TypeScript を生成する方向（手書きの二重管理を避ける）。
- edge スタックに `/app/*` の CacheBehavior と App 用 S3 バケット（OAC）を追加し、App 用 CI ロールは
  「App バケットへの sync と `/app/*` の invalidation」だけを許可する。Blog / App / Backend はそれぞれ単独でデプロイできる。
- `/app` → `/app/` はリダイレクト。manifest の `scope` / `start_url` と Service Worker の scope は `/app/`
  （Blog 側のページを SW が横取りしないため）。
- API / WS の URL は `location.origin` からの相対（`/api/`, `wss://<host>/ws`）で導出する。APP_BASE_URL を
  ビルド成果物に埋め込まない（同じ成果物を Test / Prod に配れる）。

### 2. Web Endpoint の認証（ADR-0002 を共有）

- Endpoint = Browser / PWA インスタンス。登録は既存の一回限り Enrollment Key。
- 鍵: Web Crypto で `Ed25519` を `extractable: false` で生成し、`CryptoKey` をそのまま IndexedDB に保存する
  （structured clone で非抽出のまま永続化できる。秘密鍵のバイト列は JS から読めない）。
  サーバーの検証ロジックは Native と同一。アルゴリズムを増やさないため Ed25519 に揃える。
  - 未対応 Browser が主要ターゲットに残る場合のみ ECDSA P-256 の追加を検討する（着手時に判断）。
- アクセストークンは **メモリのみ** に保持し、起動のたびに challenge/response で取り直す
  （localStorage に置くと XSS 一発で持ち出されるため）。
- API は `Authorization: Bearer` のみで認証し、**Cookie を使わない**。ambient credential が無いので CSRF が
  成立しない。CORS 許可ヘッダは返さない（同一オリジンのみ）。
- `navigator.storage.persist()` を要求する。それでも Browser Data 削除・ストレージ退避で鍵が消えたら
  再 Enrollment（古い Endpoint は他の Endpoint から revoke）。Keychain 同等の永続性は前提にしない。

### 3. WebSocket の認証（Browser 向けの追加）

- Browser の WebSocket API は `Authorization` ヘッダを付けられない。そこで
  `POST /api/ws-ticket`（Bearer 認証）で **一回限り・TTL 30 秒の ticket** を発行し、
  `Sec-WebSocket-Protocol: tsute.v1, ticket.<ticket>` で渡す。`$connect` で条件付き削除により消費し、
  応答で `tsute.v1` を echo する。
- クエリ文字列にしない理由: URL は CloudFront / API Gateway のアクセスログに残りうるため。
- Native は従来どおり `Authorization` ヘッダ（変更なし）。

### 4. Capability Model

Endpoint が持つ能力を 3 種類に分け、「誰がそれを知る必要があるか」で置き場所を決める。

| 種類 | 例 | 置き場所 | 決め方 |
|---|---|---|---|
| **受信できる Payload**（`accepts`） | `clipboard_text`, `clipboard_image`, `clipboard_video`, `files` | Endpoint レコード | クライアントが自己申告（登録時 + `PUT /api/endpoints/me/capabilities`） |
| **到達手段**（`reach`） | `websocket`, `web_push` | サーバーが状態から導出 | WS 接続レコード / Push Subscription の有無。フラグとして保存しない（古くならないため） |
| **ローカル機能** | clipboard read API、share target、音声入力、常駐 | サーバーに送らない | クライアントが feature detection で UI を切り替える |

- `accepts` は `TransferKind` の集合（必要なら `max_receive_bytes` を併記）。既存レコードで欠落している場合は
  「Native の全種類」とみなす（既存 Endpoint の移行不要）。
- `EndpointInfo` に `client_kind`（`native` | `web`）・`accepts`・`reach` を追加する。`client_kind` は表示用で、
  判定には使わない。
- 送信側 UI は「今の Payload の kind ∈ 送信先の accepts」で選択可否を決め、不可なら理由を表示する。
  サーバーも `create_transfer` で同じ判定を行い、不一致は拒否する（UI を迂回した呼び出しへの多層防御）。
- 通知は「到達手段ごとの Notifier」を順に試す形にし、`if web {..} else {..}` をコアに持ち込まない。
- Web の MVP の `accepts` は `clipboard_text`, `clipboard_image`。将来 `files` を申告すれば既存の
  Transfer Engine（chunk 転送）へそのまま繋がる。

### 5. 転送と Offline（ADR-0005/0007 を共有）

- Text は既存どおり HTTP inline（≤ `INLINE_TEXT_MAX_BYTES`）。Image は既存の chunk 転送（1 ファイル）を使い、
  Browser から presigned URL で S3 へ直接 PUT/GET する。S3 バケットの CORS に App のオリジン
  （環境設定から導出）だけを許可する。presigned URL はメモリのみで扱い、ログ・永続ストレージに書かない。
- **状態の正は DynamoDB の Transfer**。受信側は「App を開いた / WS 再接続 / Push から起動」のいずれでも
  `GET` で未受信 Transfer を回収する。WS・Push はヒントであり、取りこぼしても配送は失われない。
- 受信内容は画面にだけ表示し、Clipboard へのコピー / 画像保存 / Dismiss をユーザー操作で行う。
  受信内容を IndexedDB / Cache Storage に保存しない（SW は App Shell のみキャッシュし、`/api/` はキャッシュしない）。

### 6. Web Push

- VAPID 鍵ペアはスクリプトで生成し、秘密鍵は SSM Parameter Store（SecureString）に置く。リポジトリには入れない。
- Subscription は認証済み Endpoint に紐付けて保存する（新アイテム `PUSH#<endpoint_id>` / `S#<sha256(endpoint URL)>`）。
  登録・削除は本人のトークンでのみ可能。Endpoint の revoke 時に削除、push service が 404/410 を返したら削除。
- 送信条件: 受信側 Endpoint に **生きた WS 接続が無いときだけ** Push する（iOS は Push ごとに通知表示が必須のため、
  前面で WS 通知を受けている時に二重に出さない）。
- **Payload は内容・種類・サイズを含めない**（`{"t":"transfer_ready"}` のみ）。通知文言も汎用
  （「新しい受信があります」）。ロック画面での露出を避けるため。本文は通知タップ後に App が API から取得する。
- SSRF 対策: Subscription の endpoint URL は https かつ既知の push service ホスト
  （FCM / Apple / Mozilla / WNS 等、着手時に確定）だけを受け付ける。
- Permission 要求は必ずユーザーのボタン操作から。iOS / iPadOS はホーム画面に追加した PWA のみ対象。
  Push 不可の環境は「次回起動時の回収」がフォールバック。

### 7. 入力 / Clipboard / Share Target

- Text 入力欄が主経路。キーボード入力・Paste・OS キーボードの音声入力はすべて同じ Text Payload になる。
- 送信は常に「内容取得 → Preview → 送信先確認 → Send」。入力や Clipboard 読み込みだけでは送らない。
- 「Clipboard から読み込む」ボタン（Async Clipboard API、対応時のみ表示）と、入力欄への Paste イベントの 2 経路。
  Image は PNG を基本に正規化し、Preview・形式・幅・高さ・サイズを表示する。
- **Web Speech API は MVP で使わない**。Chromium の実装は音声をサーバー側で認識しうる（Clipboard 同様に機微な
  内容が第三者に渡る）ため。OS キーボードの音声入力で主要ユースケースは満たせる。将来は明示 opt-in の拡張として検討。
- Web Share Target は manifest で宣言し、対応環境（Android / Chromium）でのみ動く。共有された内容は
  送信画面の Preview に入るだけで、送信は通常どおりユーザーの Send。iOS 向けの Workaround は作らない。

### 8. Web Security

- CSP（`/app/*` の ResponseHeadersPolicy）: `default-src 'self'; script-src 'self'; connect-src 'self' <S3 転送バケット>;
  img-src 'self' blob: data:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'`。インライン script なし。
- 受信内容・Endpoint 名は `textContent` で描画（Native UI と同方針）。`innerHTML` を使わない。
- Analytics・外部 CDN を読み込まない。Clipboard 本文・画像・トークン・ticket・presigned URL を
  Application Log / Access Log に出さない（既存方針の継続）。
- Enrollment Key の入力欄は `autocomplete="off"`。リプレイは既存の一回限り消費で防ぐ。

## 着手時に再確認する事項（現時点の理解は要検証）

- Web Crypto Ed25519 の対応（Chrome / Safari / Firefox、iOS Safari の最低版）と IndexedDB への CryptoKey 保存。
- Safari の script-writable storage 削除ポリシー（ホーム画面 PWA が対象外か）と `storage.persist()` の挙動。
- Async Clipboard API の read/write（Image 形式、Permission、Paste 確認 UI、User Activation 要件）各 Browser 差。
- iOS / iPadOS の Web Push（ホーム画面 PWA 限定か、Declarative Web Push の扱い）と Android / Desktop の Push。
- Web Share Target の対応状況（Android Chrome、Desktop Chromium、iOS）。
- API Gateway WebSocket の `$connect` で `Sec-WebSocket-Protocol` を echo できること（実機で確認）。
- Web クライアントの実装技術（素の TS + 小さなビルド or ビルドなし）と proto からの型生成手段。

## 検討した代替

- **Web 用に Cookie セッション**: CSRF 対策（SameSite/トークン）が必要になり、Native と認証経路が分かれる。Bearer に揃えた。
- **WS の ticket をクエリ文字列で渡す**: 実装は単純だがアクセスログに残る。短命・一回限りでも避けた。
- **Capability を単一のフラグ集合で保存**（`websocket` や `web_push` も Endpoint に保存）: 接続・購読の実状態と
  ずれる。到達手段は状態から導出する。
- **Push に Text 本文を暗号化して載せる**: RFC 8291 で暗号化されるが、通知表示・端末側の保持で露出しうる。
  Push はヒントに限定した。
