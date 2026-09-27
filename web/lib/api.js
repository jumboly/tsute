// control-plane HTTP API と chunk 転送（crates/proto と crates/client-core に対応する Browser 実装）。
//
// - URL は location.origin 相対で導出する。APP_BASE_URL を成果物に埋め込まないため、同じ成果物を
//   Test / Prod に配れる（ADR-0015 §1）。
// - アクセストークンはメモリのみ（localStorage に置くと XSS で持ち出されるため）。起動のたびに取り直す。
// - Cookie を使わず Authorization: Bearer だけで認証する。ambient credential が無いので CSRF が成立しない。
// - presigned URL・トークン・内容はログ（console）に出さない。

import { sign } from "./identity.js";

export const INLINE_TEXT_MAX_BYTES = 64 * 1024;
export const DEFAULT_CHUNK_SIZE = 8 * 1024 * 1024;
/** Web は受信内容をメモリに載せて表示するため、Browser が扱える大きさに制限する */
export const WEB_MAX_RECEIVE_BYTES = 64 * 1024 * 1024;
/** MVP で受信できる Payload（ADR-0015 §4）。Video / Files は Native のみ */
export const WEB_ACCEPTS = ["clipboard_text", "clipboard_image"];

export class ApiError extends Error {
  constructor(status, code, message) {
    super(message || code || `HTTP ${status}`);
    this.status = status;
    this.code = code;
  }
}

async function readJson(resp) {
  const text = await resp.text();
  let body = null;
  try { body = text ? JSON.parse(text) : null; } catch { /* 非 JSON（CloudFront のエラーページ等） */ }
  if (!resp.ok) throw new ApiError(resp.status, body?.error ?? "", body?.message ?? "");
  return body;
}

async function post(path, body) {
  const r = await fetch(path, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
    cache: "no-store",
  });
  return readJson(r);
}

export function detectPlatform() {
  const p = (navigator.userAgentData?.platform || navigator.platform || "").toLowerCase();
  const ua = navigator.userAgent;
  // iPadOS は Mac を名乗るため、タッチ対応で区別する
  if (/iphone|ipad|ipod/i.test(ua) || (p.includes("mac") && navigator.maxTouchPoints > 1)) return "other";
  if (p.includes("mac")) return "macos";
  if (p.includes("win")) return "windows";
  return "other";
}

/** 登録名の初期値（例: "iPhone / Safari"）。ユーザーが編集できる */
export function suggestName() {
  const ua = navigator.userAgent;
  const device = /iphone/i.test(ua) ? "iPhone"
    : /ipad/i.test(ua) || (/macintosh/i.test(ua) && navigator.maxTouchPoints > 1) ? "iPad"
    : /android/i.test(ua) ? "Android"
    : /macintosh/i.test(ua) ? "Mac"
    : /windows/i.test(ua) ? "Windows"
    : /linux/i.test(ua) ? "Linux" : "Browser";
  const browser = /edg\//i.test(ua) ? "Edge"
    : /firefox|fxios/i.test(ua) ? "Firefox"
    : /chrome|crios/i.test(ua) ? "Chrome"
    : /safari/i.test(ua) ? "Safari" : "Browser";
  const standalone = matchMedia("(display-mode: standalone)").matches || navigator.standalone;
  return `${device} / ${standalone ? "PWA" : browser}`;
}

export async function enroll({ enrollmentKey, name, publicKey }) {
  const r = await post("/api/enroll", {
    enrollment_key: enrollmentKey.trim(),
    name,
    platform: detectPlatform(),
    public_key: publicKey,
    client_kind: "web",
    accepts: WEB_ACCEPTS,
  });
  return r.endpoint_id;
}

function b64std(bytes) {
  let s = "";
  const u = new Uint8Array(bytes);
  for (let i = 0; i < u.length; i += 0x8000) s += String.fromCharCode(...u.subarray(i, i + 0x8000));
  return btoa(s);
}

export async function sha256b64(data) {
  return b64std(await crypto.subtle.digest("SHA-256", data));
}

function chunkCount(size, chunkSize) {
  return size === 0 ? 1 : Math.ceil(size / chunkSize);
}

export class Api {
  /** @param {{endpointId: string, privateKey: CryptoKey}} identity */
  constructor(identity) {
    this.identity = identity;
    this.token = null;
    this.tokenExp = 0;
  }

  async authenticate() {
    const id = this.identity.endpointId;
    const ch = await post("/api/auth/challenge", { endpoint_id: id });
    // proto::auth_signing_message と同じドメイン分離付きメッセージ
    const msg = new TextEncoder().encode(`tsute-auth-v1\n${id}\n${ch.nonce}`);
    const signature = await sign(this.identity.privateKey, msg);
    const t = await post("/api/auth/token", { endpoint_id: id, nonce: ch.nonce, signature });
    this.token = t.access_token;
    this.tokenExp = t.expires_at;
  }

  async request(method, path, body, retry = true) {
    // 期限の少し前に更新し、期限切れの 401 往復を避ける
    if (!this.token || this.tokenExp - Date.now() / 1000 < 60) await this.authenticate();
    const r = await fetch(path, {
      method,
      headers: {
        authorization: `Bearer ${this.token}`,
        ...(body !== undefined ? { "content-type": "application/json" } : {}),
      },
      body: body !== undefined ? JSON.stringify(body) : undefined,
      cache: "no-store",
    });
    if (r.status === 401 && retry) {
      this.token = null;
      return this.request(method, path, body, false);
    }
    return readJson(r);
  }

  me() { return this.request("GET", "/api/me"); }
  endpoints() { return this.request("GET", "/api/endpoints").then((r) => r.endpoints); }
  rename(name) { return this.request("PUT", "/api/me/name", { name }); }
  setCapabilities(accepts) { return this.request("PUT", "/api/me/capabilities", { accepts }); }
  transfers() { return this.request("GET", "/api/transfers").then((r) => r.transfers); }
  transfer(id) { return this.request("GET", `/api/transfers/${encodeURIComponent(id)}`); }
  received(id) { return this.request("POST", `/api/transfers/${encodeURIComponent(id)}/received`, {}); }
  cancel(id) { return this.request("DELETE", `/api/transfers/${encodeURIComponent(id)}`); }
  wsTicket() { return this.request("POST", "/api/ws-ticket", {}).then((r) => r.ticket); }
  pushConfig() { return this.request("GET", "/api/push/config"); }
  putPushSubscription(endpoint) { return this.request("PUT", "/api/push/subscription", { endpoint }); }
  deletePushSubscription(endpoint) { return this.request("DELETE", "/api/push/subscription", { endpoint }); }

  /** Text を送る。64KiB を超えるものは Native と同じくファイル（clipboard.txt）として送る */
  async sendText(receiver, text, onProgress) {
    const bytes = new TextEncoder().encode(text);
    if (bytes.length <= INLINE_TEXT_MAX_BYTES) {
      return this.request("POST", "/api/transfers", { receiver, kind: "clipboard_text", text });
    }
    const blob = new Blob([bytes], { type: "text/plain; charset=utf-8" });
    return this.sendFile(receiver, "clipboard_text", blob, { name: "clipboard.txt", mime: blob.type }, onProgress);
  }

  /** 1 ファイルの転送（Image / 大きい Text）。chunk ごとに SHA-256 を署名へ含めて presigned PUT する */
  async sendFile(receiver, kind, blob, { name, mime, media = {} }, onProgress) {
    const chunkSize = DEFAULT_CHUNK_SIZE;
    const t = await this.request("POST", "/api/transfers", {
      receiver, kind, files: [{ name, size: blob.size, mime, media }], chunk_size: chunkSize,
    });
    try {
      const n = chunkCount(blob.size, chunkSize);
      let done = 0;
      for (let i = 0; i < n; i++) {
        const part = blob.slice(i * chunkSize, Math.min(blob.size, (i + 1) * chunkSize));
        const buf = await part.arrayBuffer();
        const info = { file: 0, index: i, size: buf.byteLength, sha256: await sha256b64(buf) };
        const { urls } = await this.request("POST", `/api/transfers/${t.transfer_id}/upload-urls`, { chunks: [info] });
        const u = urls[0];
        const put = await fetch(u.url, {
          method: "PUT",
          headers: Object.fromEntries(u.headers),
          body: buf,
          cache: "no-store",
          credentials: "omit",
        });
        if (!put.ok) throw new ApiError(put.status, "upload_failed", `chunk upload failed (${put.status})`);
        await this.request("POST", `/api/transfers/${t.transfer_id}/chunks`, { chunks: [info] });
        done += buf.byteLength;
        onProgress?.(done, blob.size);
      }
      await this.request("POST", `/api/transfers/${t.transfer_id}/files/0/finalize`, {
        sha256: await sha256b64(await blob.arrayBuffer()),
      });
      return t;
    } catch (e) {
      // 途中で失敗した転送を受信側に残さない（Browser では再起動後の再開を持たないため）
      this.cancel(t.transfer_id).catch(() => {});
      throw e;
    }
  }

  /**
   * 受信した 1 ファイルを取得して検証する。全 chunk の SHA-256 とファイル全体の SHA-256 を照合し、
   * 改ざん・破損したデータを表示しない。
   */
  async downloadFile(transferId) {
    const d = await this.transfer(transferId);
    const f = d.transfer.files[0];
    if (!f) throw new ApiError(0, "no_file", "transfer has no file");
    if (f.size > WEB_MAX_RECEIVE_BYTES) throw new ApiError(0, "too_large", "too large for the browser client");
    const want = [...Array(f.chunk_count).keys()];
    const byIndex = new Map(d.chunks.filter((c) => c.file === 0).map((c) => [c.index, c]));
    if (want.some((i) => !byIndex.has(i))) throw new ApiError(0, "not_ready", "chunks are not ready");
    const { urls } = await this.request("POST", `/api/transfers/${transferId}/download-urls`, {
      chunks: want.map((index) => ({ file: 0, index })),
    });
    const parts = [];
    for (const u of urls.sort((a, b) => a.index - b.index)) {
      const r = await fetch(u.url, { cache: "no-store", credentials: "omit" });
      if (!r.ok) throw new ApiError(r.status, "download_failed", `chunk download failed (${r.status})`);
      const buf = await r.arrayBuffer();
      if ((await sha256b64(buf)) !== byIndex.get(u.index).sha256) {
        throw new ApiError(0, "checksum", "chunk checksum mismatch");
      }
      parts.push(buf);
    }
    const blob = new Blob(parts, { type: f.mime });
    if (f.sha256 && (await sha256b64(await blob.arrayBuffer())) !== f.sha256) {
      throw new ApiError(0, "checksum", "file checksum mismatch");
    }
    return { blob, file: f };
  }
}
