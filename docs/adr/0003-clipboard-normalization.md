# ADR-0003: Clipboard 形式の正規化（macOS）

- 状態: Accepted（2026-09-27）。Windows の形式対応は ADR-0016。

## 背景

NSPasteboard は 1 つのコピーに複数表現（UTI）を持つ。例: Finder でファイルをコピーすると `public.file-url` と
**ファイルアイコンの `public.tiff`** が同時に載る。Excel/Office は表のテキストと PNG 画像を同時に載せる。

## 決定（送信時の読み取り）

Send Clipboard 押下時に一度だけ読み、以下の候補を列挙してプレビューに出す。

1. `public.file-url`（複数可）
   - 1 件かつ UTI が `public.movie` 準拠 → **Video**（ファイルとして Transfer Engine で送信）
   - 1 件かつ `public.image` 準拠 → **Image**（元ファイルの形式のまま送信）
   - それ以外 → **Files**（File 送信と同じ確認画面）
   - file URL がある場合、同時に載っている TIFF はアイコンなので無視する
2. 動画の実データ（`public.mpeg-4` / `com.apple.quicktime-movie` 等 `public.movie` 準拠 UTI）→ 一時ファイルに書き出し **Video**
3. テキスト（`public.utf8-plain-text`）→ **Text**
4. 画像データ（`public.png` 優先、なければ `public.tiff` 等を PNG に変換）→ **Image (PNG)**

テキストと画像が両方ある場合（Office 等）は Text を既定にし、プレビュー画面で Image に切り替え可能にする。

## 決定（受信時の反映）

受信しただけでは OS Clipboard を変更しない。ユーザーが「Clipboard にコピー」を押した時だけ書き込む。

- Text: `NSPasteboardTypeString`
- Image: PNG と TIFF の両方（TIFF しか受け付けない古いアプリがあるため）
- Video / Files: 受信済みファイルの file URL（Finder・メッセージ等へ貼り付け可能）
- Image/Video は「ファイルとして保存」も提供。
