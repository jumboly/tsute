# ADR-0017: Namespace による Endpoint の分離

- 状態: Accepted（2026-09-29）。issue #15。

## 背景

1 つの環境（AWS アカウント内の 1 スタック）に登録した Endpoint は、これまで全員が互いを送信先として見られた。
家族や別の用途の端末、E2E のテスト用 Endpoint を同じ環境に置くと、送信先一覧に混ざり、誤送信の余地が生まれる。
表示上のグループ分けでは UI を迂回した呼び出しを防げないため、**認可の境界**として Namespace を導入する。

## 決定

1. **所属は Enrollment Key が決める**。Key 発行時に管理者が Namespace を指定し（`scripts/admin.sh <env> issue-key <ns>`）、
   Key の DynamoDB 項目に `namespace` 属性として保存する。登録時は Key の消費（条件付き削除 + `ALL_OLD`）で
   同時に Namespace を受け取り、Endpoint に書く。クライアントは Namespace を申告できず（Enroll 本文に書いても無視）、
   登録後に移動する API も無い。所属を変えるときは revoke → 別 Namespace の Key で再 Enrollment（ADR-0002 の管理経路のまま）。
   Lambda の管理操作では Namespace は必須（既定値に落とすと、指定漏れで意図しない Namespace に参加させてしまうため）。
2. **Backend で強制する境界**:
   - `GET /api/endpoints`・`GET /api/me`: 自分の Namespace の Endpoint だけを返す。
   - `POST /api/transfers`: 受信者が別 Namespace なら「未登録の receiver」と同じ 400 を返す（ID の存在を判別させない）。
   - Transfer の取得・Upload / Download URL・received・cancel: 従来どおり当事者（送信者か受信者）だけが操作できる。
     作成時点で当事者は同じ Namespace に限られ、Namespace は変わらないので、これで境界内に収まる。
   - WebSocket の一斉通知（`presence`・`endpoints_changed`）: 対象の Endpoint と同じ Namespace の接続にだけ送る
     （別 Namespace へ Endpoint ID やオンライン状態を漏らさない）。個別通知（`transfer_created` 等）と Web Push は
     従来どおり当事者宛のみ。
3. **データモデル**（ADR-0006 に追記）: Endpoint（`ENDPOINTS` / `EP#<id>`）と Enrollment Key（`EKEY#…`）に `namespace` 属性。
   件数が小さく一覧は固定パーティションの Query で足りるため、GSI やキー構造の変更はしない（アプリ側で絞り込む）。
4. **Namespace 名**: 1〜64 文字の `[a-z0-9_-]`、先頭は英数字。管理者が打ち、ログや管理出力にそのまま出る値なので、
   大文字小文字の揺れや紛らわしい文字を許さない。`admin.sh` も同じ規則で先に検証する（Payload の JSON を壊させない）。
5. **既存データの互換**: `namespace` 属性が無い Endpoint・Enrollment Key は `default` に属するとみなす。
   既存レコードの書き換え・データ消失・再 Enrollment は不要で、既存の Endpoint 同士は従来どおり送受信できる。
   既存の Endpoint と同じグループに端末を足すときは `scripts/admin.sh <env> issue-key default` で発行する。
6. **管理一覧**: `admin.sh <env> list [ns]` は全 Namespace（または指定した Namespace）の Endpoint を `namespace` 付きで返す。
7. **クラウド E2E** は既定で `e2e` Namespace に登録する（`TSUTE_E2E_NAMESPACE` で変更可）。テスト用 Endpoint が
   普段使いの端末の送信先一覧に混ざらない。ローカル開発サーバーの `/admin/enrollment-keys` は本文 `{"namespace": …}` を
   省略すると `default`（ローカル専用の簡易経路で、既存 E2E をそのまま動かすため）。

## 検討した代替

- **pk に Namespace を含める**（`ENDPOINTS#<ns>`）: Namespace 単位の Query になるが、既存レコードの移行（書き換え）が必要。
  件数が極小のいまは利点が無い。件数が増えたら GSI かキー変更で見直す。
- **Namespace をトークンに埋め込む**: トークンは opaque（ADR-0002）で、毎回 Endpoint を引いているため不要。
- **Transfer に Namespace を保存して取得時に照合**: 当事者チェックと Namespace の不変性で既に保証されるため二重になる。
  Namespace を移動可能にする場合は見直す。
