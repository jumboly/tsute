// つて UI。ビルド工程なしの素の JS（依存を増やさず、WebView に読み込むコードを最小にするため）。
// 受信内容や名前は必ず textContent で表示し、innerHTML には入れない（XSS 防止）。
"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
let state = null;
let endpoints = [];
let clipPreview = null;
let clipIndex = 0;
let pendingFiles = [];
let focusId = null;

function el(tag, attrs = {}, ...children) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") e.className = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else if (k === "dataset") Object.assign(e.dataset, v);
    else if (v !== undefined && v !== null && v !== false) e.setAttribute(k, v === true ? "" : v);
  }
  for (const c of children.flat()) {
    if (c === null || c === undefined || c === false) continue;
    e.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return e;
}

function humanSize(n) {
  const u = ["B", "KB", "MB", "GB", "TB"];
  let v = n, i = 0;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return i === 0 ? `${n} B` : `${v.toFixed(1)} ${u[i]}`;
}

function fmtDuration(ms) {
  if (ms == null) return null;
  const s = Math.round(ms / 100) / 10;
  return s >= 60 ? `${Math.floor(s / 60)}分${Math.round(s % 60)}秒` : `${s}秒`;
}

function toast(msg, ms = 2600) {
  const t = $("toast");
  t.textContent = msg;
  t.hidden = false;
  clearTimeout(toast._t);
  toast._t = setTimeout(() => (t.hidden = true), ms);
}

function show(view) {
  for (const v of document.querySelectorAll(".view")) v.hidden = v.id !== `view-${view}`;
  document.body.dataset.view = view;
  fitWindow();
}

// ---------------- 履歴の折りたたみとウィンドウの高さ ----------------
// 開閉状態と広げていたときの高さはこの WebView だけの表示設定なので localStorage に置く（読めなくても動く）

const store = {
  get(k) { try { return localStorage.getItem(k); } catch { return null; } },
  set(k, v) { try { localStorage.setItem(k, v); } catch { /* 保存できなくても表示は続ける */ } },
};
let historyCollapsed = store.get("tsute.historyCollapsed") === "1";
let windowFixed = false;
let newWhileCollapsed = 0;

function setHistoryCollapsed(collapsed) {
  if (collapsed && !historyCollapsed) store.set("tsute.expandedHeight", String(window.innerHeight));
  historyCollapsed = collapsed;
  store.set("tsute.historyCollapsed", collapsed ? "1" : "0");
  if (!collapsed) newWhileCollapsed = 0;
  renderHistoryToggle();
  fitWindow();
}

function renderHistoryToggle() {
  $("history-toggle").setAttribute("aria-expanded", String(!historyCollapsed));
  $("history").hidden = historyCollapsed;
  const badge = $("history-new");
  badge.hidden = !newWhileCollapsed;
  badge.textContent = `新着 ${newWhileCollapsed}`;
}

// 折りたたみ中のメイン画面ではウィンドウを中身の高さに合わせて固定し、それ以外では広げていたときの高さに戻す
function fitWindow() {
  const compact = historyCollapsed && currentView() === "main";
  if (compact) {
    const view = $("view-main");
    const bottom = $("history-toggle").getBoundingClientRect().bottom;
    const height = Math.ceil(bottom + parseFloat(getComputedStyle(view).paddingBottom));
    windowFixed = true;
    invoke("set_window_height", { height, fixed: true }).catch(() => {});
  } else if (windowFixed) {
    windowFixed = false;
    const height = Number(store.get("tsute.expandedHeight")) || 580;
    invoke("set_window_height", { height, fixed: false }).catch(() => {});
  }
}

function currentView() {
  return document.body.dataset.view;
}

async function call(cmd, args) {
  try {
    return await invoke(cmd, args);
  } catch (e) {
    toast(`エラー: ${e}`, 5000);
    throw e;
  }
}

// ---------------- 状態 ----------------

async function refreshState() {
  state = await invoke("get_state");
  $("me-name").textContent = state.endpoint_name || "つて（未登録）";
  const badge = $("profile-badge");
  badge.hidden = state.profile === "default";
  badge.textContent = state.profile;
  $("insecure-banner").hidden = !state.insecure_credentials;
  setConn(state.connection);
  if (!state.enrolled) {
    show("enroll");
    if (!$("enroll-url").value && state.default_base_url) $("enroll-url").value = state.default_base_url;
    if (!$("enroll-name").value) $("enroll-name").value = `${state.platform === "windows" ? "Windows" : "Mac"} / ${state.profile}`;
  }
}

function setConn(c) {
  const d = $("conn-dot");
  d.className = `dot ${c}`;
  d.title = c;
  document.body.dataset.connection = c;
}

async function refreshEndpoints() {
  if (!state?.enrolled) return;
  try { endpoints = await invoke("list_endpoints"); } catch { return; }
  const sel = $("target");
  // 画面を開き直した直後は、前回送った相手を初期値にする（先頭の Endpoint を既定にしない）
  const prev = sel.value || state.last_receiver;
  sel.replaceChildren();
  const others = endpoints.filter((e) => e.endpoint_id !== state.endpoint_id);
  for (const e of others) {
    sel.append(el("option", { value: e.endpoint_id }, `${e.online ? "● " : "○ "}${e.name}${e.client_kind === "web" ? "（Web）" : ""}`));
  }
  if (others.some((e) => e.endpoint_id === prev)) sel.value = prev;
  if (!others.length) sel.append(el("option", { value: "", disabled: true }, "送信先がありません（他の Endpoint を登録してください）"));
  $("send-clipboard").disabled = !others.length;
  const list = $("endpoints");
  list.replaceChildren(...endpoints.map((e) =>
    el("li", {}, `${e.online ? "● " : "○ "}${e.name}${e.endpoint_id === state.endpoint_id ? "（このEndpoint）" : ""}`,
      el("div", { class: "mono" }, `${e.endpoint_id} · ${e.platform}`))));
}

function targetName(id) {
  return endpoints.find((e) => e.endpoint_id === id)?.name ?? id;
}

// 確認画面の候補 → Transfer の種類。送信先の accepts（受信できる Payload, ADR-0015）と照らす
const CANDIDATE_TRANSFER_KIND = { text: "clipboard_text", image: "clipboard_image", video: "clipboard_video", files: "files" };
const TRANSFER_KIND_LABEL = { clipboard_text: "テキスト", clipboard_image: "画像", clipboard_video: "動画", files: "ファイル" };

/** 送信先が受け取れないなら理由の文字列、受け取れるなら null。サーバーも同じ判定で拒否する（多層防御） */
function rejectReason(targetId, kind) {
  const t = endpoints.find((e) => e.endpoint_id === targetId);
  // accepts を返さない旧サーバーでは判定できないので、サーバー側の判定に任せる
  if (!t?.accepts || t.accepts.includes(kind)) return null;
  const web = t.client_kind === "web" ? "（Web / PWA の Endpoint はテキストと画像のみ受信できます）" : "";
  return `${t.name} は${TRANSFER_KIND_LABEL[kind]}を受け取れません${web}`;
}

// ---------------- 履歴 ----------------

const KIND = { clipboard_text: "テキスト", clipboard_image: "画像", clipboard_video: "動画", files: "ファイル" };
const STATUS = { active: "転送中", uploaded: "受信待ち", done: "完了", cancelled: "取消", failed: "失敗" };

async function refreshHistory() {
  if (!state?.enrolled) return;
  let items;
  try { items = await invoke("history"); } catch { return; }
  const ul = $("history");
  ul.replaceChildren(...items.map(renderItem));
  if (!items.length) ul.append(el("li", { class: "muted" }, "まだ何も送受信していません"));
  if (focusId) {
    const li = ul.querySelector(`[data-id="${CSS.escape(focusId)}"]`);
    if (li) { li.classList.add("focus"); li.scrollIntoView({ block: "center" }); }
  }
}

function renderItem(it) {
  const t = it.transfer;
  const incoming = it.direction === "incoming";
  const total = t.files.reduce((a, f) => a + f.size, 0) || (t.text ? new TextEncoder().encode(t.text).length : 0);
  const li = el("li", { dataset: { id: t.transfer_id, status: it.status, kind: t.kind, direction: it.direction } });
  li.append(el("div", { class: "head" },
    el("span", { class: "dir" }, `${incoming ? "⬇︎ " + it.peer_name + " から" : "⬆︎ " + it.peer_name + " へ"} · ${KIND[t.kind]}`),
    el("span", { class: "meta" }, STATUS[it.status] ?? it.status)));
  const details = [];
  if (t.kind === "files") details.push(`${t.files.length} ファイル`);
  if (t.files.length === 1 && t.kind !== "clipboard_text") details.push(t.files[0].name);
  const m = t.files[0]?.media;
  if (m?.width) details.push(`${m.width}×${m.height}`);
  if (m?.duration_ms) details.push(fmtDuration(m.duration_ms));
  details.push(humanSize(total));
  details.push(new Date(t.created_at * 1000).toLocaleString());
  li.append(el("div", { class: "meta" }, details.join(" · ")));
  if (it.text_preview) li.append(el("div", { class: "text", "data-testid": "text-preview" }, it.text_preview));
  if (it.status === "active" && it.progress) {
    const [done, tot] = it.progress;
    li.append(el("progress", { max: String(Math.max(tot, 1)), value: String(done), "data-testid": "progress" }));
    li.append(el("div", { class: "meta" }, `${humanSize(done)} / ${humanSize(tot)}`));
  }
  if (it.error && it.status !== "done") li.append(el("div", { class: "err" }, it.error));
  const btns = el("div", { class: "btns" });
  if (incoming && it.status === "done") {
    btns.append(el("button", { class: "small primary", "data-testid": "apply", onclick: () => applyClipboard(t.transfer_id) },
      t.kind === "files" || t.kind === "clipboard_video" ? "Clipboard にコピー（ファイル）" : "Clipboard にコピー"));
    if (t.kind !== "files") btns.append(el("button", { class: "small", "data-testid": "save", onclick: () => saveAs(t.transfer_id) }, "保存…"));
    if (t.files.length && it.files_exist) btns.append(el("button", { class: "small", onclick: () => call("reveal", { transferId: t.transfer_id }) }, state.platform === "windows" ? "Explorer で表示" : "Finder で表示"));
  }
  if ((it.status === "active" || it.status === "uploaded")) {
    btns.append(el("button", { class: "small danger", "data-testid": "cancel", onclick: () => cancelTransfer(t.transfer_id) }, "取消"));
  }
  if (btns.childElementCount) li.append(btns);
  return li;
}

async function applyClipboard(id) {
  await call("apply_to_clipboard", { transferId: id });
  toast("Clipboard にコピーしました");
}

async function saveAs(id) {
  const p = await call("save_as", { transferId: id });
  if (p) toast(`保存しました: ${p}`);
}

async function cancelTransfer(id) {
  await call("cancel_transfer", { transferId: id });
  refreshHistory();
}

// ---------------- Clipboard 送信 ----------------

async function startSendClipboard() {
  if (!$("target").value) { toast("送信先を選んでください"); return; }
  // ここで初めて Clipboard を読む（アプリを開いただけでは読まない）
  clipPreview = await call("read_clipboard");
  clipIndex = 0;
  if (!clipPreview.candidates.length) {
    toast("Clipboard に送信できる内容（テキスト/画像/動画/ファイル）がありません", 4000);
    clipPreview = null;
    return;
  }
  $("clip-target").textContent = targetName($("target").value);
  renderClip();
  show("clip");
}

const CANDIDATE_LABEL = { text: "テキスト", image: "画像", video: "動画", files: "ファイル" };

function renderClip() {
  const tabs = $("clip-tabs");
  tabs.replaceChildren(...clipPreview.candidates.map((c, i) =>
    el("button", { class: i === clipIndex ? "active small" : "small", "data-testid": `clip-tab-${c.kind}`, onclick: () => { clipIndex = i; renderClip(); } },
      CANDIDATE_LABEL[c.kind])));
  tabs.hidden = clipPreview.candidates.length < 2;
  const c = clipPreview.candidates[clipIndex];
  const p = $("clip-preview");
  p.dataset.kind = c.kind;
  const rows = [];
  if (c.kind === "text") {
    p.replaceChildren(el("pre", { "data-testid": "clip-text" }, c.text.length > 20000 ? c.text.slice(0, 20000) + "\n…（以下省略）" : c.text));
    rows.push(["文字数", `${c.char_count.toLocaleString()} 文字`], ["サイズ", humanSize(c.byte_size)]);
  } else if (c.kind === "image" || c.kind === "video") {
    p.replaceChildren();
    if (c.preview_data_url) p.append(el("img", { src: c.preview_data_url, alt: "preview", "data-testid": "clip-image" }));
    rows.push(["形式", c.mime], ["サイズ", humanSize(c.size)]);
    if (c.width) rows.push(["解像度", `${c.width} × ${c.height}`]);
    if (c.kind === "video" && c.duration_ms != null) rows.push(["長さ", fmtDuration(c.duration_ms)]);
    rows.push(["ファイル名", c.name]);
    if (!c.temporary) rows.push(["パス", c.path]);
    rows.push(["取得元", c.source]);
  } else if (c.kind === "files") {
    p.replaceChildren(el("ul", { class: "files" }, c.paths.map((x) => el("li", {}, el("span", { class: "mono path" }, x)))));
    rows.push(["ファイル数", String(c.paths.length)], ["合計", humanSize(c.byte_size)]);
  }
  p.append(el("dl", { "data-testid": "clip-meta" }, rows.flatMap(([k, v]) => [el("dt", {}, k), el("dd", {}, v)])));
  const why = rejectReason($("target").value, CANDIDATE_TRANSFER_KIND[c.kind]);
  $("clip-reject").hidden = !why;
  $("clip-reject").textContent = why ?? "";
  $("clip-send").disabled = !!why;
}

async function sendClip() {
  const target = $("target").value;
  $("clip-send").disabled = true;
  try {
    await call("send_clipboard", { index: clipPreview.candidates[clipIndex].index, receiver: target });
    toast("送信を開始しました");
    clipPreview = null;
    show("main");
    refreshHistory();
  } finally {
    $("clip-send").disabled = false;
  }
}

async function cancelClip() {
  clipPreview = null;
  await invoke("cancel_clipboard");
  show("main");
}

// ---------------- ファイル送信 ----------------

// Drop は「送信候補の選択」だけ。確認画面の送信ボタンで初めて転送する
async function handleDrop(paths) {
  if (!state?.enrolled || currentView() === "enroll") return;
  if (!$("target").value) { toast("送信先を選んでください"); return; }
  const r = await call("prepare_files", { paths });
  pendingFiles = r.files.map((f) => f.path);
  $("files-count").textContent = String(r.files.length);
  $("files-total").textContent = humanSize(r.total_size);
  $("files-list").replaceChildren(...r.files.map((f) =>
    el("li", {}, el("strong", {}, f.name), el("span", {}, humanSize(f.size)), el("span", { class: "mono path" }, f.path))));
  $("files-rejected").replaceChildren(...r.rejected.map(([p, why]) => el("li", {}, el("span", { class: "mono path" }, p), el("span", {}, why))));
  $("files-target").textContent = targetName($("target").value);
  const why = rejectReason($("target").value, "files");
  $("files-reject").hidden = !why;
  $("files-reject").textContent = why ?? "";
  $("files-send").disabled = r.files.length === 0 || !!why;
  show("files");
}

async function sendFiles() {
  $("files-send").disabled = true;
  try {
    await call("send_files", { paths: pendingFiles, receiver: $("target").value });
    toast("送信を開始しました");
    pendingFiles = [];
    show("main");
    refreshHistory();
  } finally {
    $("files-send").disabled = false;
  }
}

// ---------------- 設定 ----------------

async function openSettings() {
  await refreshState();
  if (!state.enrolled) return;
  $("set-profile").textContent = state.profile;
  $("set-endpoint").textContent = state.endpoint_id;
  $("set-url").textContent = state.base_url;
  $("set-cred").textContent = state.credential_store;
  $("set-dl").textContent = state.download_dir;
  $("set-version").textContent = state.version;
  $("set-name").value = state.endpoint_name;
  const login = $("set-login");
  $("set-login-label").textContent = state.platform === "windows" ? "Windows の起動時に開始" : "ログイン時に起動";
  $("forget").textContent = state.platform === "windows" ? "この PC の登録情報を削除" : "このMacの登録情報を削除";
  login.checked = state.login_item === "enabled";
  login.disabled = state.login_item === "unavailable";
  $("set-login-status").textContent = {
    enabled: "有効", not_registered: "無効", requires_approval: "システム設定で承認が必要です",
    unavailable: ".app として起動した場合のみ設定できます", not_found: "無効",
    disabled_by_user: "Windows の設定（スタートアップ アプリ）で無効になっています。オンにすると有効に戻します",
  }[state.login_item] ?? state.login_item;
  refreshEndpoints();
  show("settings");
}

// ---------------- 起動・イベント ----------------

function route(v) {
  if (!v) return;
  if (v === "send_clipboard") { show("main"); startSendClipboard(); }
  else if (v === "settings") openSettings();
  else if (v.startsWith("focus:")) {
    // 通知から特定の受信を開いたときは、折りたたんでいても広げて見せる
    focusId = v.slice(6); show("main"); setHistoryCollapsed(false); refreshHistory();
  }
}

// E2E オートメーション用（Rust 側の automation.rs から呼ばれる。--automation 起動時のみ使われる）
window.__tsuteAuto = async (id, fn) => {
  try {
    const v = await fn();
    await invoke("automation_result", { id, ok: true, value: v === undefined ? null : v });
  } catch (e) {
    await invoke("automation_result", { id, ok: false, value: String(e?.stack || e) });
  }
};
window.__tsute = { handleDrop, refreshHistory, refreshEndpoints, show, state: () => state };

async function init() {
  $("enroll-submit").onclick = async () => {
    $("enroll-error").textContent = "";
    $("enroll-submit").disabled = true;
    try {
      await invoke("enroll", { baseUrl: $("enroll-url").value, enrollmentKey: $("enroll-key").value, name: $("enroll-name").value });
      $("enroll-key").value = "";
      await refreshState();
      show("main");
      await refreshEndpoints();
      await refreshHistory();
    } catch (e) {
      $("enroll-error").textContent = String(e);
    } finally {
      $("enroll-submit").disabled = false;
    }
  };
  $("send-clipboard").onclick = startSendClipboard;
  $("clip-send").onclick = sendClip;
  $("clip-cancel").onclick = cancelClip;
  $("files-send").onclick = sendFiles;
  $("files-cancel").onclick = () => { pendingFiles = []; show("main"); };
  $("nav-settings").onclick = openSettings;
  $("settings-back").onclick = () => { show(state.enrolled ? "main" : "enroll"); };
  $("set-login").onchange = async (e) => {
    try { await call("set_login_item", { enabled: e.target.checked }); } finally { openSettings(); }
  };
  $("set-name-save").onclick = async () => { await call("rename_endpoint", { name: $("set-name").value }); await refreshState(); toast("保存しました"); };
  $("set-dl-change").onclick = async () => { const d = await call("choose_download_dir"); if (d) $("set-dl").textContent = d; };
  $("forget").onclick = async () => {
    if (!confirm("このプロファイルの登録情報（秘密鍵を含む）を削除します。サーバー側の失効は管理者が行ってください。よろしいですか？")) return;
    await call("forget_enrollment");
    await refreshState();
  };

  const dz = $("dropzone");
  window.__TAURI__.webview.getCurrentWebview().onDragDropEvent((ev) => {
    const p = ev.payload;
    if (p.type === "enter" || p.type === "over") dz.classList.add("over");
    else if (p.type === "leave") dz.classList.remove("over");
    else if (p.type === "drop") { dz.classList.remove("over"); handleDrop(p.paths); }
  });

  await listen("client-event", (e) => {
    const ev = e.payload;
    if (ev.type === "connection") setConn(ev.state);
    else if (ev.type === "progress") updateProgress(ev);
    else if (["transfer_updated", "incoming_ready", "delivered", "transfer_failed"].includes(ev.type)) scheduleHistory();
    if (ev.type === "incoming_ready" && historyCollapsed) { newWhileCollapsed++; renderHistoryToggle(); }
  });
  $("history-toggle").onclick = () => setHistoryCollapsed(!historyCollapsed);
  renderHistoryToggle();
  await listen("endpoints-changed", () => refreshEndpoints());
  await listen("navigate", (e) => route(e.payload));

  await refreshState();
  if (state.enrolled) {
    show("main");
    await refreshEndpoints();
    await refreshHistory();
  }
  route(decodeURIComponent(location.hash.slice(1)));
  document.body.dataset.ready = "1";
}

let historyTimer = null;
function scheduleHistory() {
  clearTimeout(historyTimer);
  historyTimer = setTimeout(refreshHistory, 150);
}

function updateProgress(ev) {
  const li = document.querySelector(`#history [data-id="${CSS.escape(ev.transfer_id)}"]`);
  if (!li) { scheduleHistory(); return; }
  let bar = li.querySelector("progress");
  if (!bar) { scheduleHistory(); return; }
  bar.max = Math.max(ev.total_bytes, 1);
  bar.value = ev.done_bytes;
  const m = bar.nextElementSibling;
  if (m) m.textContent = `${humanSize(ev.done_bytes)} / ${humanSize(ev.total_bytes)}`;
}

init();
