# ADR-0014: 受信の知らせ方 — メニューバーアイコンの未確認印（OS 通知の代替）

- 状態: Accepted（2026-09-27）

## 背景

- ad-hoc 署名の .app は UNUserNotificationCenter に登録されず、通知許可が
  `UNErrorDomain code=1 (Notifications are not allowed for this application)` で即拒否される（実機で確認）。
- 通知を確実に使うには Developer ID 署名（Apple Developer Program 年 $99）が必要。ユーザー判断で当面は使わない。

## 決定

- 未確認の受信がある間、メニューバーアイコンを右上に丸印の付いたテンプレート画像（`tray-unread.png`）にする。
  メニューの状態行とツールチップに「未確認の受信 N 件」を表示する。
- 未確認とみなす条件: 受信完了時にウィンドウが「表示中かつ前面」でないこと。
- 印を消す条件: ウィンドウを開いた（Open / 最近の受信 / Reopen）、またはウィンドウが前面になった（Focused）。
- 未確認件数はプロセス内だけで保持する（再起動で消える）。常駐アプリの一時的な「気づき」用途で、
  永続化するほどの価値がないため。
- OS 通知のコードは残す。Developer ID 等で署名すれば通知も併用される（署名が有効かは起動時ログの
  `notification authorization granted=...` で分かる）。
- 検証: E2E の「ウィンドウを閉じても常駐・受信継続」ステップで、印が付き・開くと消えることを確認。
