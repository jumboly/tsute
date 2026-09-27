# 進捗

最終更新: 2026-09-27

## 現在地

Phase 1（macOS）実装中。**ローカル開発サーバー相手の同一 Mac 2 Endpoint E2E は 14/14 PASS**。
**実 AWS へのデプロイと AWS 経由の E2E が未実施**（AWS 認証・アカウント確認待ち）。

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

1. **[ユーザー待ち]** AWS アカウント・リージョン確認と `aws login` → `infra/bootstrap.sh` → `infra/deploy.sh test`
2. AWS 経由 E2E: `python3 e2e/run_e2e.py --target cloud --env test`（chunk size/並列数のベンチも）
3. 手動確認: OS 通知の許可と表示（ad-hoc 署名の .app では自動許可されず granted=false だった）、
   メニューバーのクリック操作、Finder からの実ドラッグ&ドロップ
4. GitHub リポジトリ作成（ユーザー確認が必要）→ CI 実行
5. Phase 1 完了報告

## .app 統合チェック結果（2026-09-27, e2e/out/app-checks.json）

- Keychain: 保存 → 再起動後に許可ダイアログなしで読み出し・再認証 → 削除 OK
- ログイン項目（SMAppService.mainApp）: 登録で enabled → 解除で not_registered（テスト後に解除済み）
- Idle（ウィンドウを閉じた状態）: CPU 0.0%、RSS 約 101MB（ウィンドウ表示中 約 110MB + WebContent プロセス）
- 通知: authorization granted=false（要手動許可・表示確認）

## 既知の問題 / 注意

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

- ed25519-dalek は 2.x（3.0 はリリース直後で rand 0.8 系との互換を優先）。
- UI はビルド工程なしの素の HTML/CSS/JS。受信内容は textContent のみで表示（XSS 防止）。
- 通知には内容本文を出さず種類とサイズのみ（ロック画面等での露出を避ける）。
- ローカル開発サーバーは本番と同じ URL 構造。`--blob-delay-ms` で低速回線を模擬（E2E で「転送中」を確実に捉えるため）。
- スクリーンショットはアプリのウィンドウのみ撮る（`e2e/winshot.swift`）。画面全体は他アプリの内容が写り込むため撮らない。
