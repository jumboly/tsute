// 送信 Payload の取得と正規化（ADR-0015 §7）。
//
// Browser の Clipboard は複数表現を持ちうるので、Native と同じく Text / Image に正規化する。
// - Image は PNG を基本にする（Native 受信側が PNG を前提に Clipboard へ書くため）。他形式は canvas で PNG 化。
// - Text は text/plain。
// どの経路でも「取得しただけ」で送らない。呼び出し側が Preview → 送信先確認 → Send を必ず経る。

export const MAX_IMAGE_BYTES = 32 * 1024 * 1024;

export const clipboardReadSupported = () => !!(navigator.clipboard?.read || navigator.clipboard?.readText);

export function textPayload(text) {
  return { kind: "text", text, bytes: new TextEncoder().encode(text).length };
}

async function dims(blob) {
  const bmp = await createImageBitmap(blob);
  return { width: bmp.width, height: bmp.height, bmp };
}

/** 画像を PNG の Payload にする。source は元の形式（表示用） */
export async function imagePayload(blob) {
  if (blob.size > MAX_IMAGE_BYTES) throw new Error("画像が大きすぎます（32MB まで）");
  const { width, height, bmp } = await dims(blob);
  let png = blob;
  if (blob.type !== "image/png") {
    const canvas = document.createElement("canvas");
    canvas.width = width;
    canvas.height = height;
    canvas.getContext("2d").drawImage(bmp, 0, 0);
    png = await new Promise((res, rej) => canvas.toBlob((b) => (b ? res(b) : rej(new Error("PNG 変換に失敗しました"))), "image/png"));
  }
  bmp.close();
  return { kind: "image", blob: png, mime: "image/png", source: blob.type || "unknown", width, height, bytes: png.size };
}

/**
 * 「Clipboard から読み込む」ボタン。ユーザー操作の中で呼ぶこと（Safari / Firefox は User Activation 必須、
 * Safari・Firefox は「ペースト」確認 UI を出す）。
 */
export async function readClipboard() {
  if (navigator.clipboard?.read) {
    let items;
    try {
      items = await navigator.clipboard.read();
    } catch (e) {
      // read() は拒否されても readText() なら通る Browser がある
      if (!navigator.clipboard.readText) throw e;
      items = null;
    }
    if (items) {
      for (const item of items) {
        const img = item.types.find((t) => t === "image/png") ?? item.types.find((t) => t.startsWith("image/"));
        if (img) return imagePayload(await item.getType(img));
      }
      for (const item of items) {
        if (item.types.includes("text/plain")) {
          return textPayload(await (await item.getType("text/plain")).text());
        }
      }
      return null;
    }
  }
  const t = await navigator.clipboard.readText();
  return t ? textPayload(t) : null;
}

/** Paste イベント（Clipboard API が無い環境でも使える経路）。画像なら Payload、Text は既定動作に任せて null */
export async function imageFromPaste(e) {
  const files = [...(e.clipboardData?.files ?? [])].filter((f) => f.type.startsWith("image/"));
  if (files.length) {
    e.preventDefault();
    return imagePayload(files[0]);
  }
  return null;
}

export async function copyText(text) {
  if (navigator.clipboard?.writeText) return navigator.clipboard.writeText(text);
  // 古い Browser 用。ユーザー操作の中で呼ばれる前提
  const ta = Object.assign(document.createElement("textarea"), { value: text });
  ta.setAttribute("readonly", "");
  ta.style.position = "fixed";
  ta.style.opacity = "0";
  document.body.append(ta);
  ta.select();
  const ok = document.execCommand("copy");
  ta.remove();
  if (!ok) throw new Error("copy failed");
}

export const imageCopySupported = () => !!(navigator.clipboard?.write && globalThis.ClipboardItem);

export async function copyImage(blob) {
  const png = blob.type === "image/png" ? blob : (await imagePayload(blob)).blob;
  await navigator.clipboard.write([new ClipboardItem({ "image/png": png })]);
}

export function formatBytes(n) {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}
