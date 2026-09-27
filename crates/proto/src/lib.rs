//! つて のワイヤープロトコル。
//!
//! クライアント（デスクトップ）とサーバー（Lambda / ローカル開発サーバー）で同じ型を
//! 共有し、片側だけ変更してプロトコルが食い違う事故をコンパイル時に防ぐ。
//! JSON のフィールド名は snake_case、列挙は `type` タグ付きで固定する。

use serde::{Deserialize, Serialize};

/// プロトコル版。互換性のない変更時に上げ、サーバーは未知の版を拒否できる。
pub const PROTOCOL_VERSION: u32 = 1;

/// Text をHTTP API に inline で載せる上限（UTF-8 バイト）。
/// API Gateway HTTP API の payload 上限 10MB / Lambda 同期 6MB / DynamoDB item 400KB に対し
/// JSON エスケープで最大 6 倍に膨らんでも十分余裕がある値として 64KiB を選ぶ（ADR-0004）。
pub const INLINE_TEXT_MAX_BYTES: usize = 64 * 1024;

/// 既定チャンクサイズ。根拠は ADR-0004。
pub const DEFAULT_CHUNK_SIZE: u64 = 8 * 1024 * 1024;
pub const MIN_CHUNK_SIZE: u64 = 256 * 1024;
pub const MAX_CHUNK_SIZE: u64 = 64 * 1024 * 1024;
/// 1 転送あたりの上限。無制限にすると誤操作で巨大な課金/ストレージ消費を招くため。
pub const MAX_TRANSFER_BYTES: u64 = 50 * 1024 * 1024 * 1024;
pub const MAX_FILES_PER_TRANSFER: usize = 1000;

/// 認証チャレンジの署名対象。ドメイン分離のため固定プレフィックスを付け、
/// 他用途の署名を流用したリプレイを防ぐ。
pub fn auth_signing_message(endpoint_id: &str, nonce: &str) -> Vec<u8> {
    format!("tsute-auth-v1\n{endpoint_id}\n{nonce}").into_bytes()
}

pub fn chunk_count(size: u64, chunk_size: u64) -> u32 {
    if size == 0 {
        // 空ファイルも「1 つの空チャンク」として扱い、転送パスを特別扱いしない
        1
    } else {
        size.div_ceil(chunk_size) as u32
    }
}

// ---------- Endpoint / 認証 ----------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Macos,
    Windows,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollRequest {
    pub enrollment_key: String,
    pub name: String,
    pub platform: Platform,
    /// Ed25519 公開鍵（base64url, no pad）
    pub public_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollResponse {
    pub endpoint_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChallengeRequest {
    pub endpoint_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChallengeResponse {
    pub nonce: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenRequest {
    pub endpoint_id: String,
    pub nonce: String,
    /// `auth_signing_message` への Ed25519 署名（base64url, no pad）
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EndpointInfo {
    pub endpoint_id: String,
    pub name: String,
    pub platform: Platform,
    pub online: bool,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeResponse {
    pub endpoint: EndpointInfo,
    pub protocol_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointList {
    pub endpoints: Vec<EndpointInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameRequest {
    pub name: String,
}

// ---------- Transfer ----------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferKind {
    ClipboardText,
    ClipboardImage,
    ClipboardVideo,
    Files,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    /// 送信側がチャンクをアップロード中（受信側は完了済みチャンクから取得開始できる）
    Uploading,
    /// 全ファイルの finalize 済み
    Uploaded,
    /// 受信側が検証まで完了し受領通知した
    Received,
    Cancelled,
}

impl TransferState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Received | Self::Cancelled)
    }
}

/// 画像・動画の補助情報。取得できたものだけ埋める。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MediaInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewFile {
    /// パス区切りを含まないファイル名（サーバーでも検証する）
    pub name: String,
    pub size: u64,
    pub mime: String,
    #[serde(default)]
    pub media: MediaInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTransferRequest {
    pub receiver: String,
    pub kind: TransferKind,
    /// ClipboardText で INLINE_TEXT_MAX_BYTES 以下のときだけ使う
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default)]
    pub files: Vec<NewFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileEntry {
    pub index: u32,
    pub name: String,
    pub size: u64,
    pub mime: String,
    #[serde(default)]
    pub media: MediaInfo,
    pub chunk_count: u32,
    /// 送信側が finalize 時に報告するファイル全体の SHA-256（base64）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Transfer {
    pub transfer_id: String,
    pub sender: String,
    pub receiver: String,
    pub kind: TransferKind,
    pub state: TransferState,
    pub created_at: i64,
    pub expires_at: i64,
    pub chunk_size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub files: Vec<FileEntry>,
}

impl Transfer {
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
    pub fn total_chunks(&self) -> u64 {
        self.files.iter().map(|f| f.chunk_count as u64).sum()
    }
    pub fn chunk_len(&self, file: u32, index: u32) -> u64 {
        let f = &self.files[file as usize];
        let start = index as u64 * self.chunk_size;
        f.size.saturating_sub(start).min(self.chunk_size)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChunkRef {
    pub file: u32,
    pub index: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChunkInfo {
    pub file: u32,
    pub index: u32,
    pub size: u64,
    /// チャンクの SHA-256（base64 標準, S3 の x-amz-checksum-sha256 と同形式）
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferDetail {
    pub transfer: Transfer,
    /// アップロード完了済みチャンク
    pub chunks: Vec<ChunkInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferList {
    pub transfers: Vec<Transfer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadUrlRequest {
    pub chunks: Vec<ChunkInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadUrlRequest {
    pub chunks: Vec<ChunkRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresignedUrl {
    pub file: u32,
    pub index: u32,
    pub url: String,
    /// PUT 時にそのまま付与すべきヘッダ（署名対象に含まれる）
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresignedUrls {
    pub urls: Vec<PresignedUrl>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkCompleteRequest {
    /// サーバーは Object Storage 上の実体（サイズ・checksum）と照合してから完了扱いにする
    pub chunks: Vec<ChunkInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalizeFileRequest {
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
    pub message: String,
}

// ---------- WebSocket ----------

/// サーバー → クライアントの通知。通知は「ヒント」であり、取りこぼしても
/// クライアントは HTTP で再同期できる設計（ADR-0007）。32KB フレーム上限に収まる小ささを保つ。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    Hello {
        endpoint_id: String,
        connection_id: String,
    },
    Presence {
        endpoint_id: String,
        online: bool,
    },
    EndpointsChanged,
    TransferCreated {
        transfer: Box<Transfer>,
    },
    ChunksReady {
        transfer_id: String,
        chunks: Vec<ChunkInfo>,
    },
    TransferState {
        transfer_id: String,
        state: TransferState,
    },
    Pong,
}

/// クライアント → サーバー。API Gateway の route selection expression `$request.body.action` に合わせる。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ClientMessage {
    Ping,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_math() {
        assert_eq!(chunk_count(0, 8), 1);
        assert_eq!(chunk_count(8, 8), 1);
        assert_eq!(chunk_count(9, 8), 2);
        let t = Transfer {
            transfer_id: "t".into(),
            sender: "a".into(),
            receiver: "b".into(),
            kind: TransferKind::Files,
            state: TransferState::Uploading,
            created_at: 0,
            expires_at: 0,
            chunk_size: 8,
            text: None,
            files: vec![FileEntry {
                index: 0,
                name: "x".into(),
                size: 20,
                mime: "a/b".into(),
                media: Default::default(),
                chunk_count: 3,
                sha256: None,
            }],
        };
        assert_eq!(t.chunk_len(0, 0), 8);
        assert_eq!(t.chunk_len(0, 2), 4);
    }

    #[test]
    fn event_json_shape() {
        let e = ServerEvent::Presence {
            endpoint_id: "e".into(),
            online: true,
        };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"type":"presence","endpoint_id":"e","online":true}"#
        );
        assert_eq!(
            serde_json::to_string(&ClientMessage::Ping).unwrap(),
            r#"{"action":"ping"}"#
        );
    }
}
