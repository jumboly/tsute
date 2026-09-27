# ADR-0013: Desktop E2E — アプリ内オートメーション経路

- 状態: Accepted（2026-09-27）

## 背景

- macOS では tauri-driver（WebDriver）が非対応。
- System Events による UI 操作には Accessibility 権限が必要で、この開発環境には付与されていない（-1719）。

## 決定

- `--automation` フラグ **かつ** 環境変数 `TSUTE_AUTOMATION=1` のときだけ、プロファイル内に
  Unix ドメインソケット（0600）を開き、JSON 行で `show` / `hide` / `eval` / `quit` を受け付ける。
- `eval` は WebView 内で JS を実行し、**実際の DOM 要素（ボタン等）をクリック**する。
  したがって UI → Tauri コマンド → client-core → Cloud → 相手アプリ という本番と同じ経路を通る。
- Clipboard は実 OS Clipboard（pbcopy / osascript / NSPasteboard）を使う。
- 自動化できない部分と扱い:
  - Finder からの**ドラッグ操作そのもの**: Drop イベントのハンドラ（`handleDrop(paths)`）以降を自動化。
    OS からの Drop イベント配送は手動確認項目とする。
  - メニューバーのクリック・OS 通知の表示/クリック・ログイン項目の実ログイン: 手動確認項目。
- ドライバは `e2e/driver.py`、シナリオは `e2e/run_e2e.py`（local / cloud 両対応）。
