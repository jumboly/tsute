//! 実行環境（AWS / ローカル）ごとに差し替える境界。
//!
//! 汎用 KV ではなくドメイン操作単位で定義している。理由: 一回限りの消費や条件付き状態遷移など
//! 「原子的であるべき操作」を実装側（DynamoDB の条件付き書き込み等）に確実に落とし込むため。

use std::future::Future;

use tsute_proto::{ChunkInfo, EndpointInfo, Platform, ServerEvent, Transfer, TransferState};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, BoxError>;

#[derive(Debug, Clone, PartialEq)]
pub struct EndpointRecord {
    pub endpoint_id: String,
    pub name: String,
    pub platform: Platform,
    pub public_key: String,
    pub created_at: i64,
}

impl EndpointRecord {
    pub fn info(&self, online: bool) -> EndpointInfo {
        EndpointInfo {
            endpoint_id: self.endpoint_id.clone(),
            name: self.name.clone(),
            platform: self.platform,
            online,
            created_at: self.created_at,
        }
    }
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
