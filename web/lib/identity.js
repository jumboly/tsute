// Web Endpoint の身元（Endpoint ID と Ed25519 鍵）の保持。ADR-0015 §2。
//
// 秘密鍵は extractable: false の CryptoKey のまま IndexedDB に structured clone で保存する。
// JS から鍵のバイト列を読めないため、XSS があっても鍵そのものは持ち出せない（署名はされうるが、
// トークンと同じく「そのページが開いている間」に限られる）。
// Browser Data の削除で消えたら再 Enrollment する設計で、Keychain 同等の永続性は前提にしない。

const DB_NAME = "tsute";
const STORE = "identity";
const KEY = "self";

function openDb() {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, 1);
    req.onupgradeneeded = () => req.result.createObjectStore(STORE);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

async function tx(mode, fn) {
  const db = await openDb();
  try {
    return await new Promise((resolve, reject) => {
      const t = db.transaction(STORE, mode);
      const r = fn(t.objectStore(STORE));
      t.oncomplete = () => resolve(r?.result);
      t.onerror = () => reject(t.error);
      t.onabort = () => reject(t.error);
    });
  } finally {
    db.close();
  }
}

export function b64url(bytes) {
  let s = "";
  for (const b of new Uint8Array(bytes)) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

export function b64urlDecode(s) {
  const t = s.replace(/-/g, "+").replace(/_/g, "/");
  const bin = atob(t + "=".repeat((4 - (t.length % 4)) % 4));
  return Uint8Array.from(bin, (c) => c.charCodeAt(0));
}

/** Web Crypto の Ed25519 が使えるか（Chrome 137 / Firefox 129 / Safari 17 以降） */
export async function ed25519Supported() {
  if (!globalThis.crypto?.subtle) return false;
  try {
    await crypto.subtle.generateKey({ name: "Ed25519" }, false, ["sign", "verify"]);
    return true;
  } catch {
    return false;
  }
}

/** 新しい鍵ペア。公開鍵は Backend へ登録するため raw（32 バイト）で取り出す */
export async function generateKeyPair() {
  const kp = await crypto.subtle.generateKey({ name: "Ed25519" }, false, ["sign", "verify"]);
  const raw = await crypto.subtle.exportKey("raw", kp.publicKey);
  return { privateKey: kp.privateKey, publicKey: b64url(raw) };
}

export async function sign(privateKey, bytes) {
  return b64url(await crypto.subtle.sign({ name: "Ed25519" }, privateKey, bytes));
}

/** @returns {Promise<{endpointId: string, name: string, privateKey: CryptoKey, createdAt: number} | undefined>} */
export async function load() {
  return tx("readonly", (s) => s.get(KEY));
}

export async function save(identity) {
  await tx("readwrite", (s) => s.put(identity, KEY));
}

export async function clear() {
  await tx("readwrite", (s) => s.delete(KEY));
}

/** 鍵が Storage 退避で消えにくくなるよう永続化を要求する（許可は Browser 次第） */
export async function requestPersist() {
  try {
    if (await navigator.storage?.persisted?.()) return true;
    return (await navigator.storage?.persist?.()) ?? false;
  } catch {
    return false;
  }
}
