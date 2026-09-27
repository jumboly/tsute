# ADR-0001: Desktop framework — Tauri 2 + Rust core + OS ネイティブ API

- 状態: Accepted（2026-09-27）

## 背景

優先順位は「1.OS に自然な常駐 2.軽量 3.Clipboard/D&D/通知/Credential の安定統合 4.Core 共有 5.UI 共通化」。
Cross-platform であること自体は目的ではない。

## 検討

| 候補 | 評価 |
|---|---|
| Tauri 2 (2.12, 2026-09 時点の安定版。3.0 は alpha) | Rust で Core を共有。Tray/Accessory(Dock 非表示)/Reopen を標準提供。WebView は OS 付属で配布物が小さい。**WebView はウィンドウを閉じたら破棄でき、Idle 時は Rust プロセスのみ** |
| Electron | Chromium 同梱で常駐時のメモリが大きい（優先度 2 に反する） |
| 純ネイティブ (Swift/AppKit + WinUI) | 最もネイティブだが UI を 2 回書く。Core は Rust で共有できるが FFI 境界が増える |

## PoC 結果（2026-09-27, macOS 27.0 / arm64）

- `--profile test-a` / `--profile test-b` で 2 プロセス同時起動 → メニューバーに 2 つのアイコンが並び、
  各アイコン横に profile 名が表示されることをスクリーンショットで確認。
- `ActivationPolicy::Accessory` で Dock に出ない常駐アプリになる。
- ウィンドウ未生成時の RSS はデバッグビルドで約 83MB/プロセス（WebContent プロセスなし）。

## 決定

- Tauri 2.12 を採用。UI はビルド工程なしの素の HTML/CSS/JS（依存と攻撃面を減らすため）。
- Close では `WebviewWindow::destroy()` し WebView/WebContent プロセスを解放。Tray / Reopen で再生成。
- Clipboard（特に Video / file URL / 複数表現）、通知、Login Item、Keychain は Tauri プラグインに頼らず
  objc2 系 crate / security-framework で macOS API を直接呼ぶ（抽象化の不足を回避し挙動を把握するため）。
- 認証・転送・再開ロジックは `tsute-client-core`（OS 非依存）に置き、Windows 版で再利用する。
