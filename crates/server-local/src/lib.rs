//! ローカル開発・テスト用サーバー。
//!
//! 本番の CloudFront + API Gateway + Lambda + DynamoDB + S3 と同じ URL 構造
//! (`/api/*`, `/ws`, presigned URL による Object 転送) を 1 プロセスで再現し、
//! AWS なしでもクライアントの結合テストを実通信（HTTP/WebSocket/ファイル I/O）で行えるようにする。
//! ロジックは server-core を共有するため、ここに独自のビジネスロジックは置かない。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use tsute_server_core::memory::{ChannelNotifier, MemoryStore};
use tsute_server_core::traits::{BlobStore, PresignedPut, Result as CoreResult};
use tsute_server_core::{Config, Core, Request};
use tsute_webpush::VapidPusher;

/// ファイルシステム上の Object Storage。URL は HMAC で署名し、S3 presigned URL と同様に
/// 「期限付き・対象キー固定・PUT は内容の checksum 固定」という性質を再現する。
pub struct FsBlobStore {
    pub dir: PathBuf,
    pub base_url: String,
    secret: [u8; 32],
}

impl FsBlobStore {
    pub fn new(dir: PathBuf, base_url: String) -> Self {
        let mut secret = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut secret);
        Self { dir, base_url, secret }
    }

    fn sign(&self, method: &str, key: &str, exp: i64, extra: &str) -> String {
        let mut m = Hmac::<Sha256>::new_from_slice(&self.secret).expect("hmac");
        m.update(format!("{method}\n{key}\n{exp}\n{extra}").as_bytes());
        URL_SAFE_NO_PAD.encode(m.finalize().into_bytes())
    }

    fn verify(&self, method: &str, key: &str, exp: i64, extra: &str, sig: &str) -> bool {
        exp > tsute_server_core::now() && self.sign(method, key, exp, extra) == sig
    }

    fn path(&self, key: &str) -> PathBuf {
        // キーは server-core が生成したものだけだが、念のため親ディレクトリ参照を拒否
        assert!(!key.split('/').any(|s| s == ".."));
        self.dir.join(key)
    }
}

impl BlobStore for FsBlobStore {
    async fn presign_put(&self, key: &str, size: u64, sha: &str, ttl: u64) -> CoreResult<PresignedPut> {
        let exp = tsute_server_core::now() + ttl as i64;
        let sig = self.sign("PUT", key, exp, &format!("{size}\n{sha}"));
        let url = format!("{}/blob/{key}?exp={exp}&size={size}&sig={sig}", self.base_url);
        // S3 と同じヘッダ名にして、クライアント側の実装を環境で分岐させない
        Ok(PresignedPut {
            url,
            headers: vec![("x-amz-checksum-sha256".into(), sha.into())],
        })
    }
    async fn presign_get(&self, key: &str, ttl: u64) -> CoreResult<String> {
        let exp = tsute_server_core::now() + ttl as i64;
        let sig = self.sign("GET", key, exp, "");
        Ok(format!("{}/blob/{key}?exp={exp}&sig={sig}", self.base_url))
    }
    async fn head(&self, key: &str) -> CoreResult<Option<(u64, Option<String>)>> {
        let p = self.path(key);
        match tokio::fs::metadata(&p).await {
            Ok(m) => {
                let sha = tokio::fs::read_to_string(p.with_extension("sha256")).await.ok();
                Ok(Some((m.len(), sha)))
            }
            Err(_) => Ok(None),
        }
    }
    async fn delete_prefix(&self, prefix: &str) -> CoreResult<()> {
        let p = self.path(prefix.trim_end_matches('/'));
        let _ = tokio::fs::remove_dir_all(p).await;
        Ok(())
    }
}

pub type LocalCore = Core<MemoryStore, FsBlobStore, ChannelNotifier, Option<VapidPusher>>;

/// 本番の `/app/*`（CloudFront の ResponseHeadersPolicy）と同じ方針の CSP。
/// ローカルは presigned URL（/blob）も同一オリジンなので connect-src は 'self' だけで足りる
pub const APP_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; \
img-src 'self' blob: data:; manifest-src 'self'; worker-src 'self'; object-src 'none'; base-uri 'none'; \
form-action 'self'; frame-ancestors 'none'";

#[derive(Default)]
pub struct Options {
    /// Web / PWA Client（リポジトリの `web/`）。指定すると `/app/` で配信する
    pub web_dir: Option<PathBuf>,
    /// VAPID 秘密鍵（base64url）と subject。指定すると実際の push service へ Web Push を送る
    pub vapid: Option<(String, String)>,
}

#[derive(Clone)]
pub struct AppState {
    pub core: Arc<LocalCore>,
    pub admin_token: Arc<String>,
    /// テストで通信断を再現するためのフラグ（true の間 blob 転送を 503 にする）
    pub fail_blobs: Arc<std::sync::atomic::AtomicBool>,
    /// blob 転送ごとの人工遅延（ms）。ローカルでは転送が一瞬で終わり「転送中」の挙動（overlap・中断→再開）を
    /// E2E で捉えられないため、回線の遅さを模擬する
    pub blob_delay_ms: Arc<std::sync::atomic::AtomicU64>,
    pub web_dir: Option<Arc<PathBuf>>,
}

pub struct Server {
    pub addr: SocketAddr,
    pub base_url: String,
    pub state: AppState,
    pub handle: tokio::task::JoinHandle<()>,
}

impl Server {
    pub async fn issue_enrollment_key(&self) -> String {
        self.state.core.issue_enrollment_key().await.expect("issue").0
    }
}

pub async fn start(bind: SocketAddr, data_dir: PathBuf, admin_token: String, cfg: Config) -> std::io::Result<Server> {
    start_with(bind, data_dir, admin_token, cfg, Options::default()).await
}

pub async fn start_with(
    bind: SocketAddr,
    data_dir: PathBuf,
    admin_token: String,
    cfg: Config,
    opts: Options,
) -> std::io::Result<Server> {
    let pusher = match &opts.vapid {
        Some((key, subject)) => Some(VapidPusher::new(key, subject).map_err(std::io::Error::other)?),
        None => None,
    };
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let addr = listener.local_addr()?;
    let base_url = format!("http://{addr}");
    tokio::fs::create_dir_all(&data_dir).await?;
    let core = Core::new(
        MemoryStore::default(),
        FsBlobStore::new(data_dir, base_url.clone()),
        ChannelNotifier::default(),
        cfg,
    )
    .with_pusher(pusher);
    let state = AppState {
        core: Arc::new(core),
        admin_token: Arc::new(admin_token),
        fail_blobs: Arc::new(Default::default()),
        blob_delay_ms: Arc::new(Default::default()),
        web_dir: opts.web_dir.map(Arc::new),
    };
    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/admin/enrollment-keys", post(admin_issue))
        // チャンク（最大 64MiB）を受けるため axum の既定 2MB 上限を引き上げる
        .route(
            "/blob/{*key}",
            get(blob_get).put(blob_put).layer(axum::extract::DefaultBodyLimit::max(
                tsute_proto::MAX_CHUNK_SIZE as usize + 1024,
            )),
        )
        .route("/api/{*rest}", any(api))
        .route("/app", get(|| async { axum::response::Redirect::permanent("/app/") }))
        .route("/app/", get(app_static))
        .route("/app/{*path}", get(app_static).post(app_static))
        .with_state(state.clone());
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Ok(Server {
        addr,
        base_url,
        state,
        handle,
    })
}

fn lower_headers(h: &HeaderMap) -> HashMap<String, String> {
    h.iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|v| (k.as_str().to_ascii_lowercase(), v.to_string()))
        })
        .collect()
}

async fn api(State(st): State<AppState>, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let r = st
        .core
        .handle_http(Request {
            method: method.to_string(),
            path: uri.path().to_string(),
            headers: lower_headers(&headers),
            body: body.to_vec(),
        })
        .await;
    (
        StatusCode::from_u16(r.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        [("content-type", "application/json")],
        r.body,
    )
        .into_response()
}

/// `/app/*` の静的配信。本番の S3 + CloudFront（`/app/` → index.html, CSP 等のヘッダ）を模す
async fn app_static(State(st): State<AppState>, uri: Uri) -> Response {
    let Some(dir) = st.web_dir.as_deref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let rel = uri.path().trim_start_matches("/app/").trim_start_matches("/app");
    let rel = if rel.is_empty() { "index.html" } else { rel };
    if rel.split('/').any(|s| s == ".." || s.is_empty() || s.starts_with('.')) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(body) = tokio::fs::read(dir.join(rel)).await else {
        // Share Target の POST 等は Service Worker が処理する。SW が無い（未インストール）ときの受け皿
        return StatusCode::NOT_FOUND.into_response();
    };
    let ext = rel.rsplit('.').next().unwrap_or("");
    let ctype = match ext {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "webmanifest" => "application/manifest+json",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "json" => "application/json",
        _ => "application/octet-stream",
    };
    (
        [
            ("content-type", ctype),
            ("content-security-policy", APP_CSP),
            ("x-content-type-options", "nosniff"),
            ("referrer-policy", "no-referrer"),
            // 開発中の変更を即時反映させる（本番は deploy 時にファイル種別ごとに Cache-Control を付ける）
            ("cache-control", "no-cache"),
        ],
        body,
    )
        .into_response()
}

async fn admin_issue(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let ok = headers.get("x-admin-token").and_then(|v| v.to_str().ok()) == Some(st.admin_token.as_str());
    if !ok {
        return StatusCode::FORBIDDEN.into_response();
    }
    match st.core.issue_enrollment_key().await {
        Ok((key, exp)) => axum::Json(serde_json::json!({"enrollment_key": key, "expires_at": exp})).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn blob_put(
    State(st): State<AppState>,
    Path(key): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if st.fail_blobs.load(std::sync::atomic::Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let delay = st.blob_delay_ms.load(std::sync::atomic::Ordering::SeqCst);
    if delay > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
    let blob = &st.core.blob;
    let exp: i64 = q.get("exp").and_then(|v| v.parse().ok()).unwrap_or(0);
    let size: u64 = q.get("size").and_then(|v| v.parse().ok()).unwrap_or(u64::MAX);
    let sha = headers
        .get("x-amz-checksum-sha256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let sig = q.get("sig").map(String::as_str).unwrap_or("");
    if !blob.verify("PUT", &key, exp, &format!("{size}\n{sha}"), sig) {
        return StatusCode::FORBIDDEN.into_response();
    }
    // S3 は Content-Length を署名対象にするため、ヘッダがない/不一致のリクエストを拒否する（0 バイト時に実際に踏んだ）
    let cl = headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if cl != Some(size) {
        return (StatusCode::FORBIDDEN, "SignatureDoesNotMatch (content-length)").into_response();
    }
    // S3 が x-amz-checksum-sha256 不一致を 400 BadDigest で拒否する挙動を再現
    if body.len() as u64 != size || STANDARD.encode(Sha256::digest(&body)) != sha {
        return (StatusCode::BAD_REQUEST, "BadDigest").into_response();
    }
    let p = blob.path(&key);
    if let Some(d) = p.parent() {
        let _ = tokio::fs::create_dir_all(d).await;
    }
    if tokio::fs::write(&p, &body).await.is_err() || tokio::fs::write(p.with_extension("sha256"), sha).await.is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    StatusCode::OK.into_response()
}

async fn blob_get(
    State(st): State<AppState>,
    Path(key): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if st.fail_blobs.load(std::sync::atomic::Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let delay = st.blob_delay_ms.load(std::sync::atomic::Ordering::SeqCst);
    if delay > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
    let blob = &st.core.blob;
    let exp: i64 = q.get("exp").and_then(|v| v.parse().ok()).unwrap_or(0);
    let sig = q.get("sig").map(String::as_str).unwrap_or("");
    if !blob.verify("GET", &key, exp, "", sig) {
        return StatusCode::FORBIDDEN.into_response();
    }
    match tokio::fs::read(blob.path(&key)).await {
        Ok(b) => Body::from(b).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn ws_handler(State(st): State<AppState>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    let conn_id = uuid::Uuid::new_v4().simple().to_string();
    let h = lower_headers(&headers);
    // 接続レコード作成より先に送信キューを用意し、直後の通知を取りこぼさない
    let rx = st.core.notifier.register(&conn_id);
    // API Gateway の $connect と同様、ハンドシェイク完了前に認証して拒否できるようにする
    match st.core.ws_connect(&conn_id, &h).await {
        Ok(acc) => {
            // Browser（ticket 認証）には要求されたサブプロトコルを echo する（API Gateway の $connect 応答と同じ）
            let ws = match acc.subprotocol {
                Some(p) => ws.protocols([p]),
                None => ws,
            };
            let ep = acc.endpoint_id;
            ws.on_upgrade(move |socket| ws_session(st, socket, conn_id, ep, rx))
        }
        Err(e) => {
            st.core.notifier.unregister(&conn_id);
            StatusCode::from_u16(e.status)
                .unwrap_or(StatusCode::UNAUTHORIZED)
                .into_response()
        }
    }
}

async fn ws_session(
    st: AppState,
    socket: WebSocket,
    conn_id: String,
    ep: String,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<String>,
) {
    let (mut tx_ws, mut rx_ws) = socket.split();
    let hello = st.core.hello_event(&conn_id, &ep).await;
    let _ = tx_ws
        .send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
        .await;
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(m) => if tx_ws.send(Message::Text(m.into())).await.is_err() { break },
                None => break,
            },
            inc = rx_ws.next() => match inc {
                Some(Ok(Message::Text(t))) => {
                    if let Some(ev) = st.core.ws_message(&conn_id, &t).await {
                        let _ = tx_ws.send(Message::Text(serde_json::to_string(&ev).unwrap().into())).await;
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            }
        }
    }
    st.core.notifier.unregister(&conn_id);
    let _ = st.core.ws_disconnect(&conn_id).await;
}

/// テストから特定接続を強制切断する（API Gateway の 2 時間切断などの再現用）
pub fn kick_all(st: &AppState) {
    st.core.notifier.senders.lock().expect("lock").clear();
}
