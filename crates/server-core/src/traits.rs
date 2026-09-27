//! 実行環境（AWS / ローカル）ごとに差し替える境界。
//!
//! 汎用 KV ではなくドメイン操作単位で定義している。理由: 一回限りの消費や条件付き状態遷移など
//! 「原子的であるべき操作」を実装側（DynamoDB の条件付き書き込み等）に確実に落とし込むため。

use std::future::Future;

use tsute_proto::{
    ChunkInfo, ClientKind, EndpointInfo, NATIVE_ACCEPTS, Platform, Reach, ServerEvent, Transfer, TransferKind,
    TransferState,
};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, BoxError>;

#[derive(Debug, Clone, PartialEq)]
pub struct EndpointRecord {
    pub endpoint_id: String,
    pub name: String,
    pub platform: Platform,
    pub public_key: String,
    pub created_at: i64,
    pub client_kind: ClientKind,
    /// None = 申告なし（Phase 1 の Native レコード）。`NATIVE_ACCEPTS` とみなし、既存データの移行を不要にする
    pub accepts: Option<Vec<TransferKind>>,
}

impl EndpointRecord {
    pub fn accepts(&self) -> Vec<TransferKind> {
        self.accepts.clone().unwrap_or_else(|| NATIVE_ACCEPTS.to_vec())
    }

    pub fn info(&self, reach: Vec<Reach>) -> EndpointInfo {
        EndpointInfo {
            endpoint_id: self.endpoint_id.clone(),
            name: self.name.clone(),
            platform: self.platform,
            online: reach.contains(&Reach::Websocket),
            created_at: self.created_at,
            client_kind: self.client_kind,
            accepts: self.accepts(),
            reach,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PushSubscriptionRecord {
    pub endpoint_id: String,
    /// sha256(url)。同じ購読の再登録を上書きにするためのキー
    pub url_hash: String,
    pub url: String,
    /// 最終登録時刻（マイクロ秒）。上限超過時に「古い順」を秒の同着なく決めるため
    pub updated_at_us: i64,
    pub expires_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionRecord {
    pub connection_id: String,
    pub endpoint_id: String,
    pub connected_at: i64,
    pub expires_at: i64,
}

pub trait Store: Send + Sync {
    fn put_enrollment_key(&self, key_hash: &str, expires_at: i64) -> impl Future<Output = Result<()>> + Send;
    /// 未失効なら削除して true。同時に 2 回呼ばれても true は高々 1 回（一回限り性の保証）。
    fn consume_enrollment_key(&self, key_hash: &str, now: i64) -> impl Future<Output = Result<bool>> + Send;

    fn put_endpoint(&self, ep: &EndpointRecord) -> impl Future<Output = Result<()>> + Send;
    fn get_endpoint(&self, id: &str) -> impl Future<Output = Result<Option<EndpointRecord>>> + Send;
    fn list_endpoints(&self) -> impl Future<Output = Result<Vec<EndpointRecord>>> + Send;
    fn delete_endpoint(&self, id: &str) -> impl Future<Output = Result<()>> + Send;

    fn put_challenge(&self, nonce: &str, endpoint_id: &str, expires_at: i64)
    -> impl Future<Output = Result<()>> + Send;
    /// 未失効かつ endpoint が一致すれば削除して true（リプレイ防止）
    fn consume_challenge(&self, nonce: &str, endpoint_id: &str, now: i64) -> impl Future<Output = Result<bool>> + Send;

    fn put_token(
        &self,
        token_hash: &str,
        endpoint_id: &str,
        expires_at: i64,
    ) -> impl Future<Output = Result<()>> + Send;
    fn get_token(&self, token_hash: &str, now: i64) -> impl Future<Output = Result<Option<String>>> + Send;
    fn delete_tokens_of(&self, endpoint_id: &str) -> impl Future<Output = Result<()>> + Send;

    fn put_connection(&self, c: &ConnectionRecord) -> impl Future<Output = Result<()>> + Send;
    fn delete_connection(&self, connection_id: &str) -> impl Future<Output = Result<Option<String>>> + Send;
    fn list_connections(&self, now: i64) -> impl Future<Output = Result<Vec<ConnectionRecord>>> + Send;

    fn put_ws_ticket(
        &self,
        ticket_hash: &str,
        endpoint_id: &str,
        expires_at: i64,
    ) -> impl Future<Output = Result<()>> + Send;
    /// 未失効なら削除して endpoint_id を返す（一回限り）
    fn consume_ws_ticket(&self, ticket_hash: &str, now: i64) -> impl Future<Output = Result<Option<String>>> + Send;

    fn put_push_subscription(&self, s: &PushSubscriptionRecord) -> impl Future<Output = Result<()>> + Send;
    fn delete_push_subscription(&self, endpoint_id: &str, url_hash: &str) -> impl Future<Output = Result<()>> + Send;
    /// 全 Endpoint 分（`reach` の導出に使う）。件数は Endpoint 数 × 数件に収まる前提
    fn list_push_subscriptions(&self, now: i64) -> impl Future<Output = Result<Vec<PushSubscriptionRecord>>> + Send;

    fn put_transfer(&self, t: &Transfer) -> impl Future<Output = Result<()>> + Send;
    /// finalize 済みファイルの sha256 を反映した Transfer を返す
    fn get_transfer(&self, id: &str) -> impl Future<Output = Result<Option<Transfer>>> + Send;
    fn list_transfers(&self, now: i64) -> impl Future<Output = Result<Vec<Transfer>>> + Send;
    /// state が `from` のいずれかであるときだけ `to` に遷移。遷移したら true。
    fn transition(
        &self,
        id: &str,
        from: &[TransferState],
        to: TransferState,
    ) -> impl Future<Output = Result<bool>> + Send;
    fn put_file_sha(
        &self,
        id: &str,
        file: u32,
        sha256: &str,
        expires_at: i64,
    ) -> impl Future<Output = Result<()>> + Send;
    fn put_chunk(&self, id: &str, c: &ChunkInfo, expires_at: i64) -> impl Future<Output = Result<()>> + Send;
    fn list_chunks(&self, id: &str) -> impl Future<Output = Result<Vec<ChunkInfo>>> + Send;
}

pub struct PresignedPut {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

pub trait BlobStore: Send + Sync {
    /// サイズと SHA-256 を署名に含めた PUT URL。Object Storage 側で内容検証させ、
    /// 壊れたデータが「完了済みチャンク」として残らないようにする。
    fn presign_put(
        &self,
        key: &str,
        size: u64,
        sha256_b64: &str,
        expires_in_secs: u64,
    ) -> impl Future<Output = Result<PresignedPut>> + Send;
    fn presign_get(&self, key: &str, expires_in_secs: u64) -> impl Future<Output = Result<String>> + Send;
    /// 存在すれば (size, sha256_b64)
    fn head(&self, key: &str) -> impl Future<Output = Result<Option<(u64, Option<String>)>>> + Send;
    fn delete_prefix(&self, prefix: &str) -> impl Future<Output = Result<()>> + Send;
}

pub trait Notifier: Send + Sync {
    /// 接続が既に存在しない場合は Ok(false)（呼び出し側で接続レコードを掃除する）
    fn send(&self, connection_id: &str, event: &ServerEvent) -> impl Future<Output = Result<bool>> + Send;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Sent,
    /// push service が 404/410 を返した。購読は失効しているので削除する
    Gone,
}

/// Web Push の送信（到達手段 `web_push` の Notifier）。Payload は常に空で、内容を push service に渡さない。
pub trait Pusher: Send + Sync {
    /// VAPID 公開鍵。None なら Web Push は無効（`/api/push/config` が null を返す）
    fn vapid_public_key(&self) -> Option<String>;
    fn push(&self, subscription_url: &str) -> impl Future<Output = Result<PushOutcome>> + Send;
}

/// Web Push を使わない環境（テスト・VAPID 未設定）
pub struct NoPush;

impl Pusher for NoPush {
    fn vapid_public_key(&self) -> Option<String> {
        None
    }
    async fn push(&self, _url: &str) -> Result<PushOutcome> {
        Ok(PushOutcome::Sent)
    }
}

/// VAPID 鍵が設定されていない環境では None（Push 無効）にできるように
impl<P: Pusher> Pusher for Option<P> {
    fn vapid_public_key(&self) -> Option<String> {
        self.as_ref().and_then(Pusher::vapid_public_key)
    }
    async fn push(&self, url: &str) -> Result<PushOutcome> {
        match self {
            Some(p) => p.push(url).await,
            None => Ok(PushOutcome::Sent),
        }
    }
}
