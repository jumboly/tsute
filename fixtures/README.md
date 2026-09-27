# テスト用 fixture

すべて合成データ（個人の画面や写真を含まない）で、再生成可能。

| ファイル | 生成方法 |
|---|---|
| `video-320x240-2s.mov` | `swift scripts/make-video-fixture.swift fixtures/video-320x240-2s.mov 2 320 240`（H.264, 15fps） |
| `image-64x48.png` | `scripts/gen_icons.py` の PNG writer で生成したグラデーション（64x48, RGBA） |
| テキスト | テスト内で生成（ASCII / 日本語 / 絵文字 / 大きいテキスト） |
