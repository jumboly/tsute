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

#[derive(Debug, Clone)]
pub struct Config {
    pub enrollment_key_ttl_secs: i64,
    pub challenge_ttl_secs: i64,
    pub token_ttl_secs: i64,
    pub transfer_ttl_secs: i64,
    pub presign_ttl_secs: u64,
    /// API Gateway の最大接続時間(2h)より長くし、切断イベントを取りこぼした接続を最終的に消す
    pub connection_ttl_secs: i64,
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

fn validate_endpoint_name(name: &str) -> ApiResult<String> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 64 || n.chars().any(|c| c.is_control()) {
        return Err(ApiErr::bad("endpoint name must be 1..64 printable chars"));
    }
    Ok(n.to_string())
}

pub struct Core<S, B, N> {
    pub store: S,
    pub blob: B,
    pub notifier: N,
    pub cfg: Config,
}

impl<S: Store, B: BlobStore, N: Notifier> Core<S, B, N> {
    pub fn new(store: S, blob: B, notifier: N, cfg: Config) -> Self {
        Self {
            store,
            blob,
            notifier,
            cfg,
        }
    }

    // ---------- 管理操作（IAM で保護された経路からのみ呼ぶ） ----------

    pub async fn issue_enrollment_key(&self) -> Result<(String, i64)> {
        let key = random_token(ENROLLMENT_KEY_PREFIX, 20);
        let exp = now() + self.cfg.enrollment_key_ttl_secs;
        self.store.put_enrollment_key(&secret_hash(&key), exp).await?;
        Ok((key, exp))
    }

    pub async fn revoke_endpoint(&self, endpoint_id: &str) -> Result<()> {
        self.store.delete_endpoint(endpoint_id).await?;
        self.store.delete_tokens_of(endpoint_id).await?;
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
        self.broadcast(None, &ServerEvent::EndpointsChanged).await;
        Ok(())
    }

    pub async fn list_endpoints_admin(&self) -> Result<Vec<EndpointInfo>> {
        self.endpoint_infos().await
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
                let me = self.authenticate(&req.headers).await?;
                match (m, seg.as_slice()) {
                    ("GET", ["api", "me"]) => self.me(&me).await,
                    ("PUT", ["api", "me", "name"]) => self.rename(&me, parse(&req.body)?).await,
                    ("GET", ["api", "endpoints"]) => Ok(Response::json(
                        200,
                        &EndpointList {
                            endpoints: self.endpoint_infos().await?,
                        },
                    )),
                    ("POST", ["api", "transfers"]) => self.create_transfer(&me, parse(&req.body)?).await,
                    ("GET", ["api", "transfers"]) => self.list_transfers(&me).await,
                    ("GET", ["api", "transfers", id]) => {
                        let (t, chunks) = self.load_for(&me, id).await?;
                        Ok(Response::json(200, &TransferDetail { transfer: t, chunks }))
                    }
                    ("DELETE", ["api", "transfers", id]) => self.cancel(&me, id).await,
                    ("POST", ["api", "transfers", id, "upload-urls"]) => {
                        self.upload_urls(&me, id, parse(&req.body)?).await
                    }
                    ("POST", ["api", "transfers", id, "chunks"]) => {
                        self.chunks_complete(&me, id, parse(&req.body)?).await
                    }
                    ("POST", ["api", "transfers", id, "files", file, "finalize"]) => {
                        let file: u32 = file.parse().map_err(|_| ApiErr::bad("file index"))?;
                        self.finalize_file(&me, id, file, parse(&req.body)?).await
                    }
                    ("POST", ["api", "transfers", id, "download-urls"]) => {
                        self.download_urls(&me, id, parse(&req.body)?).await
                    }
                    ("POST", ["api", "transfers", id, "received"]) => self.received(&me, id).await,
                    _ => Err(ApiErr::not_found()),
                }
            }
        }
    }

    async fn authenticate(&self, headers: &HashMap<String, String>) -> ApiResult<String> {
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
        if self.store.get_endpoint(&ep).await?.is_none() {
            return Err(ApiErr::unauthorized());
        }
        Ok(ep)
    }

    async fn enroll(&self, r: EnrollRequest) -> ApiResult<Response> {
        let name = validate_endpoint_name(&r.name)?;
        let pk = URL_SAFE_NO_PAD
            .decode(&r.public_key)
            .map_err(|_| ApiErr::bad("public_key encoding"))?;
        let pk: [u8; 32] = pk.try_into().map_err(|_| ApiErr::bad("public_key length"))?;
        VerifyingKey::from_bytes(&pk).map_err(|_| ApiErr::bad("public_key invalid"))?;
        if !r.enrollment_key.starts_with(ENROLLMENT_KEY_PREFIX)
            || !self
                .store
                .consume_enrollment_key(&secret_hash(&r.enrollment_key), now())
                .await?
        {
            // 鍵の存在有無を区別できる情報は返さない
            return Err(ApiErr::new(
                403,
                "invalid_enrollment_key",
                "enrollment key is invalid, used, or expired",
            ));
        }
        let ep = EndpointRecord {
            endpoint_id: format!("ep_{}", uuid::Uuid::now_v7().simple()),
            name,
            platform: r.platform,
            public_key: r.public_key,
            created_at: now(),
        };
        self.store.put_endpoint(&ep).await?;
        tracing::info!(endpoint_id = %ep.endpoint_id, "endpoint enrolled");
        self.broadcast(None, &ServerEvent::EndpointsChanged).await;
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

    async fn endpoint_infos(&self) -> Result<Vec<EndpointInfo>> {
        let conns = self.store.list_connections(now()).await?;
        let mut eps = self.store.list_endpoints().await?;
        eps.sort_by_key(|e| e.created_at);
        Ok(eps
            .iter()
            .map(|e| e.info(conns.iter().any(|c| c.endpoint_id == e.endpoint_id)))
            .collect())
    }

    async fn me(&self, me: &str) -> ApiResult<Response> {
        let infos = self.endpoint_infos().await?;
        let endpoint = infos
            .into_iter()
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

    async fn rename(&self, me: &str, r: RenameRequest) -> ApiResult<Response> {
        let name = validate_endpoint_name(&r.name)?;
        let mut ep = self.store.get_endpoint(me).await?.ok_or_else(ApiErr::unauthorized)?;
        ep.name = name;
        self.store.put_endpoint(&ep).await?;
        self.broadcast(None, &ServerEvent::EndpointsChanged).await;
        self.me(me).await
    }

    async fn create_transfer(&self, me: &str, r: CreateTransferRequest) -> ApiResult<Response> {
        if r.receiver == me {
            return Err(ApiErr::bad("cannot send to self"));
        }
        if self.store.get_endpoint(&r.receiver).await?.is_none() {
            return Err(ApiErr::bad("unknown receiver"));
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
        self.notify_endpoint(
            &t.receiver,
            &ServerEvent::TransferCreated {
                transfer: Box::new(t.clone()),
            },
        )
        .await;
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

    /// $connect。Err を返すと接続を拒否する。
    pub async fn ws_connect(
        &self,
        connection_id: &str,
        headers: &HashMap<String, String>,
    ) -> std::result::Result<String, ApiErr> {
        let ep = self.authenticate(headers).await?;
        self.store
            .put_connection(&ConnectionRecord {
                connection_id: connection_id.into(),
                endpoint_id: ep.clone(),
                connected_at: now(),
                expires_at: now() + self.cfg.connection_ttl_secs,
            })
            .await?;
        tracing::info!(endpoint_id = %ep, "ws connected");
        self.broadcast(
            Some(connection_id),
            &ServerEvent::Presence {
                endpoint_id: ep.clone(),
                online: true,
            },
        )
        .await;
        Ok(ep)
    }

    pub async fn ws_disconnect(&self, connection_id: &str) -> Result<()> {
        if let Some(ep) = self.store.delete_connection(connection_id).await? {
            let still_online = self
                .store
                .list_connections(now())
                .await?
                .iter()
                .any(|c| c.endpoint_id == ep);
            if !still_online {
                self.broadcast(
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

    async fn send_or_prune(&self, c: &ConnectionRecord, ev: &ServerEvent) {
        match self.notifier.send(&c.connection_id, ev).await {
            Ok(true) => {}
            // 接続直後（API Gateway の $connect 完了前）は送信が失敗し得るが、切断ではないので消さない
            Ok(false) if now() - c.connected_at > 30 => {
                let _ = self.store.delete_connection(&c.connection_id).await;
            }
            Ok(false) => {}
            // 通知はベストエフォート。クライアントは HTTP で再同期できるので失敗で API を失敗させない
            Err(e) => tracing::warn!(error = %e, "notify failed"),
        }
    }

    async fn notify_endpoint(&self, endpoint_id: &str, ev: &ServerEvent) {
        let Ok(conns) = self.store.list_connections(now()).await else {
            return;
        };
        for c in conns.iter().filter(|c| c.endpoint_id == endpoint_id) {
            self.send_or_prune(c, ev).await;
        }
    }

    async fn broadcast(&self, except: Option<&str>, ev: &ServerEvent) {
        let Ok(conns) = self.store.list_connections(now()).await else {
            return;
        };
        for c in conns.iter().filter(|c| Some(c.connection_id.as_str()) != except) {
            self.send_or_prune(c, ev).await;
        }
    }
}
