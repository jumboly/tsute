//! つて クライアントの OS 非依存コア。
//!
//! デスクトップ UI（Tauri）や CLI はこの `Client` を操作するだけにし、
//! 認証・通知・転送・再開のロジックを OS ごとに重複させない（Windows 版でも再利用するため）。

pub mod api;
pub mod db;
pub mod profile;
pub mod secrets;
pub mod transfer;
pub mod ws;

pub use api::Api;
pub use db::{Direction, HistoryItem, LocalStatus};
pub use profile::{Profile, ProfileConfig};
pub use transfer::{Client, ClientEvent, OutgoingFile};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("db: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("api {status} {code}: {message}")]
    Api { status: u16, code: String, message: String },
    #[error("object storage {status}: {message}")]
    Blob { status: u16, message: String },
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("credential storage: {0}")]
    Secret(String),
    #[error("not enrolled")]
    NotEnrolled,
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// 再試行で回復し得るか（ネットワーク断・一時的なサーバーエラー・URL 期限切れなど）
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Http(_) | Error::Io(_) => true,
            Error::Api { status, .. } => *status >= 500 || *status == 429 || *status == 401,
            Error::Blob { status, .. } => *status >= 500 || *status == 403 || *status == 429 || *status == 400,
            Error::Protocol(_) => true,
            _ => false,
        }
    }
}
