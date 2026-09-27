// つて Web / PWA Client。ビルド工程なしの ES Modules（ADR-0015 §「実装技術」）。
//
// 原則（Native と共通）:
// - ユーザーが明示的に送ると決めたものだけを送る。入力・Clipboard 読み込み・共有の受け取りだけでは送らず、
//   必ず Preview → 送信先確認 → Send を経る。
// - 受信しても Clipboard に自動反映しない。Copy / 保存 / 閉じる はユーザー操作で行う。
// - 受信内容・Endpoint 名は textContent で描画する（innerHTML を使わない。XSS 防止）。
// - 受信内容を IndexedDB / Cache Storage に保存しない。状態の正は Backend の Transfer で、
//   ユーザーが操作するまで received にしないため、再読み込みしても未処理の受信は消えない。

import * as identity from "./lib/identity.js";
import { Api, ApiError, WEB_ACCEPTS, enroll, suggestName } from "./lib/api.js";
import { Realtime } from "./lib/ws.js";
import {
  clipboardReadSupported, copyImage, copyText, formatBytes, imageCopySupported, imageFromPaste, imagePayload,
  readClipboard, textPayload,
} from "./lib/payload.js";

const $ = (id) => document.getElementById(id);
const KIND = { clipboard_text: "テキスト", clipboard_image: "画像", clipboard_video: "動画", files: "ファイル" };
const STATE = { uploading: "転送中", uploaded: "受信待ち", received: "受信済み", cancelled: "取消" };

/** @type {Api} */
let api;
let me = null;
let endpoints = [];
let realtime = null;
let imageDraft = null; // 送信候補の画像 Payload（Text より優先）
/** 表示中の受信: transfer_id → { transfer, status, text?, blob?, url?, acted } */
const inbox = new Map();
let swReg = null;
let inboxLoaded = false; // 起動直後の初回取得か（新着の知らせ方を変える）

function el(tag, attrs = {}, ...children) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") e.className = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else if (v === true) e.setAttribute(k, "");
    else if (v !== false && v != null) e.setAttribute(k, v);
  }
  for (const c of children) if (c != null) e.append(typeof c === "string" ? document.createTextNode(c) : c);
  return e;
}

let toastTimer;
function toast(msg) {
  const t = $("toast");
  t.textContent = msg;
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), 3000);
}

function show(view) {
  document.body.dataset.view = view;
  $("open-settings").hidden = !(view === "main");
}

function errText(e) {
  if (e instanceof ApiError) {
    if (e.code === "receiver_cannot_accept") return "送信先がこの種類を受け取れません";
    if (e.code === "invalid_enrollment_key") return "Enrollment Key が無効・使用済み・期限切れです";
    if (e.status === 0) return e.message;
    return `${e.message || e.code} (HTTP ${e.status})`;
  }
  if (e instanceof TypeError) return "通信できませんでした（オフライン？）";
  return e?.message || String(e);
}

// ---------------- 起動 ----------------

async function boot() {
  if (!isSecureContext || !globalThis.indexedDB) {
    $("unsupported-reason").textContent = "安全な接続（https）ではないため、鍵を安全に保存できません。";
    return show("unsupported");
  }
  if (!(await identity.ed25519Supported())) {
    $("unsupported-reason").textContent = "この Browser は Web Crypto の Ed25519 に対応していません。";
    return show("unsupported");
  }
  if ("serviceWorker" in navigator) {
    // scope は /app/ に限定し、同じ FQDN の Blog のページを横取りしない
    swReg = await navigator.serviceWorker.register("sw.js", { scope: "./" }).catch(() => null);
  }
  const id = await identity.load().catch(() => undefined);
  if (!id) return showEnroll();
  await startMain(id);
}

function showEnroll() {
  $("me-name").textContent = "つて";
  $("enroll-name").value = suggestName();
  show("enroll");
}

$("enroll-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const btn = $("enroll-submit");
  btn.disabled = true;
  $("enroll-error").textContent = "";
  try {
    const name = $("enroll-name").value.trim();
    const kp = await identity.generateKeyPair();
    const endpointId = await enroll({ enrollmentKey: $("enroll-key").value, name, publicKey: kp.publicKey });
    const id = { endpointId, name, privateKey: kp.privateKey, createdAt: Date.now() };
    await identity.save(id);
    $("enroll-key").value = "";
    // 鍵が Storage 退避で消えにくくなるよう永続化を要求（結果は設定画面に表示）
    identity.requestPersist();
    await startMain(id);
    toast("登録しました");
  } catch (e) {
    $("enroll-error").textContent = errText(e);
  } finally {
    btn.disabled = false;
  }
});

async function startMain(id) {
  api = new Api(id);
  $("me-name").textContent = id.name;
  show("main");
  try {
    me = (await api.me()).endpoint;
  } catch (e) {
    if (e instanceof ApiError && (e.status === 401 || e.code === "auth_failed")) {
      // 鍵はあるが Backend 側で revoke された
      banner("この Browser の登録は無効になっています。設定から鍵を削除して再登録してください。");
      return;
    }
    banner(`接続できません: ${errText(e)}（再接続します）`);
    setTimeout(() => startMain(id), 5000);
    return;
  }
  banner(null);
  $("me-name").textContent = me.name;
  // 以前の版で登録した Endpoint も、現在の Web の受信能力に揃える
  if (JSON.stringify([...me.accepts].sort()) !== JSON.stringify([...WEB_ACCEPTS].sort())) {
    me = (await api.setCapabilities(WEB_ACCEPTS)).endpoint;
  }
  realtime ??= new Realtime(api, {
    onEvent,
    onResync: () => resync(),
    onState: (s) => {
      document.body.dataset.connection = s;
      $("conn-dot").title = { online: "接続中", connecting: "接続しています", offline: "未接続" }[s];
    },
  });
  realtime.start();
  await resync();
  refreshPushSubscription();
  consumeShare();
  closeNotifications();
}

function banner(msg) {
  const b = $("banner");
  b.hidden = !msg;
  b.textContent = msg ?? "";
}

document.addEventListener("visibilitychange", () => {
  if (!document.hidden && api) {
    resync();
    closeNotifications();
  }
});

// Service Worker からの再同期要求（Push 受信・通知クリック）
navigator.serviceWorker?.addEventListener("message", (m) => {
  if (m.data?.type === "resync" && api) resync();
});

function onEvent(e) {
  switch (e.type) {
    case "transfer_created":
    case "transfer_state":
      resync();
      break;
    case "endpoints_changed":
    case "presence":
      refreshEndpoints();
      break;
  }
}

let resyncing = null;
let resyncAgain = false;
/** 状態の正（HTTP API）から取り直す。WS / Push / 起動のどれが契機でも同じ処理にする */
async function resync() {
  if (resyncing) {
    resyncAgain = true;
    return resyncing;
  }
  resyncing = (async () => {
    try {
      await Promise.all([refreshEndpoints(), refreshTransfers()]);
    } catch (e) {
      if (!(e instanceof TypeError)) console.warn("resync failed:", e?.code || e?.name);
    }
  })();
  await resyncing;
  resyncing = null;
  if (resyncAgain) {
    resyncAgain = false;
    await resync();
  }
}

// ---------------- 送信先 ----------------

async function refreshEndpoints() {
  if (!api) return;
  endpoints = await api.endpoints();
  renderTargets();
}

function currentPayload() {
  if (imageDraft) return imageDraft;
  const t = $("text").value;
  return t ? textPayload(t) : null;
}

function payloadKind(p) {
  return p?.kind === "image" ? "clipboard_image" : "clipboard_text";
}

function renderTargets() {
  const ul = $("targets");
  const prev = ul.querySelector("input:checked")?.value;
  const others = endpoints.filter((e) => e.endpoint_id !== me?.endpoint_id);
  const kind = payloadKind(currentPayload());
  ul.replaceChildren(...others.map((e) => {
    const ok = e.accepts.includes(kind);
    const reach = e.reach?.includes("websocket") ? "オンライン"
      : e.reach?.includes("web_push") ? "オフライン（通知で知らせます）" : "オフライン（次に開いたときに受信）";
    const sub = ok ? `${e.client_kind === "web" ? "Web" : "Native"} · ${reach}` : `${KIND[kind]}を受け取れません`;
    return el("li", {},
      el("label", {},
        el("input", { type: "radio", name: "target", value: e.endpoint_id, disabled: !ok, checked: ok && e.endpoint_id === prev }),
        el("span", {}, e.name, el("span", { class: "sub" }, sub))));
  }));
  if (!others.length) ul.append(el("li", { class: "muted" }, "送信先がありません（他の Endpoint を登録してください）"));
  // 送信先が 1 つだけなら選んでおく（確認画面で送信先を必ず表示するので誤送信にはならない）
  const enabled = [...ul.querySelectorAll("input:not(:disabled)")];
  if (!ul.querySelector("input:checked") && enabled.length === 1) enabled[0].checked = true;
  updateReview();
}

$("targets").addEventListener("change", updateReview);

function selectedTarget() {
  const id = $("targets").querySelector("input:checked")?.value;
  return endpoints.find((e) => e.endpoint_id === id);
}

function updateReview() {
  const p = currentPayload();
  const t = selectedTarget();
  $("review").disabled = !p || !t || !t.accepts.includes(payloadKind(p));
  $("payload-meta").textContent = !p ? ""
    : p.kind === "image" ? `画像 · ${formatBytes(p.bytes)}`
    : `テキスト · ${[...p.text].length} 文字 · ${formatBytes(p.bytes)}`;
}

// ---------------- 送る内容 ----------------

$("text").addEventListener("input", () => renderTargets());

$("text").addEventListener("paste", async (e) => {
  try {
    const img = await imageFromPaste(e);
    if (img) setImage(img);
  } catch (err) {
    toast(errText(err));
  }
});

if (clipboardReadSupported()) $("read-clip").hidden = false;
$("read-clip").addEventListener("click", async () => {
  try {
    const p = await readClipboard();
    if (!p) return toast("Clipboard にテキスト・画像がありません");
    if (p.kind === "image") setImage(p);
    else {
      setImage(null);
      $("text").value = p.text;
      renderTargets();
    }
  } catch (e) {
    toast(e?.name === "NotAllowedError" ? "Clipboard の読み取りが許可されませんでした。貼り付けを使ってください" : errText(e));
  }
});

$("clear").addEventListener("click", () => {
  $("text").value = "";
  setImage(null);
});
$("image-remove").addEventListener("click", () => setImage(null));

function setImage(p) {
  if (imageDraft?.url) URL.revokeObjectURL(imageDraft.url);
  imageDraft = p;
  $("image-preview").hidden = !p;
  $("text").hidden = !!p;
  if (p) {
    p.url = URL.createObjectURL(p.blob);
    $("image-preview-img").src = p.url;
    $("image-preview-meta").textContent =
      `PNG · ${p.width}×${p.height} · ${formatBytes(p.bytes)}${p.source !== "image/png" ? `（${p.source} から変換）` : ""}`;
  } else {
    $("image-preview-img").removeAttribute("src");
  }
  renderTargets();
}

// ---------------- 確認 → Send ----------------

let pending = null;
$("review").addEventListener("click", () => {
  const p = currentPayload();
  const t = selectedTarget();
  if (!p || !t) return;
  pending = { p, t };
  $("confirm-target").textContent = t.name;
  $("confirm-kind").textContent = p.kind === "image" ? `画像（PNG ${p.width}×${p.height}）` : "テキスト";
  $("confirm-size").textContent = p.kind === "image" ? formatBytes(p.bytes) : `${[...p.text].length} 文字 · ${formatBytes(p.bytes)}`;
  $("confirm-text").hidden = p.kind !== "text";
  $("confirm-image").hidden = p.kind !== "image";
  if (p.kind === "text") $("confirm-text").textContent = p.text;
  else $("confirm-image").src = p.url;
  $("confirm").showModal();
});

$("confirm").addEventListener("close", async () => {
  const { p, t } = pending ?? {};
  pending = null;
  $("confirm-image").removeAttribute("src");
  if ($("confirm").returnValue !== "send" || !p) return;
  const btn = $("review");
  btn.disabled = true;
  try {
    if (p.kind === "image") {
      await api.sendFile(t.endpoint_id, "clipboard_image", p.blob,
        { name: "clipboard.png", mime: "image/png", media: { width: p.width, height: p.height } },
        (done, total) => toast(`送信中… ${Math.round((done / total) * 100)}%`));
      setImage(null);
    } else {
      await api.sendText(t.endpoint_id, p.text);
      $("text").value = "";
    }
    toast(`${t.name} へ送信しました`);
  } catch (e) {
    toast(`送信できませんでした: ${errText(e)}`);
  } finally {
    renderTargets();
    resync();
  }
});

// ---------------- 受信 ----------------

async function refreshTransfers() {
  if (!api || !me) return;
  const list = await api.transfers();
  const mine = me.endpoint_id;
  const incoming = list.filter((t) => t.receiver === mine && (t.state === "uploading" || t.state === "uploaded"));
  const fresh = [];
  for (const t of incoming) {
    const cur = inbox.get(t.transfer_id);
    if (!cur) {
      inbox.set(t.transfer_id, { transfer: t, status: "new", acted: false });
      fresh.push(t);
    } else cur.transfer = t;
  }
  // 送信側が取り消した・期限切れになったものは消す（操作済みで表示中のものは残す）
  for (const [id, item] of inbox) {
    if (!item.acted && !incoming.some((t) => t.transfer_id === id)) dropItem(id);
  }
  for (const [id, item] of inbox) {
    if (item.status === "new" || (item.status === "waiting" && item.transfer.state === "uploaded")) loadItem(id, item);
  }
  renderInbox();
  renderHistory(list.filter((t) => t.sender === mine).slice(-10).reverse());
  announce(fresh);
}

/**
 * 新着を知らせる。受信欄は画面上部にあるが、スクロールしていたり入力中だったりすると気づけなかった
 * （iPhone 実機で実際に見落とした）ため、トーストと受信欄へのスクロールで知らせる。
 */
function announce(fresh) {
  const first = !inboxLoaded;
  inboxLoaded = true;
  if (!fresh.length) return;
  if (first) {
    toast(`未処理の受信が ${inbox.size} 件あります`);
  } else if (fresh.length === 1) {
    toast(`${senderName(fresh[0].sender)} から${KIND[fresh[0].kind]}を受信しました`);
  } else {
    toast(`${fresh.length} 件受信しました`);
  }
  // 入力中にスクロールさせると入力位置を見失うので、テキスト欄にフォーカスがあるときは動かさない
  if (document.activeElement !== $("text")) {
    $("inbox-section").scrollIntoView({ behavior: "smooth", block: "start" });
  }
}

/** ホーム画面のアイコン（対応環境）とタブの表題に未処理件数を出す */
function updateBadge() {
  const n = [...inbox.values()].filter((i) => !i.acted).length;
  document.title = n ? `(${n}) つて` : "つて";
  try {
    if (n) navigator.setAppBadge?.(n)?.catch(() => {});
    else navigator.clearAppBadge?.()?.catch(() => {});
  } catch { /* 未対応 */ }
}

async function loadItem(id, item) {
  const t = item.transfer;
  if (!WEB_ACCEPTS.includes(t.kind)) {
    item.status = "error";
    item.error = "この種類は Web では受信できません";
    return;
  }
  if (t.kind === "clipboard_text" && t.text != null) {
    item.text = t.text;
    item.status = "ready";
    return;
  }
  if (t.state !== "uploaded") {
    // 送信側がアップロード中。完了（transfer_state）通知か次の再同期で取得する
    item.status = "waiting";
    return;
  }
  item.status = "loading";
  renderInbox();
  try {
    const { blob } = await api.downloadFile(id);
    if (t.kind === "clipboard_text") item.text = await blob.text();
    else {
      item.blob = blob;
      item.url = URL.createObjectURL(blob);
    }
    item.status = "ready";
  } catch (e) {
    item.status = "error";
    item.error = errText(e);
  }
  renderInbox();
}

function dropItem(id) {
  const item = inbox.get(id);
  if (item?.url) URL.revokeObjectURL(item.url);
  inbox.delete(id);
}

/** Copy / 保存 / 閉じる のいずれかで受信済みにする（ユーザーが扱うまでは Backend に残し、再読込でも消えない） */
async function markReceived(id) {
  const item = inbox.get(id);
  if (!item || item.acted) return;
  item.acted = true;
  try {
    await api.received(id);
  } catch (e) {
    item.acted = false;
    toast(`受信済みにできませんでした: ${errText(e)}`);
  }
  renderInbox();
}

function senderName(id) {
  return endpoints.find((e) => e.endpoint_id === id)?.name ?? id;
}

function renderInbox() {
  const ul = $("inbox");
  $("inbox-section").hidden = inbox.size === 0;
  ul.replaceChildren(...[...inbox.entries()].reverse().map(([id, item]) => renderCard(id, item)));
  updateBadge();
}

function renderCard(id, item) {
  const t = item.transfer;
  const f = t.files[0];
  const when = new Date(t.created_at * 1000).toLocaleString();
  const size = t.text != null ? formatBytes(new TextEncoder().encode(t.text).length) : f ? formatBytes(f.size) : "";
  const dims = f?.media?.width ? ` · ${f.media.width}×${f.media.height}` : "";
  const meta = el("div", { class: "meta" }, `${senderName(t.sender)} から · ${KIND[t.kind]}${dims} · ${size} · ${when}`);
  // コピー・保存などの操作をするまでは「未処理」として強調する
  const li = el("li", { "data-id": id, class: item.acted ? "" : "unhandled" }, meta);
  if (item.status === "waiting" || item.status === "loading" || item.status === "new") {
    li.append(el("p", { class: "muted" }, item.status === "waiting" ? "送信側がアップロード中…" : "取得中…"));
    return li;
  }
  if (item.status === "error") {
    li.append(el("p", { class: "error" }, item.error));
    li.append(el("div", { class: "row end" }, el("button", { type: "button", onclick: () => dismiss(id) }, "閉じる")));
    return li;
  }
  const actions = el("div", { class: "row end" });
  if (item.text != null) {
    li.append(el("pre", { class: "content" }, item.text));
    actions.append(el("button", {
      type: "button", class: "primary",
      onclick: async () => {
        try {
          await copyText(item.text);
          toast("Clipboard にコピーしました");
          markReceived(id);
        } catch (e) {
          toast(`コピーできませんでした: ${errText(e)}`);
        }
      },
    }, "コピー"));
  } else if (item.blob) {
    li.append(el("img", { class: "content", src: item.url, alt: "受信した画像" }));
    if (imageCopySupported()) {
      actions.append(el("button", {
        type: "button", class: "primary",
        onclick: async () => {
          try {
            await copyImage(item.blob);
            toast("Clipboard にコピーしました");
            markReceived(id);
          } catch (e) {
            toast(`コピーできませんでした: ${errText(e)}`);
          }
        },
      }, "コピー"));
    }
    const fileName = `tsute-${new Date(t.created_at * 1000).toISOString().replace(/[:.]/g, "-")}.png`;
    const file = new File([item.blob], fileName, { type: item.blob.type });
    // iOS では共有シート経由で「写真に保存」できる
    if (navigator.canShare?.({ files: [file] })) {
      actions.append(el("button", {
        type: "button",
        onclick: async () => {
          try {
            await navigator.share({ files: [file] });
            markReceived(id);
          } catch (e) {
            if (e?.name !== "AbortError") toast(errText(e));
          }
        },
      }, "共有…"));
    }
    actions.append(el("button", {
      type: "button",
      onclick: () => {
        el("a", { href: item.url, download: fileName }).click();
        markReceived(id);
      },
    }, "保存"));
  }
  actions.append(el("button", { type: "button", onclick: () => dismiss(id) }, "閉じる"));
  li.append(actions);
  return li;
}

async function dismiss(id) {
  await markReceived(id);
  dropItem(id);
  renderInbox();
}

function renderHistory(sent) {
  const ul = $("history");
  ul.replaceChildren(...sent.map((t) => el("li", {},
    el("span", {}, `${KIND[t.kind]} → ${senderName(t.receiver)}`),
    el("span", { class: "state" }, `${STATE[t.state] ?? t.state} · ${new Date(t.created_at * 1000).toLocaleTimeString()}`))));
  if (!sent.length) ul.append(el("li", { class: "muted" }, "まだ送信していません"));
}

// ---------------- Share Target（Android / Chromium のみ） ----------------

/** Service Worker が受け取った共有内容を、送信画面の Preview に入れるだけ（送信は通常どおりユーザーの Send） */
async function consumeShare() {
  const params = new URLSearchParams(location.search);
  if (!params.has("share")) return;
  history.replaceState(null, "", location.pathname);
  const sw = navigator.serviceWorker?.controller;
  if (!sw) return;
  const data = await new Promise((resolve) => {
    const ch = new MessageChannel();
    ch.port1.onmessage = (m) => resolve(m.data);
    sw.postMessage({ type: "take-share", id: params.get("share") }, [ch.port2]);
    setTimeout(() => resolve(null), 5000);
  });
  if (!data) return toast("共有された内容を受け取れませんでした");
  const image = data.files?.find((f) => f.type.startsWith("image/"));
  if (image) {
    try {
      setImage(await imagePayload(image));
    } catch (e) {
      toast(errText(e));
    }
  } else {
    setImage(null);
    $("text").value = [data.title, data.text, data.url].filter(Boolean).join("\n");
    renderTargets();
  }
  toast("共有された内容を読み込みました。送信先を確認して送ってください");
}

// ---------------- Web Push ----------------

function pushSupport() {
  if (!swReg || !("PushManager" in window) || !("Notification" in window)) {
    const ios = /iphone|ipad/i.test(navigator.userAgent) || (navigator.maxTouchPoints > 1 && /macintosh/i.test(navigator.userAgent));
    const standalone = matchMedia("(display-mode: standalone)").matches || navigator.standalone;
    if (ios && !standalone) return "iPhone / iPad では、共有メニューの「ホーム画面に追加」から開くと通知を使えます。";
    return "この Browser は Web Push に対応していません。次に開いたときに未受信を取得します。";
  }
  return null;
}

async function currentSubscription() {
  // 許可が無ければ有効な購読は存在し得ないので、pushManager に触れない
  // （push service の無い環境では getSubscription() が応答しないことがある: Playwright の WebKit で実際に踏んだ）
  if (!swReg?.pushManager || globalThis.Notification?.permission !== "granted") return null;
  return swReg.pushManager.getSubscription().catch(() => null);
}

async function refreshPushSubscription() {
  // 起動のたびに再登録して、サーバー側の購読の期限を延ばす（使われなくなった購読は期限で消える）
  const sub = await currentSubscription();
  if (sub) api.putPushSubscription(sub.endpoint).catch(() => {});
}

async function renderPush() {
  const unsupported = pushSupport();
  const btn = $("push-toggle");
  if (unsupported) {
    $("push-status").textContent = unsupported;
    btn.hidden = true;
    return;
  }
  let cfg = null;
  try { cfg = await api.pushConfig(); } catch { /* 通信失敗時は無効として扱う */ }
  if (!cfg?.vapid_public_key) {
    $("push-status").textContent = "この環境では Web Push が無効です。次に開いたときに未受信を取得します。";
    btn.hidden = true;
    return;
  }
  const sub = await currentSubscription();
  btn.hidden = false;
  if (Notification.permission === "denied") {
    $("push-status").textContent = "通知がブロックされています。Browser の設定から許可してください。";
    btn.hidden = true;
  } else if (sub) {
    $("push-status").textContent = "有効: アプリを閉じている間の受信を通知します（内容は通知に表示しません）。";
    btn.textContent = "通知を無効にする";
    btn.onclick = async () => {
      try {
        await api.deletePushSubscription(sub.endpoint);
        await sub.unsubscribe();
      } catch (e) {
        toast(errText(e));
      }
      renderPush();
    };
  } else {
    $("push-status").textContent = "無効: アプリを閉じている間の受信は、次に開いたときに表示します。";
    btn.textContent = "通知を有効にする";
    // Permission 要求は必ずこのボタン操作から行う（iOS / Firefox は User Activation 必須）
    btn.onclick = async () => {
      try {
        if ((await Notification.requestPermission()) !== "granted") return renderPush();
        const s = await swReg.pushManager.subscribe({
          userVisibleOnly: true,
          applicationServerKey: identity.b64urlDecode(cfg.vapid_public_key),
        });
        await api.putPushSubscription(s.endpoint);
        toast("通知を有効にしました");
      } catch (e) {
        toast(`通知を有効にできませんでした: ${errText(e)}`);
      }
      renderPush();
    };
  }
}

async function closeNotifications() {
  // App を開いたら、受信を知らせる通知は役目を終えている
  for (const n of (await swReg?.getNotifications?.().catch(() => [])) ?? []) n.close();
}

// ---------------- 設定 ----------------

$("open-settings").addEventListener("click", async () => {
  const id = await identity.load();
  $("set-name").value = me?.name ?? id?.name ?? "";
  $("set-endpoint").textContent = id?.endpointId ?? "";
  const persisted = await navigator.storage?.persisted?.().catch(() => false);
  $("set-persist").textContent = persisted
    ? "永続化済み（Browser が自動で消さない）"
    : "永続化されていません（Browser Data の削除などで消えたら再登録が必要）";
  show("settings");
  renderPush();
});
$("close-settings").addEventListener("click", () => show("main"));

$("set-name-save").addEventListener("click", async () => {
  try {
    me = (await api.rename($("set-name").value)).endpoint;
    const id = await identity.load();
    await identity.save({ ...id, name: me.name });
    $("me-name").textContent = me.name;
    toast("保存しました");
  } catch (e) {
    toast(errText(e));
  }
});

$("forget").addEventListener("click", async () => {
  if (!confirm("この Browser の鍵を削除します。再び使うには新しい Enrollment Key で登録し直す必要があります。")) return;
  try {
    const sub = await currentSubscription();
    if (sub) {
      await api.deletePushSubscription(sub.endpoint).catch(() => {});
      await sub.unsubscribe().catch(() => {});
    }
  } finally {
    realtime?.stop();
    await identity.clear();
    location.reload();
  }
});

boot();
