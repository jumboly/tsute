# ADR-0004: 転送方式・chunk size・並列数・checksum

- 状態: Accepted（2026-09-27）

## 決定

- **論理単位**: Transfer（= 1 回の送信）> File > Chunk。Chunk を S3 の独立 Object
  `transfers/<transfer_id>/<file>/<index>` として presigned PUT。S3 Multipart は使わない
  （Multipart は Complete までオブジェクトが読めず、Upload/Download の overlap ができないため）。
- 受信側は WebSocket `chunks_ready` 通知（取りこぼし時は 20 秒ポーリング）で、アップロード済み chunk から取得開始。
- **chunk size = 8 MiB（既定）**。範囲 256KiB–64MiB を API で許可。
  - S3 PUT は $0.005/1000 req。8MiB なら 1GB で 128 PUT ≈ $0.00064。1MiB だとリクエスト数と API/Lambda 呼び出しが 8 倍。
  - 失敗時の再送単位として 8MiB は低速回線（10Mbps で約 7 秒）でも許容範囲。
  - メモリ: 並列 4 × 8MiB ≈ 32MiB/方向。常駐アプリとして許容。
- **並列数 = upload 4 / download 4**。単一 TCP ストリームの帯域上限を超えるには数本の並列が有効な一方、
  それ以上は家庭回線で効果が薄くメモリを使うため。（実 AWS でのベンチ結果は PROGRESS.md に追記予定）
- **checksum**:
  - chunk: SHA-256。presigned PUT の署名に `x-amz-checksum-sha256` を含め、S3 が内容を検証（不一致は 400 BadDigest）。
    受信側もダウンロード後に再計算し、不一致なら書き込まずに chunk 単位で再取得。
  - サーバーは `chunks` 完了報告時に HeadObject でサイズ・checksum を照合してから完了扱いにする。
  - file: SHA-256（送信側がチャンク送信と並行して計算し finalize で報告）。受信側は組み立て後に再計算して最終検証。
    SHA-256 を選んだ理由: S3 がネイティブに検証でき、追加依存なしで両 OS に高速実装がある。
- **Text inline 上限 = 64 KiB（UTF-8）**。HTTP API payload 10MB / Lambda 6MB / DynamoDB item 400KB に対し、
  JSON エスケープで最大 6 倍になっても十分余裕がある値。超える場合はファイルとして Transfer Engine で送る。
- **Image** は PNG ファイルとして Transfer Engine（1 chunk になることが多い）で送る。
- **1 Transfer 上限 50GiB / 1000 ファイル**: 誤操作による大量課金防止。
- **期限**: Transfer 7 日（DynamoDB TTL）。S3 Lifecycle で `transfers/` を 8 日で削除（完了・取消時は即削除）。
- **受信側書き込み**: 最終サイズで確保した隠し part ファイルに offset 指定（pwrite）で書き、検証後 rename。
  再結合のための二重ディスク消費なし。書き込み後 `fsync` してから受信済みとして記録。
