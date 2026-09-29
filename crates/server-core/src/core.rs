//! HTTP API / WebSocket イベントのハンドラ。実行環境非依存。

use std::collections::HashMap;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use rand::RngCore;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use tsute_proto::*;

use crate::traits::*;

pub const ENROLLMENT_KEY_PREFIX: &str = "tsute-ek-";
const TOKEN_PREFIX: &str = "tsute-at-";
const WS_TICKET_PREFIX: &str = "tsute-wt-";
/// Namespace 導入前に登録された Endpoint が属する Namespace
pub const DEFAULT_NAMESPACE: &str = "default";
/// 1 Endpoint が持てる Push Subscription の上限（同じ Browser の再購読で増え続けないように）
const MAX_PUSH_SUBSCRIPTIONS_PER_ENDPOINT: usize = 5;

#[derive(Debug, Clone)]
pub struct Config {
    pub enrollment_key_ttl_secs: i64,
    pub challenge_ttl_secs: i64,
    pub token_ttl_secs: i64,
    pub transfer_ttl_secs: i64,
    pub presign_ttl_secs: u64,
    /// API Gateway の最大接続時間(2h)より長くし、切断イベントを取りこぼした接続を最終的に消す
    pub connection_ttl_secs: i64,
    /// 発行直後に接続する用途なので短く（漏れても使える時間を最小にする）
    pub ws_ticket_ttl_secs: i64,
    /// Web Client は起動のたびに購読を再登録して延長する。使われなくなった購読はこの期間で消える
    pub push_subscription_ttl_secs: i64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enrollment_key_ttl_secs: 10 * 60,
            challenge_ttl_secs: 120,
            token_ttl_secs: 60 * 60,
            transfer_ttl_secs: 7 * 24 * 3600,
            presign_ttl_secs: 30 * 60,
            connection_ttl_secs: 3 * 3600,
            ws_ticket_ttl_secs: 30,
            push_subscription_ttl_secs: 60 * 24 * 3600,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// 小文字化したヘッダ名
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    fn json<T: Serialize>(status: u16, v: &T) -> Self {
        Self {
            status,
            body: serde_json::to_vec(v).expect("serialize"),
        }
    }
}

#[derive(Debug)]
pub struct ApiErr {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl ApiErr {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
    fn bad(m: impl Into<String>) -> Self {
        Self::new(400, "bad_request", m)
    }
    fn unauthorized() -> Self {
        Self::new(401, "unauthorized", "authentication required")
    }
    fn not_found() -> Self {
        Self::new(404, "not_found", "not found")
    }
    fn conflict(m: impl Into<String>) -> Self {
        Self::new(409, "conflict", m)
    }
}

impl From<BoxError> for ApiErr {
    fn from(e: BoxError) -> Self {
        // 内部エラーの詳細（テーブル名など）はクライアントへ返さずログにだけ残す
        tracing::error!(error = %e, "internal error");
        Self::new(500, "internal", "internal error")
    }
}

type ApiResult<T> = std::result::Result<T, ApiErr>;

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
}

fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_micros() as i64
}

fn random_token(prefix: &str, bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut b);
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(b))
}

/// 秘密値は平文で保存せずハッシュだけ保持する（DB が漏れても再利用できないように）
pub fn secret_hash(s: &str) -> String {
    let d = Sha256::digest(s.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn blob_key(transfer_id: &str, file: u32, index: u32) -> String {
    format!("transfers/{transfer_id}/{file}/{index}")
}

pub fn blob_prefix(transfer_id: &str) -> String {
    format!("transfers/{transfer_id}/")
}

fn parse<T: DeserializeOwned>(body: &[u8]) -> ApiResult<T> {
    serde_json::from_slice(body).map_err(|e| ApiErr::bad(format!("invalid json: {e}")))
}

fn validate_file_name(name: &str) -> ApiResult<()> {
    let bad = name.is_empty()
        || name.len() > 255
        || name == "."
        || name == ".."
        || name
            .chars()
            .any(|c| c == '/' || c == '\\' || c == '\0' || c.is_control());
    if bad {
        Err(ApiErr::bad(format!("invalid file name: {name:?}")))
    } else {
        Ok(())
    }
}

fn validate_accepts(accepts: Vec<TransferKind>) -> Vec<TransferKind> {
    // 集合として扱う（重複した申告で表示や判定がぶれないように）
    let mut out: Vec<TransferKind> = Vec::new();
    for k in accepts {
        if !out.contains(&k) {
            out.push(k);
        }
    }
    out
}

/// Push Subscription の URL として受け付けるホスト。任意 URL を許すと、サーバーから内部ネットワークや
/// 第三者へリクエストを送らせる SSRF の踏み台になるため、主要な push service に限定する。
const PUSH_SERVICE_HOSTS: &[&str] = &[
    "fcm.googleapis.com",
    "updates.push.services.mozilla.com",
    "push.apple.com",
    "notify.windows.com",
];

pub fn is_allowed_push_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // userinfo（"host@evil"）やポート指定を含む URL は正規の購読では現れないので拒否する
    if authority.contains('@') || authority.contains(':') || url.len() > 2048 {
        return false;
    }
    let host = authority.to_ascii_lowercase();
    PUSH_SERVICE_HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

/// Web Socket の $connect で認証に成功した結果
#[derive(Debug, Clone)]
pub struct WsAccepted {
    pub endpoint_id: String,
    /// ハンドシェイク応答で echo すべきサブプロトコル（ticket で認証した Browser のみ）。
    /// クライアントが要求したサブプロトコルを返さないと Browser は接続を失敗させる
    pub subprotocol: Option<&'static str>,
}

/// Namespace 名の検証。管理者が入力する値で、ログや管理出力にそのまま出るため、紛らわしい文字を含めない
pub fn validate_namespace(ns: &str) -> std::result::Result<(), String> {
    let ok = !ns.is_empty()
        && ns.len() <= 64
        && ns
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && ns
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "invalid namespace {ns:?}: use 1..64 chars of [a-z0-9_-], starting with [a-z0-9]"
        ))
    }
}

/// 管理経路の Endpoint 一覧の 1 件。管理者は全 Namespace を見るため、所属を併記する
#[derive(Debug, Clone, Serialize)]
pub struct AdminEndpointInfo {
    pub namespace: String,
    #[serde(flatten)]
    pub endpoint: EndpointInfo,
}

fn validate_endpoint_name(name: &str) -> ApiResult<String> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 64 || n.chars().any(|c| c.is_control()) {
        return Err(ApiErr::bad("endpoint name must be 1..64 printable chars"));
    }
    Ok(n.to_string())
}

pub struct Core<S, B, N, P = NoPush> {
    pub store: S,
    pub blob: B,
    pub notifier: N,
    pub pusher: P,
    pub cfg: Config,
}

impl<S: Store, B: BlobStore, N: Notifier> Core<S, B, N, NoPush> {
    pub fn new(store: S, blob: B, notifier: N, cfg: Config) -> Self {
        Self {
            store,
            blob,
            notifier,
            pusher: NoPush,
            cfg,
        }
    }
}

impl<S, B, N, P> Core<S, B, N, P> {
    pub fn with_pusher<P2: Pusher>(self, pusher: P2) -> Core<S, B, N, P2> {
        Core {
            store: self.store,
            blob: self.blob,
            notifier: self.notifier,
            pusher,
            cfg: self.cfg,
        }
    }
}

impl<S: Store, B: BlobStore, N: Notifier, P: Pusher> Core<S, B, N, P> {
    // ---------- 管理操作（IAM で保護された経路からのみ呼ぶ） ----------

    /// 発行した Key で登録した Endpoint は `namespace` に属する（クライアントは Namespace を指定できない）
    pub async fn issue_enrollment_key(&self, namespace: &str) -> Result<(String, i64)> {
        validate_namespace(namespace)?;
        let key = random_token(ENROLLMENT_KEY_PREFIX, 20);
        let exp = now() + self.cfg.enrollment_key_ttl_secs;
        self.store
            .put_enrollment_key(&secret_hash(&key), namespace, exp)
            .await?;
        Ok((key, exp))
    }

    pub async fn revoke_endpoint(&self, endpoint_id: &str) -> Result<()> {
        // 通知先を決めるため削除前に所属を引く。既に無い ID でも、残った転送の後始末は従来どおり行う
        let namespace = self.store.get_endpoint(endpoint_id).await?.map(|e| e.namespace);
        self.store.delete_endpoint(endpoint_id).await?;
        self.store.delete_tokens_of(endpoint_id).await?;
        // 失効した Endpoint へ Push し続けないよう購読も消す
        for s in self.store.list_push_subscriptions(now()).await? {
            if s.endpoint_id == endpoint_id {
                self.store.delete_push_subscription(endpoint_id, &s.url_hash).await?;
            }
        }
        // 失効した Endpoint が関わる未完了の転送は完了し得ないので、取り消して一時データを即削除する
        for t in self.store.list_transfers(now()).await? {
            if (t.sender == endpoint_id || t.receiver == endpoint_id)
                && self
                    .store
                    .transition(
                        &t.transfer_id,
                        &[TransferState::Uploading, TransferState::Uploaded],
                        TransferState::Cancelled,
                    )
                    .await?
            {
                self.blob.delete_prefix(&blob_prefix(&t.transfer_id)).await?;
            }
        }
        if let Some(ns) = namespace {
            self.broadcast(&ns, None, &ServerEvent::EndpointsChanged).await;
        }
        Ok(())
    }

    /// `namespace` が None なら全 Namespace
    pub async fn list_endpoints_admin(&self, namespace: Option<&str>) -> Result<Vec<AdminEndpointInfo>> {
        Ok(self
            .endpoint_infos(namespace)
            .await?
            .into_iter()
            .map(|(namespace, endpoint)| AdminEndpointInfo { namespace, endpoint })
            .collect())
    }

    // ---------- HTTP ----------

    pub async fn handle_http(&self, req: Request) -> Response {
        match self.route(&req).await {
            Ok(r) => r,
            Err(e) => {
                if e.status >= 500 {
                    tracing::warn!(status = e.status, code = e.code, path = %req.path, "request failed");
                }
                Response::json(
                    e.status,
                    &tsute_proto::ApiError {
                        error: e.code.into(),
                        message: e.message,
                    },
                )
            }
        }
    }

    async fn route(&self, req: &Request) -> ApiResult<Response> {
        let path = req.path.split('?').next().unwrap_or("");
        let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
        let m = req.method.as_str();
        match (m, seg.as_slice()) {
            ("GET", ["api", "health"]) => Ok(Response::json(
                200,
                &serde_json::json!({"ok": true, "protocol_version": PROTOCOL_VERSION}),
            )),
            ("POST", ["api", "enroll"]) => self.enroll(parse(&req.body)?).await,
            ("POST", ["api", "auth", "challenge"]) => self.challenge(parse(&req.body)?).await,
            ("POST", ["api", "auth", "token"]) => self.token(parse(&req.body)?).await,
            _ => {
                let ep = self.authenticate(&req.headers).await?;
                let (me, ns) = (ep.endpoint_id.as_str(), ep.namespace.as_str());
                match (m, seg.as_slice()) {
                    ("GET", ["api", "me"]) => self.me(me, ns).await,
                    ("PUT", ["api", "me", "name"]) => self.rename(me, ns, parse(&req.body)?).await,
                    ("PUT", ["api", "me", "capabilities"]) => self.set_capabilities(me, ns, parse(&req.body)?).await,
                    ("POST", ["api", "ws-ticket"]) => self.ws_ticket(me).await,
                    ("GET", ["api", "push", "config"]) => Ok(Response::json(
                        200,
                        &PushConfig {
                            vapid_public_key: self.pusher.vapid_public_key(),
                        },
                    )),
                    ("PUT", ["api", "push", "subscription"]) => self.put_push(me, ns, parse(&req.body)?).await,
                    ("DELETE", ["api", "push", "subscription"]) => self.delete_push(me, ns, parse(&req.body)?).await,
                    // 別 Namespace の Endpoint は存在自体を見せない
                    ("GET", ["api", "endpoints"]) => Ok(Response::json(
                        200,
                        &EndpointList {
                            endpoints: self
                                .endpoint_infos(Some(ns))
                                .await?
                                .into_iter()
                                .map(|(_, e)| e)
                                .collect(),
                        },
                    )),
                    ("POST", ["api", "transfers"]) => self.create_transfer(me, ns, parse(&req.body)?).await,
                    ("GET", ["api", "transfers"]) => self.list_transfers(me).await,
                    ("GET", ["api", "transfers", id]) => {
                        let (t, chunks) = self.load_for(me, id).await?;
                        Ok(Response::json(200, &TransferDetail { transfer: t, chunks }))
                    }
                    ("DELETE", ["api", "transfers", id]) => self.cancel(me, id).await,
                    ("POST", ["api", "transfers", id, "upload-urls"]) => {
                        self.upload_urls(me, id, parse(&req.body)?).await
                    }
                    ("POST", ["api", "transfers", id, "chunks"]) => {
                        self.chunks_complete(me, id, parse(&req.body)?).await
                    }
                    ("POST", ["api", "transfers", id, "files", file, "finalize"]) => {
                        let file: u32 = file.parse().map_err(|_| ApiErr::bad("file index"))?;
                        self.finalize_file(me, id, file, parse(&req.body)?).await
                    }
                    ("POST", ["api", "transfers", id, "download-urls"]) => {
                        self.download_urls(me, id, parse(&req.body)?).await
                    }
                    ("POST", ["api", "transfers", id, "received"]) => self.received(me, id).await,
                    _ => Err(ApiErr::not_found()),
                }
            }
        }
    }

    /// 認証した Endpoint のレコード。Namespace 境界の判定に使うため ID だけでなく所属も返す
    async fn authenticate(&self, headers: &HashMap<String, String>) -> ApiResult<EndpointRecord> {
        let token = headers
            .get("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(ApiErr::unauthorized)?;
        if !token.starts_with(TOKEN_PREFIX) {
            return Err(ApiErr::unauthorized());
        }
        let ep = self
            .store
            .get_token(&secret_hash(token), now())
            .await?
            .ok_or_else(ApiErr::unauthorized)?;
        // 失効（revoke）された Endpoint のトークンは即座に無効にする
        self.store.get_endpoint(&ep).await?.ok_or_else(ApiErr::unauthorized)
    }

    async fn enroll(&self, r: EnrollRequest) -> ApiResult<Response> {
        let name = validate_endpoint_name(&r.name)?;
        let pk = URL_SAFE_NO_PAD
            .decode(&r.public_key)
            .map_err(|_| ApiErr::bad("public_key encoding"))?;
        let pk: [u8; 32] = pk.try_into().map_err(|_| ApiErr::bad("public_key length"))?;
        VerifyingKey::from_bytes(&pk).map_err(|_| ApiErr::bad("public_key invalid"))?;
        let namespace = if r.enrollment_key.starts_with(ENROLLMENT_KEY_PREFIX) {
            self.store
                .consume_enrollment_key(&secret_hash(&r.enrollment_key), now())
                .await?
        } else {
            None
        };
        // 鍵の存在有無を区別できる情報は返さない
        let Some(namespace) = namespace else {
            return Err(ApiErr::new(
                403,
                "invalid_enrollment_key",
                "enrollment key is invalid, used, or expired",
            ));
        };
        let ep = EndpointRecord {
            endpoint_id: format!("ep_{}", uuid::Uuid::now_v7().simple()),
            name,
            platform: r.platform,
            public_key: r.public_key,
            created_at: now(),
            // 所属は Key が決める。クライアントの申告は受け付けない
            namespace,
            client_kind: r.client_kind,
            accepts: r.accepts.map(validate_accepts),
        };
        self.store.put_endpoint(&ep).await?;
        tracing::info!(endpoint_id = %ep.endpoint_id, namespace = %ep.namespace, "endpoint enrolled");
        self.broadcast(&ep.namespace, None, &ServerEvent::EndpointsChanged)
            .await;
        Ok(Response::json(
            200,
            &EnrollResponse {
                endpoint_id: ep.endpoint_id,
            },
        ))
    }

    async fn challenge(&self, r: ChallengeRequest) -> ApiResult<Response> {
        // 存在しない Endpoint にも同じ形で応答し、ID の存在確認に使われないようにする
        let nonce = random_token("", 24);
        let exp = now() + self.cfg.challenge_ttl_secs;
        if self.store.get_endpoint(&r.endpoint_id).await?.is_some() {
            self.store.put_challenge(&nonce, &r.endpoint_id, exp).await?;
        }
        Ok(Response::json(200, &ChallengeResponse { nonce, expires_at: exp }))
    }

    async fn token(&self, r: TokenRequest) -> ApiResult<Response> {
        let fail = || ApiErr::new(401, "auth_failed", "authentication failed");
        let ep = self.store.get_endpoint(&r.endpoint_id).await?.ok_or_else(fail)?;
        let pk: [u8; 32] = URL_SAFE_NO_PAD
            .decode(&ep.public_key)
            .ok()
            .and_then(|v| v.try_into().ok())
            .ok_or_else(fail)?;
        let vk = VerifyingKey::from_bytes(&pk).map_err(|_| fail())?;
        let sig: [u8; 64] = URL_SAFE_NO_PAD
            .decode(&r.signature)
            .ok()
            .and_then(|v| v.try_into().ok())
            .ok_or_else(fail)?;
        vk.verify(
            &auth_signing_message(&r.endpoint_id, &r.nonce),
            &Signature::from_bytes(&sig),
        )
        .map_err(|_| fail())?;
        // 署名検証後に消費する: 検証前に消すと第三者が他人の nonce を無効化できてしまう
        if !self.store.consume_challenge(&r.nonce, &r.endpoint_id, now()).await? {
            return Err(fail());
        }
        let token = random_token(TOKEN_PREFIX, 32);
        let exp = now() + self.cfg.token_ttl_secs;
        self.store.put_token(&secret_hash(&token), &r.endpoint_id, exp).await?;
        Ok(Response::json(
            200,
            &TokenResponse {
                access_token: token,
                expires_at: exp,
            },
        ))
    }

    /// (所属 Namespace, 情報)。`namespace` が Some ならその Namespace の Endpoint だけ
    async fn endpoint_infos(&self, namespace: Option<&str>) -> Result<Vec<(String, EndpointInfo)>> {
        let conns = self.store.list_connections(now()).await?;
        let subs = if self.pusher.vapid_public_key().is_some() {
            self.store.list_push_subscriptions(now()).await?
        } else {
            // Push を送れない環境では購読があっても到達手段にならない
            vec![]
        };
        let mut eps: Vec<EndpointRecord> = self
            .store
            .list_endpoints()
            .await?
            .into_iter()
            .filter(|e| namespace.is_none_or(|ns| e.namespace == ns))
            .collect();
        eps.sort_by_key(|e| e.created_at);
        Ok(eps
            .iter()
            .map(|e| {
                let mut reach = Vec::new();
                if conns.iter().any(|c| c.endpoint_id == e.endpoint_id) {
                    reach.push(Reach::Websocket);
                }
                if subs.iter().any(|s| s.endpoint_id == e.endpoint_id) {
                    reach.push(Reach::WebPush);
                }
                (e.namespace.clone(), e.info(reach))
            })
            .collect())
    }

    async fn set_capabilities(&self, me: &str, ns: &str, r: CapabilitiesRequest) -> ApiResult<Response> {
        let mut ep = self.store.get_endpoint(me).await?.ok_or_else(ApiErr::unauthorized)?;
        ep.accepts = Some(validate_accepts(r.accepts));
        self.store.put_endpoint(&ep).await?;
        self.broadcast(ns, None, &ServerEvent::EndpointsChanged).await;
        self.me(me, ns).await
    }

    async fn ws_ticket(&self, me: &str) -> ApiResult<Response> {
        let ticket = random_token(WS_TICKET_PREFIX, 24);
        let exp = now() + self.cfg.ws_ticket_ttl_secs;
        self.store.put_ws_ticket(&secret_hash(&ticket), me, exp).await?;
        Ok(Response::json(
            200,
            &WsTicketResponse {
                ticket,
                expires_at: exp,
            },
        ))
    }

    async fn put_push(&self, me: &str, ns: &str, r: PushSubscriptionRequest) -> ApiResult<Response> {
        if self.pusher.vapid_public_key().is_none() {
            return Err(ApiErr::new(404, "push_disabled", "web push is not configured"));
        }
        if !is_allowed_push_url(&r.endpoint) {
            return Err(ApiErr::bad("push endpoint is not an allowed push service"));
        }
        let h = secret_hash(&r.endpoint);
        let mut mine: Vec<PushSubscriptionRecord> = self
            .store
            .list_push_subscriptions(now())
            .await?
            .into_iter()
            .filter(|s| s.endpoint_id == me && s.url_hash != h)
            .collect();
        // 上限を超えたら古いものから消す（Browser が購読を作り直すと古い URL は届かなくなるため）
        mine.sort_by_key(|s| s.updated_at_us);
        while mine.len() >= MAX_PUSH_SUBSCRIPTIONS_PER_ENDPOINT {
            let old = mine.remove(0);
            self.store.delete_push_subscription(me, &old.url_hash).await?;
        }
        self.store
            .put_push_subscription(&PushSubscriptionRecord {
                endpoint_id: me.into(),
                url_hash: h,
                url: r.endpoint,
                updated_at_us: now_us(),
                expires_at: now() + self.cfg.push_subscription_ttl_secs,
            })
            .await?;
        self.broadcast(ns, None, &ServerEvent::EndpointsChanged).await;
        Ok(Response::json(200, &serde_json::json!({"ok": true})))
    }

    async fn delete_push(&self, me: &str, ns: &str, r: PushSubscriptionRequest) -> ApiResult<Response> {
        // キーに自分の endpoint_id を含むため、他 Endpoint の購読は消せない
        self.store
            .delete_push_subscription(me, &secret_hash(&r.endpoint))
            .await?;
        self.broadcast(ns, None, &ServerEvent::EndpointsChanged).await;
        Ok(Response::json(200, &serde_json::json!({"ok": true})))
    }

    async fn me(&self, me: &str, ns: &str) -> ApiResult<Response> {
        let infos = self.endpoint_infos(Some(ns)).await?;
        let endpoint = infos
            .into_iter()
            .map(|(_, e)| e)
            .find(|e| e.endpoint_id == me)
            .ok_or_else(ApiErr::unauthorized)?;
        Ok(Response::json(
            200,
            &MeResponse {
                endpoint,
                protocol_version: PROTOCOL_VERSION,
            },
        ))
    }

    async fn rename(&self, me: &str, ns: &str, r: RenameRequest) -> ApiResult<Response> {
        let name = validate_endpoint_name(&r.name)?;
        let mut ep = self.store.get_endpoint(me).await?.ok_or_else(ApiErr::unauthorized)?;
        ep.name = name;
        self.store.put_endpoint(&ep).await?;
        self.broadcast(ns, None, &ServerEvent::EndpointsChanged).await;
        self.me(me, ns).await
    }

    async fn create_transfer(&self, me: &str, ns: &str, r: CreateTransferRequest) -> ApiResult<Response> {
        if r.receiver == me {
            return Err(ApiErr::bad("cannot send to self"));
        }
        // 別 Namespace の Endpoint は未登録と同じ応答にし、ID を直接指定されても送れず存在も判別できないようにする。
        // 送受信者が同じ Namespace の Transfer しか作られないため、以後の Transfer 操作（当事者のみ可）も境界内に収まる
        let receiver = self
            .store
            .get_endpoint(&r.receiver)
            .await?
            .filter(|e| e.namespace == ns)
            .ok_or_else(|| ApiErr::bad("unknown receiver"))?;
        // UI を迂回した呼び出しでも、受信側が扱えない Payload は作らない（ADR-0015 の多層防御）
        if !receiver.accepts().contains(&r.kind) {
            return Err(ApiErr::new(
                422,
                "receiver_cannot_accept",
                "the receiver endpoint cannot receive this kind of payload",
            ));
        }
        let chunk_size = r.chunk_size.unwrap_or(DEFAULT_CHUNK_SIZE);
        if !(MIN_CHUNK_SIZE..=MAX_CHUNK_SIZE).contains(&chunk_size) {
            return Err(ApiErr::bad("chunk_size out of range"));
        }
        match (r.kind, &r.text) {
            (TransferKind::ClipboardText, Some(t)) => {
                if t.len() > INLINE_TEXT_MAX_BYTES {
                    return Err(ApiErr::bad("inline text too large; upload as file"));
                }
                if !r.files.is_empty() {
                    return Err(ApiErr::bad("inline text transfer must not have files"));
                }
            }
            (_, Some(_)) => return Err(ApiErr::bad("text is only for clipboard_text")),
            (_, None) => {
                if r.files.is_empty() || r.files.len() > MAX_FILES_PER_TRANSFER {
                    return Err(ApiErr::bad("file count out of range"));
                }
                if r.kind != TransferKind::Files && r.files.len() != 1 {
                    return Err(ApiErr::bad("clipboard transfer carries exactly one item"));
                }
            }
        }
        let mut total = 0u64;
        let mut files = Vec::with_capacity(r.files.len());
        for (i, f) in r.files.into_iter().enumerate() {
            validate_file_name(&f.name)?;
            if f.mime.len() > 255 {
                return Err(ApiErr::bad("mime too long"));
            }
            total = total.saturating_add(f.size);
            files.push(FileEntry {
                index: i as u32,
                chunk_count: chunk_count(f.size, chunk_size),
                name: f.name,
                size: f.size,
                mime: f.mime,
                media: f.media,
                sha256: None,
            });
        }
        if total > MAX_TRANSFER_BYTES {
            return Err(ApiErr::bad("transfer too large"));
        }
        let created = now();
        let inline = r.text.is_some();
        let t = Transfer {
            transfer_id: format!("tr_{}", uuid::Uuid::now_v7().simple()),
            sender: me.to_string(),
            receiver: r.receiver,
            kind: r.kind,
            // inline テキストはアップロードすべきデータがないので即 Uploaded
            state: if inline {
                TransferState::Uploaded
            } else {
                TransferState::Uploading
            },
            created_at: created,
            expires_at: created + self.cfg.transfer_ttl_secs,
            chunk_size,
            text: r.text,
            files,
        };
        self.store.put_transfer(&t).await?;
        tracing::info!(transfer_id = %t.transfer_id, kind = ?t.kind, bytes = t.total_bytes(), "transfer created");
        let delivered = self
            .notify_endpoint(
                &t.receiver,
                &ServerEvent::TransferCreated {
                    transfer: Box::new(t.clone()),
                },
            )
            .await;
        // Foreground の WebSocket で届いたときは Push しない（iOS は Push ごとに通知表示が必須で二重になるため）
        if !delivered {
            self.push_endpoint(&t.receiver).await;
        }
        Ok(Response::json(200, &t))
    }

    async fn list_transfers(&self, me: &str) -> ApiResult<Response> {
        let mut transfers: Vec<Transfer> = self
            .store
            .list_transfers(now())
            .await?
            .into_iter()
            .filter(|t| t.sender == me || t.receiver == me)
            .collect();
        transfers.sort_by_key(|t| t.created_at);
        Ok(Response::json(200, &TransferList { transfers }))
    }

    async fn load_for(&self, me: &str, id: &str) -> ApiResult<(Transfer, Vec<ChunkInfo>)> {
        let t = self.store.get_transfer(id).await?.ok_or_else(ApiErr::not_found)?;
        // 当事者以外には存在自体を見せない
        if (t.sender != me && t.receiver != me) || t.expires_at <= now() {
            return Err(ApiErr::not_found());
        }
        let chunks = self.store.list_chunks(id).await?;
        Ok((t, chunks))
    }

    fn check_chunk(t: &Transfer, file: u32, index: u32) -> ApiResult<()> {
        let f = t
            .files
            .get(file as usize)
            .ok_or_else(|| ApiErr::bad("file index out of range"))?;
        if index >= f.chunk_count {
            return Err(ApiErr::bad("chunk index out of range"));
        }
        Ok(())
    }

    async fn upload_urls(&self, me: &str, id: &str, r: UploadUrlRequest) -> ApiResult<Response> {
        let (t, _) = self.load_for(me, id).await?;
        if t.sender != me {
            return Err(ApiErr::not_found());
        }
        if t.state != TransferState::Uploading {
            return Err(ApiErr::conflict("transfer is not uploading"));
        }
        if r.chunks.len() > 100 {
            return Err(ApiErr::bad("too many chunks per request"));
        }
        let exp = now() + self.cfg.presign_ttl_secs as i64;
        let mut urls = Vec::with_capacity(r.chunks.len());
        for c in &r.chunks {
            Self::check_chunk(&t, c.file, c.index)?;
            if c.size != t.chunk_len(c.file, c.index) {
                return Err(ApiErr::bad("chunk size mismatch"));
            }
            let p = self
                .blob
                .presign_put(
                    &blob_key(id, c.file, c.index),
                    c.size,
                    &c.sha256,
                    self.cfg.presign_ttl_secs,
                )
                .await?;
            urls.push(PresignedUrl {
                file: c.file,
                index: c.index,
                url: p.url,
                headers: p.headers,
                expires_at: exp,
            });
        }
        Ok(Response::json(200, &PresignedUrls { urls }))
    }

    async fn chunks_complete(&self, me: &str, id: &str, r: ChunkCompleteRequest) -> ApiResult<Response> {
        let (t, _) = self.load_for(me, id).await?;
        if t.sender != me {
            return Err(ApiErr::not_found());
        }
        if t.state != TransferState::Uploading {
            return Err(ApiErr::conflict("transfer is not uploading"));
        }
        let mut done = Vec::new();
        for c in &r.chunks {
            Self::check_chunk(&t, c.file, c.index)?;
            // クライアントの自己申告を信用せず、実際に保存された Object を確認する
            match self.blob.head(&blob_key(id, c.file, c.index)).await? {
                Some((size, sha)) if size == c.size && sha.as_deref().is_none_or(|s| s == c.sha256) => {
                    self.store.put_chunk(id, c, t.expires_at).await?;
                    done.push(c.clone());
                }
                _ => {
                    return Err(ApiErr::conflict(format!(
                        "chunk {}/{} not uploaded or mismatched",
                        c.file, c.index
                    )));
                }
            }
        }
        // WebSocket フレーム上限(32KB)を超えないよう分割して通知する
        for part in done.chunks(100) {
            self.notify_endpoint(
                &t.receiver,
                &ServerEvent::ChunksReady {
                    transfer_id: id.into(),
                    chunks: part.to_vec(),
                },
            )
            .await;
        }
        Ok(Response::json(200, &serde_json::json!({"ok": true})))
    }

    async fn finalize_file(&self, me: &str, id: &str, file: u32, r: FinalizeFileRequest) -> ApiResult<Response> {
        let (t, chunks) = self.load_for(me, id).await?;
        if t.sender != me {
            return Err(ApiErr::not_found());
        }
        let f = t
            .files
            .get(file as usize)
            .ok_or_else(|| ApiErr::bad("file index out of range"))?;
        let have = chunks.iter().filter(|c| c.file == file).count() as u32;
        if have != f.chunk_count {
            return Err(ApiErr::conflict("not all chunks uploaded"));
        }
        if STANDARD.decode(&r.sha256).map(|v| v.len()) != Ok(32) {
            return Err(ApiErr::bad("sha256 must be base64 of 32 bytes"));
        }
        self.store.put_file_sha(id, file, &r.sha256, t.expires_at).await?;
        let t = self.store.get_transfer(id).await?.ok_or_else(ApiErr::not_found)?;
        if t.files.iter().all(|f| f.sha256.is_some())
            && self
                .store
                .transition(id, &[TransferState::Uploading], TransferState::Uploaded)
                .await?
        {
            let ev = ServerEvent::TransferState {
                transfer_id: id.into(),
                state: TransferState::Uploaded,
            };
            self.notify_endpoint(&t.receiver, &ev).await;
            self.notify_endpoint(&t.sender, &ev).await;
        }
        Ok(Response::json(200, &t))
    }

    async fn download_urls(&self, me: &str, id: &str, r: DownloadUrlRequest) -> ApiResult<Response> {
        let (t, chunks) = self.load_for(me, id).await?;
        if t.receiver != me {
            return Err(ApiErr::not_found());
        }
        if t.state.is_terminal() {
            return Err(ApiErr::conflict("transfer already finished"));
        }
        if r.chunks.len() > 100 {
            return Err(ApiErr::bad("too many chunks per request"));
        }
        let exp = now() + self.cfg.presign_ttl_secs as i64;
        let mut urls = Vec::new();
        for c in &r.chunks {
            if !chunks.iter().any(|x| x.file == c.file && x.index == c.index) {
                return Err(ApiErr::conflict("chunk not ready"));
            }
            let url = self
                .blob
                .presign_get(&blob_key(id, c.file, c.index), self.cfg.presign_ttl_secs)
                .await?;
            urls.push(PresignedUrl {
                file: c.file,
                index: c.index,
                url,
                headers: vec![],
                expires_at: exp,
            });
        }
        Ok(Response::json(200, &PresignedUrls { urls }))
    }

    async fn received(&self, me: &str, id: &str) -> ApiResult<Response> {
        let (t, _) = self.load_for(me, id).await?;
        if t.receiver != me {
            return Err(ApiErr::not_found());
        }
        if t.state == TransferState::Received {
            return Ok(Response::json(200, &serde_json::json!({"ok": true})));
        }
        if !self
            .store
            .transition(id, &[TransferState::Uploaded], TransferState::Received)
            .await?
        {
            return Err(ApiErr::conflict("transfer is not fully uploaded"));
        }
        self.blob.delete_prefix(&blob_prefix(id)).await?;
        let ev = ServerEvent::TransferState {
            transfer_id: id.into(),
            state: TransferState::Received,
        };
        self.notify_endpoint(&t.sender, &ev).await;
        self.notify_endpoint(&t.receiver, &ev).await;
        Ok(Response::json(200, &serde_json::json!({"ok": true})))
    }

    async fn cancel(&self, me: &str, id: &str) -> ApiResult<Response> {
        let (t, _) = self.load_for(me, id).await?;
        let ok = self
            .store
            .transition(
                id,
                &[TransferState::Uploading, TransferState::Uploaded],
                TransferState::Cancelled,
            )
            .await?;
        if ok {
            self.blob.delete_prefix(&blob_prefix(id)).await?;
            let ev = ServerEvent::TransferState {
                transfer_id: id.into(),
                state: TransferState::Cancelled,
            };
            self.notify_endpoint(&t.sender, &ev).await;
            self.notify_endpoint(&t.receiver, &ev).await;
        }
        Ok(Response::json(200, &serde_json::json!({"ok": ok})))
    }

    // ---------- WebSocket ----------

    /// Browser 用: `Sec-WebSocket-Protocol: tsute.v1, ticket.<ticket>` の ticket を一回限りで消費する。
    /// URL クエリにしないのは CloudFront / API Gateway のアクセスログに残りうるため。
    async fn authenticate_ticket(&self, headers: &HashMap<String, String>) -> ApiResult<EndpointRecord> {
        let protos: Vec<&str> = headers
            .get("sec-websocket-protocol")
            .map(|v| v.split(',').map(str::trim).collect())
            .unwrap_or_default();
        if !protos.contains(&WS_SUBPROTOCOL) {
            return Err(ApiErr::unauthorized());
        }
        let ticket = protos
            .iter()
            .find_map(|p| p.strip_prefix(WS_TICKET_SUBPROTOCOL_PREFIX))
            .filter(|t| t.starts_with(WS_TICKET_PREFIX))
            .ok_or_else(ApiErr::unauthorized)?;
        let ep = self
            .store
            .consume_ws_ticket(&secret_hash(ticket), now())
            .await?
            .ok_or_else(ApiErr::unauthorized)?;
        self.store.get_endpoint(&ep).await?.ok_or_else(ApiErr::unauthorized)
    }

    /// $connect。Err を返すと接続を拒否する。
    pub async fn ws_connect(
        &self,
        connection_id: &str,
        headers: &HashMap<String, String>,
    ) -> std::result::Result<WsAccepted, ApiErr> {
        // Native は従来どおり Authorization ヘッダ。ヘッダが無いときだけ ticket を見る
        let (ep, subprotocol) = if headers.contains_key("authorization") {
            (self.authenticate(headers).await?, None)
        } else {
            (self.authenticate_ticket(headers).await?, Some(WS_SUBPROTOCOL))
        };
        self.store
            .put_connection(&ConnectionRecord {
                connection_id: connection_id.into(),
                endpoint_id: ep.endpoint_id.clone(),
                connected_at: now(),
                expires_at: now() + self.cfg.connection_ttl_secs,
            })
            .await?;
        tracing::info!(endpoint_id = %ep.endpoint_id, "ws connected");
        self.broadcast(
            &ep.namespace,
            Some(connection_id),
            &ServerEvent::Presence {
                endpoint_id: ep.endpoint_id.clone(),
                online: true,
            },
        )
        .await;
        Ok(WsAccepted {
            endpoint_id: ep.endpoint_id,
            subprotocol,
        })
    }

    pub async fn ws_disconnect(&self, connection_id: &str) -> Result<()> {
        if let Some(ep) = self.store.delete_connection(connection_id).await? {
            let still_online = self
                .store
                .list_connections(now())
                .await?
                .iter()
                .any(|c| c.endpoint_id == ep);
            // 失効済みで所属が引けない Endpoint は、失効時に EndpointsChanged を通知済みなので送らない
            if !still_online && let Some(rec) = self.store.get_endpoint(&ep).await? {
                self.broadcast(
                    &rec.namespace,
                    None,
                    &ServerEvent::Presence {
                        endpoint_id: ep,
                        online: false,
                    },
                )
                .await;
            }
        }
        Ok(())
    }

    /// クライアントからのメッセージ。応答イベントを返す（呼び出し側が同じ接続へ送る）。
    pub async fn ws_message(&self, connection_id: &str, body: &str) -> Option<ServerEvent> {
        match serde_json::from_str::<ClientMessage>(body) {
            Ok(ClientMessage::Ping) => {
                // ping を機に接続レコードの寿命を延ばす（切断イベント欠落時の掃除用 TTL）
                if let Ok(conns) = self.store.list_connections(now()).await
                    && let Some(c) = conns.into_iter().find(|c| c.connection_id == connection_id)
                {
                    let _ = self
                        .store
                        .put_connection(&ConnectionRecord {
                            expires_at: now() + self.cfg.connection_ttl_secs,
                            ..c
                        })
                        .await;
                }
                Some(ServerEvent::Pong)
            }
            Err(_) => None,
        }
    }

    pub async fn hello_event(&self, connection_id: &str, endpoint_id: &str) -> ServerEvent {
        ServerEvent::Hello {
            endpoint_id: endpoint_id.into(),
            connection_id: connection_id.into(),
        }
    }

    /// 送れたら true
    async fn send_or_prune(&self, c: &ConnectionRecord, ev: &ServerEvent) -> bool {
        match self.notifier.send(&c.connection_id, ev).await {
            Ok(true) => true,
            // 接続直後（API Gateway の $connect 完了前）は送信が失敗し得るが、切断ではないので消さない
            Ok(false) if now() - c.connected_at > 30 => {
                let _ = self.store.delete_connection(&c.connection_id).await;
                false
            }
            Ok(false) => false,
            // 通知はベストエフォート。クライアントは HTTP で再同期できるので失敗で API を失敗させない
            Err(e) => {
                tracing::warn!(error = %e, "notify failed");
                false
            }
        }
    }

    /// いずれかの接続へ送れたら true
    async fn notify_endpoint(&self, endpoint_id: &str, ev: &ServerEvent) -> bool {
        let Ok(conns) = self.store.list_connections(now()).await else {
            return false;
        };
        let mut delivered = false;
        for c in conns.iter().filter(|c| c.endpoint_id == endpoint_id) {
            delivered |= self.send_or_prune(c, ev).await;
        }
        delivered
    }

    /// Web Push はヒントに過ぎない（本文は App が起動後に API から取る）ので、失敗しても API は成功させる
    async fn push_endpoint(&self, endpoint_id: &str) {
        if self.pusher.vapid_public_key().is_none() {
            return;
        }
        let Ok(subs) = self.store.list_push_subscriptions(now()).await else {
            return;
        };
        for s in subs.iter().filter(|s| s.endpoint_id == endpoint_id) {
            // 保存時にも検証しているが、送信直前にも確かめて SSRF の余地を残さない
            if !is_allowed_push_url(&s.url) {
                let _ = self.store.delete_push_subscription(endpoint_id, &s.url_hash).await;
                continue;
            }
            match self.pusher.push(&s.url).await {
                Ok(PushOutcome::Sent) => {}
                Ok(PushOutcome::Gone) => {
                    tracing::info!(endpoint_id, "push subscription gone");
                    let _ = self.store.delete_push_subscription(endpoint_id, &s.url_hash).await;
                }
                Err(e) => tracing::warn!(error = %e, "push failed"),
            }
        }
    }

    /// `namespace` の Endpoint の接続にだけ送る（Presence 等で別 Namespace の Endpoint ID を漏らさないため）
    async fn broadcast(&self, namespace: &str, except: Option<&str>, ev: &ServerEvent) {
        let (Ok(conns), Ok(eps)) = (
            self.store.list_connections(now()).await,
            self.store.list_endpoints().await,
        ) else {
            return;
        };
        let members: std::collections::HashSet<&str> = eps
            .iter()
            .filter(|e| e.namespace == namespace)
            .map(|e| e.endpoint_id.as_str())
            .collect();
        for c in conns
            .iter()
            .filter(|c| Some(c.connection_id.as_str()) != except && members.contains(c.endpoint_id.as_str()))
        {
            self.send_or_prune(c, ev).await;
        }
    }
}
