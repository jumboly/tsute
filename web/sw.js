// Service Worker（scope: /app/）。ADR-0015 §5-7。
//
// - App Shell だけをキャッシュする。/api/・/ws・presigned URL（別オリジン）はキャッシュしない。
//   受信内容を Cache Storage に残さないため。
// - Push は「新しい受信がある」ヒントだけ（Payload は空）。通知文言も汎用にして、ロック画面等に内容を出さない。
//   本文は通知を開いて App が起動した後に API から取得する。Safari は Push を受けたら必ず通知を表示しないと
//   許可を取り消すので、push イベントでは常に showNotification する。
// - Share Target（Android / Chromium）の POST を受け、内容をメモリに一時保持して App に渡す。
//   永続ストレージには書かない。
// - 常駐プロセスのように扱わない（状態は短命・イベント単位で完結させる）。

const VERSION = "v2";
const SHELL = `tsute-shell-${VERSION}`;
const SHELL_FILES = [
  "./",
  "index.html",
  "style.css",
  "app.js",
  "lib/api.js",
  "lib/identity.js",
  "lib/payload.js",
  "lib/ws.js",
  "manifest.webmanifest",
  "icons/icon-192.png",
  "icons/icon-512.png",
  "icons/apple-touch-icon.png",
];

self.addEventListener("install", (e) => {
  e.waitUntil(caches.open(SHELL).then((c) => c.addAll(SHELL_FILES.map((f) => new Request(f, { cache: "reload" })))));
  self.skipWaiting();
});

self.addEventListener("activate", (e) => {
  e.waitUntil((async () => {
    for (const k of await caches.keys()) if (k !== SHELL) await caches.delete(k);
    await self.clients.claim();
  })());
});

const scopePath = new URL(self.registration.scope).pathname; // "/app/"

self.addEventListener("fetch", (e) => {
  const url = new URL(e.request.url);
  if (url.origin !== location.origin || !url.pathname.startsWith(scopePath)) return;
  if (e.request.method === "POST" && url.pathname === `${scopePath}share`) {
    e.respondWith(receiveShare(e));
    return;
  }
  if (e.request.method !== "GET") return;
  // App Shell はネットワーク優先（デプロイ後すぐ新しい版を使う）、オフライン時だけキャッシュ
  e.respondWith((async () => {
    try {
      const r = await fetch(e.request);
      if (r.ok && r.type === "basic") {
        const c = await caches.open(SHELL);
        await c.put(e.request, r.clone());
      }
      return r;
    } catch {
      const hit = await caches.match(e.request, { ignoreSearch: e.request.mode === "navigate" });
      return hit ?? (e.request.mode === "navigate" ? caches.match("index.html") : Response.error());
    }
  })());
});

// ---------- Share Target ----------

/** id → { data, resolve }。App が取りに来るまで（最大 60 秒）だけメモリに持つ */
const shares = new Map();

async function receiveShare(e) {
  const form = await e.request.formData();
  const data = {
    title: form.get("title") || "",
    text: form.get("text") || "",
    url: form.get("url") || "",
    files: form.getAll("files").filter((f) => f instanceof File),
  };
  const id = crypto.randomUUID();
  let done;
  const taken = new Promise((r) => (done = r));
  shares.set(id, { data, done });
  // App が受け取るまで SW を生かしておく（受け取られなければ破棄）
  e.waitUntil(Promise.race([taken, new Promise((r) => setTimeout(r, 60_000))]).then(() => shares.delete(id)));
  return Response.redirect(`${scopePath}?share=${id}`, 303);
}

self.addEventListener("message", (e) => {
  if (e.data?.type !== "take-share") return;
  const s = shares.get(e.data.id);
  e.ports[0]?.postMessage(s ? s.data : null);
  s?.done();
});

// ---------- Web Push ----------

self.addEventListener("push", (e) => {
  e.waitUntil((async () => {
    // サーバーは WS で届かなかったときだけ Push する。前面の App があれば再同期させたうえで、
    // Safari の「Push ごとに通知を表示する」要件を満たすため通知は常に出す
    const wins = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
    for (const w of wins) w.postMessage({ type: "resync" });
    // 件数は Payload が空なので分からない。印だけ付け、App を開いたときに正確な件数へ置き換える
    try { await navigator.setAppBadge?.(); } catch { /* 未対応 */ }
    await self.registration.showNotification("つて", {
      body: "新しい受信があります",
      tag: "tsute-transfer",
      icon: "icons/icon-192.png",
      badge: "icons/icon-192.png",
      renotify: true,
    });
  })());
});

self.addEventListener("notificationclick", (e) => {
  e.notification.close();
  e.waitUntil((async () => {
    const wins = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
    const w = wins.find((c) => new URL(c.url).pathname.startsWith(scopePath));
    if (w) {
      await w.focus();
      w.postMessage({ type: "resync" });
    } else {
      await self.clients.openWindow(scopePath);
    }
  })());
});
