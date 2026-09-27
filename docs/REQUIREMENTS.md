# プロダクト要件（要約）と完了条件

原文はユーザーから与えられたプロンプト（2026-09-27）。ここではセッション復元用に要点を保持する。

## 目的

Windows / macOS 上の自分の Endpoint 間で、拠点・ネットワークを問わず Clipboard やファイルを
**明示的な操作で**受け渡す個人用アプリ「つて」。同期アプリではない。

**最重要原則: ユーザーが明示的に送ると決めたデータだけを共有する。**
Clipboard 常時監視・自動 Clipboard 同期・自動ファイル同期はしない。

## Endpoint / 認証

- Endpoint = 永続的な「アプリプロファイル」（例: MacBook/default, MacBook/test-a）。物理端末ではない。
- 同一マシンで複数 Endpoint を同時起動できる（`--profile test-a` 等）。
- User → Endpoint → WebSocket Connection（永続 Endpoint と一時 Connection を分離）。
- 一般 Sign Up なし。登録は一回限り・短時間有効の Enrollment Key。
- Enrollment Key 発行はクラウド基盤の強い管理者認証（AWS IAM）で保護。アプリ独自の管理者パスワードは作らない。
- Endpoint 固有鍵を生成し、秘密鍵は OS の Credential Storage。固定の共通秘密鍵をアプリに埋め込まない。

## Clipboard 送信

1. 他アプリでコピー → 2. つてをアクティブ → 3. 送信先選択 → 4. Send Clipboard →
5. その時点で Clipboard を読む → 6. プレビュー → 7. Send → 8. 転送開始。
- アクティブ化だけでは読まない。Send Clipboard 押下でも送らない（必ず確認画面）。
- MVP: Text（UTF-8, 内容とサイズ）/ Image（プレビュー, 形式・幅・高さ・サイズ）/ Video（形式・サイズ・可能なら解像度/長さ）。
- OS の複数表現は Text / Image / Video に正規化。
- 受信しただけで OS Clipboard を上書きしない。ユーザー操作で反映。Image/Video は「ファイルとして保存」も可。

## File 送信

- Finder/Explorer から D&D（単一・複数）。Drop は候補選択のみ。
- 確認画面: ファイル名・パス・各サイズ・ファイル数・合計サイズ・送信先。Send で初めて開始。

## 通信

- HTTPS: 認証・Endpoint 一覧・Transfer 作成・Metadata・Presigned URL 等の control-plane。
- WebSocket: 通知のみ（online/offline, clipboard ready, chunk ready, state change）。巨大 payload 禁止。
- Binary: Object Storage。小さい Text は HTTP に inline 可（上限は公式制限に十分な余裕）。

## 大容量転送

- S3 Multipart を論理単位にしない。アプリ側 chunk を独立 Object として upload。
- 送信中に受信側がアップロード済み chunk を取得開始できる（overlap）。受信側オフライン時は保持。
- 並列 up/down・Resume・chunk 状態管理・checksum・最終検証・cleanup・期限・retry・一部再取得・再起動後再開。
- 受信側は可能なら offset 直書き（再結合で二重にディスクを使わない）。

## Cloud / URL / DNS / Blog / CI

- 常時稼働サーバーを避けた低コスト Serverless（AWS 第一候補）。WebSocket の timeout/keepalive/reconnect を正しく扱う。
- 一時データは完了時削除 + Lifecycle 等で安全側自動削除。機微情報をログに出さない。
- `https://<APP_BASE_URL>/`=Blog, `/api/*`=HTTP API, `wss://<APP_BASE_URL>/ws`=WebSocket。
  APP_BASE_URL は環境設定（Prod/Test/Dev）。ソースにハードコードしない。DNS 事業者に依存しない。
- 外部 DNS 操作はユーザーが手動。必要レコードを提示。
- Blog は別リポジトリ（Astro 等）。Blog 更新で Backend を再デプロイしない。共有インフラの所有責務を明確に。
- GitHub Actions。OIDC + 最小権限 Role。非秘密は Variables、秘密のみ Secrets。

## Desktop 常駐（追加要件）

- macOS: Menu Bar 常駐 / Windows: Notification Area 常駐。ログイン時自動起動可。
- 自動起動時はウィンドウを出さずバックグラウンドで認証・WebSocket 接続。Idle 時 CPU/Memory 小。
- Tray から明示操作でウィンドウ表示。Close は終了ではない（hide/破棄、バックグラウンド継続）。Quit を Tray に用意。
- Tray: Open / 接続状態 / 最近の受信 / Settings / Quit。
- 複数 Profile 同時起動時も Tray で判別でき、Credential・Local State・Transfer State が衝突しない。
- 優先順位: 1.OS に自然な常駐 2.軽量 3.Clipboard/D&D/通知/Credential の安定統合 4.Core 共有 5.UI 共通化。

## テスト容易性

- 同一 Mac で 2 Endpoint を同時起動し A → Cloud → B の実通信を確認。Mock だけで完了扱いにしない。
- 実 OS Clipboard を可能な範囲で使う。Text/Image/Video の再現可能な Fixture。
- File: 小/大/複数/Upload 中断/Download 中断/再起動/chunk 再取得/checksum failure。

## Phase 1（macOS）完了条件

**同じ Mac 上で 2 つの独立 Endpoint を起動し、実際のアプリ操作で Clipboard およびファイルを
一方から他方へクラウド経由で転送できること。**

完了後に報告（実装内容・Architecture・採用技術・ADR 一覧・Test と結果・未解決事項・Windows 対応時の注意点・
Mac での確認手順・必要な外部 DNS 設定）して **停止**。Phase 2（Windows）はユーザーの実機確認後。

## Web / PWA Client（追加要件, 2026-09-27）

原文: `docs/requirements/web-pwa.md` / 技術方針: ADR-0015（Accepted）。実装: `web/`（`/app/` で配信）。

- Native の代替ではない。主用途: スマホ → PC へ Text（特に **OS キーボードの音声入力**）、Browser の Text/Image 送信、
  未インストール環境からの一時利用、PWA として素早く起動。
- Endpoint = Browser / PWA インスタンス。既存の Enrollment Key で登録。Browser 内で鍵生成（非抽出 CryptoKey）、
  共通秘密を埋め込まない。Browser Data 消失時は再 Enrollment でよい。
- 配置: 同一 FQDN の `/app/`。Blog / App / Backend は path routing で独立デプロイ。FQDN は環境設定から導出。
- MVP: Enrollment、Endpoint 一覧・選択、Text 入力、Text/Image Clipboard の送受信、明示的 Copy、PWA install、
  Foreground は WebSocket、Background は Web Push、対応環境で Web Share Target。
  対象外: Video、大容量ファイル、常駐、常時 WS、iOS Share Target、独自音声認識。
- 明示的 Send（内容取得 → Preview → 送信先確認 → Send）。受信しても Clipboard に自動反映しない。
- Clipboard API が無くても Paste で主要機能が使えること。Push が無くても次回起動時に未受信を回収できること。
- WS / Push は通知であり、Transfer 状態の正は Backend。Push Payload に本文・機密を載せない。
- Capability（受信可能な Payload・到達手段）で Native / Web の差を表現し、送信側で受信可否を判定する。
- Web Security（XSS/CSRF/CSP/鍵保存/Push 所有権/リプレイ/なりすまし/認可/CORS/presigned URL/ログ）を確認する。

### Phase 計画

- Web / PWA は Phase 1（macOS）の完了条件に含めない。独立 Phase（Phase W）。
- Phase 1 完了後、**Windows（Phase 2）と Web / PWA のどちらを先にするかはユーザーが決める**。

### Web / PWA Phase 完了条件（実機確認）

Browser から Enrollment / 再起動・再 Open 後も同一 Endpoint / スマホで PWA 起動 / Text を PC へ送信 /
**スマホ実機で「OS キーボード音声入力 → Send → PC で受信」** / Text 受信 / 明示的 Copy /
対応 Browser で Image 送受信 / Foreground で WS 通知 / PWA を閉じた状態で（対応環境で）Web Push /
Offline 後に未受信を回収 / Android 等で Share Target / 未対応 Browser でも Text 入力・Paste・Send が機能。
Browser 上で動いただけでは完了としない。

## ユーザー確認が必要な事項

要件変更 / セキュリティモデル変更 / データ喪失の可能性 / 大きな継続課金 / 新たな契約・Credential /
外部 DNS 操作 / Phase 1 → 2 移行。
