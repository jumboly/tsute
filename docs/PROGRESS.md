# 進捗

最終更新: 2026-09-27

## 現在地

**Phase 1 完了。2026-09-27 にユーザーが .app（default / test-b）で実機の動作を確認した。**
OS 通知は ad-hoc 署名では不可のため、メニューバーの未確認印で代替（ADR-0014, 実機確認済み）。 詳細は docs/PHASE1_REPORT.md。
test 環境（AWS 444369845617 / ap-northeast-1）にデプロイ済み。**クラウド経由 E2E 14/14 PASS**。
**2026-09-27 test 環境に独自ドメインを設定**（値は `infra/env/test.env` のみ。リポジトリに書かない）。
アプリ用サブドメインを Route 53 に委任（親ゾーンの DNS 事業者が ACM 検証用 CNAME を拒否したため, ADR-0009）。
独自ドメイン経由のクラウド E2E 14/14 PASS（2 回連続）。
Blog リポジトリの GitHub Actions デプロイ（OIDC）が成功し、独自ドメインで Astro 版 Blog を配信中。
Blog の存在しない URL は Lambda@Edge で 404 ページ（ステータス 404）を返す。API のエラー JSON は不変（確認済み）。
Phase 2（Windows）はユーザーの指示があるまで着手しない。

**Phase W（Web / PWA）実装済み・スマホ実機確認待ち（2026-09-27）。** ユーザー指示で Windows より先に着手。
ADR-0015 を調査結果で更新し Accepted。原文 `docs/requirements/web-pwa.md`、機能一覧 features.json の `phase: "W"`。
- test 環境にデプロイ済み（Backend / Edge / `/app/`）。クラウド Web E2E 26/26（Chromium・WebKit、2 回連続）、
  Native ↔ Web 7/7、既存デスクトップ E2E 14/14（回帰なし）。ローカルも同じく全通過。
- **Web Push は test 環境で未有効**: VAPID 鍵を SSM に書く `infra/vapid.sh test` がエージェントの権限で拒否されたため、
  ユーザーが実行する（その後 `infra/deploy.sh test` で API 関数に読み込ませる）。鍵が無い間は「次に開いたときに回収」で動く。

### できていること
- Rust workspace: proto / server-core / server-local / server-lambda / client-core / os / desktop
- Backend ロジック（認証・転送・通知）、Lambda アダプタ（DynamoDB/S3/API GW）— Lambda は arm64 ビルド確認済み
- CloudFormation（bootstrap / backend / edge）、deploy・証明書・管理スクリプト、cfn-lint 通過
- クライアントコア: Ed25519 認証、WebSocket keepalive/再接続、chunk 並列転送、overlap、再開、SHA-256 検証
- macOS 統合: NSPasteboard 読み取り/正規化/書き込み、動画メタデータ/サムネイル、通知、ログイン項目、Finder 表示
- デスクトップ: メニューバー常駐、プロファイル分離、登録/送信/プレビュー/ファイル確認/履歴/設定 UI、
  WebView の破棄/再生成、オートメーション経路（E2E 用）
- テスト: 結合 10 件、server-core ルール 4 件、実 Clipboard 6 件（手動実行）、メディア 1 件、
  デスクトップ E2E 14 ステップ（debug / release .app とも PASS）、.app 統合チェック（e2e/app_checks.py）
- GitHub Actions ワークフロー（CI / backend deploy / desktop build）、Blog リポジトリ（~/src/tsute-blog）

## 次にやること

1. **[ユーザー作業]** `infra/vapid.sh test` → `infra/deploy.sh test`（Web Push の有効化）
2. **[ユーザー実機確認]** docs/TESTING.md §7 のスマホ手順（iPhone PWA: 登録・再起動後も同一 Endpoint・音声入力 → Send →
   Mac で受信・Push 通知・機内モード後の回収、Android: Share Target、Firefox: Paste 経路）。結果で features.json を done に
3. Phase 2（Windows）はユーザーの指示があるまで着手しない。
   GitHub リポジトリの作成（CI/CD の実行。`web-e2e` ジョブと `deploy-web.yml` を追加済み・未実行）
4. chunk size / 並列数の実回線ベンチ
5. 手動確認: OS 通知の許可と表示（ad-hoc 署名の .app では自動許可されず granted=false だった）、
   メニューバーのクリック操作、Finder からの実ドラッグ&ドロップ
6. GitHub リポジトリ作成（ユーザー確認が必要）→ CI 実行。作成したら
   `gh api repos/<owner>/<repo>/actions/oidc/customization/sub` を確認し、immutable subject が有効なら
   bootstrap の `GitHubAppRepo` を `owner@ownerId/repo@repoId` 形式で更新する（ADR-0010。Blog ロールで実際に踏んだ）
7. Phase 1 完了報告

## .app 統合チェック結果（2026-09-27, e2e/out/app-checks.json）

- Keychain: 保存 → 再起動後に許可ダイアログなしで読み出し・再認証 → 削除 OK
- ログイン項目（SMAppService.mainApp）: 登録で enabled → 解除で not_registered（テスト後に解除済み）
- Idle（ウィンドウを閉じた状態）: CPU 0.0%、RSS 約 101MB（ウィンドウ表示中 約 110MB + WebContent プロセス）
- 通知: authorization granted=false（要手動許可・表示確認）

## 既知の問題 / 注意

- Web: Playwright の WebKit ビルドでは `pushManager.getSubscription()` がページごと固まる。通知許可が無いときは
  pushManager に触れない実装にして回避（許可が無ければ有効な購読は存在しないため、実 Safari でも妥当）。
- Web: 受信は Copy / 保存 / 共有 / 閉じる で received になる。操作直後にページを閉じると POST が中断され、
  次回また表示されることがある（安全側）。
- Web: 画像の Clipboard 書き込みに対応しない Browser では「保存」「共有…」を使う。
- 既存の Mac の .app（旧ビルド）は accepts を知らないため、Web 宛に動画・ファイルを選ぶと送信時にサーバーが
  422 で拒否する（確認画面での理由表示は再ビルド後）。

- 独自ドメイン設定直後のクラウド E2E で「Video（file URL）のプレビュー待ちタイムアウト」が 2 回続いた。
  サーバー通信を伴わない手順で、E2E 修正後の 2 回は再現しなかった。原因は未特定（実行中の Clipboard 操作との
  干渉を疑う）。再発したら `--keep` でログを残して調べる。
- 登録済みの Mac の Endpoint は旧 `*.cloudfront.net` の URL のままでも動く。独自ドメインへの切り替えは任意。

- AWS CLI のセッション期限切れ（`aws login` が必要）。
- Accessibility 権限がないため System Events による UI 自動操作は不可 → アプリ内オートメーション（ADR-0013）。
  OS からの実ドラッグ、メニューバークリック、通知クリック、実ログインは手動確認項目。
- Xcode 本体は未インストール（CLT のみ）。.app は ad-hoc 署名。配布には Developer ID 署名と公証が必要。
- ad-hoc 署名のため、再ビルド後は既存 Keychain 項目へのアクセス時に許可ダイアログが出る（ADR-0011）。
- テスト実行時の `sandbox_extension_consume failed` ログは file URL を扱う際の OS のメッセージで、動作には影響しない。
- フォルダの Drop は未対応（確認画面で除外理由を表示）。
- macOS 27 では strip 済み proc-macro dylib を dyld が拒否するため `[profile.release.build-override] strip = false`。
- Lambda のクロスビルドは `CARGO_TARGET_DIR=target/lambda-build`（ホストの release 成果物との衝突回避）。

## 判断ログ（ADR 化しない小さなもの）

- Web Client はビルドなし ES Modules（npm 依存ゼロ）。proto との整合は実サーバー相手の Playwright E2E で担保（ADR-0015 §1）。
- Web Push は空 Payload（暗号化不要・内容が push service に渡らない）。`/api/push/config` は無効時も 200 + null。
- Push Subscription の上限超過時の「古い順」はマイクロ秒の登録時刻で決める（秒・ミリ秒では同着が出た）。
- `e2e/__pycache__` の追跡をやめ .gitignore に追加。

- ed25519-dalek は 2.x（3.0 はリリース直後で rand 0.8 系との互換を優先）。
- UI はビルド工程なしの素の HTML/CSS/JS。受信内容は textContent のみで表示（XSS 防止）。
- 通知には内容本文を出さず種類とサイズのみ（ロック画面等での露出を避ける）。
- ローカル開発サーバーは本番と同じ URL 構造。`--blob-delay-ms` で低速回線を模擬（E2E で「転送中」を確実に捉えるため）。
- スクリーンショットはアプリのウィンドウのみ撮る（`e2e/winshot.swift`）。画面全体は他アプリの内容が写り込むため撮らない。
