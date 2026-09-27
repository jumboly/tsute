//! アプリ全体の状態（1 プロセス = 1 プロファイル = 1 Endpoint）

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tsute_client_core::secrets::SecretStore;
use tsute_client_core::{Client, Profile};
use tsute_os::ClipSnapshot;
use tsute_proto::EndpointInfo;

#[derive(Debug, Clone, Default)]
pub struct Args {
    pub profile: String,
    pub insecure_file_credentials: bool,
    pub show: bool,
    pub automation: bool,
    pub data_dir: Option<PathBuf>,
    /// 受信フォルダの上書き（テストで実際の ~/Downloads を使わないため）
    pub download_dir: Option<PathBuf>,
}

impl Args {
    pub fn parse() -> Self {
        let args: Vec<String> = std::env::args().collect();
        let value = |name: &str| {
            args.iter()
                .position(|a| a == name)
                .and_then(|i| args.get(i + 1).cloned())
        };
        let flag = |name: &str| args.iter().any(|a| a == name);
        Self {
            profile: value("--profile").unwrap_or_else(|| "default".into()),
            insecure_file_credentials: flag("--insecure-file-credentials"),
            show: flag("--show"),
            // 誤って有効化されないよう、フラグと環境変数の両方を要求する（ADR-0013）
            automation: flag("--automation") && std::env::var("TSUTE_AUTOMATION").as_deref() == Ok("1"),
            data_dir: value("--data-dir").map(PathBuf::from),
            download_dir: value("--download-dir").map(PathBuf::from),
        }
    }
}

pub struct AppState {
    pub args: Args,
    pub app_dir: PathBuf,
    pub secrets: Arc<dyn SecretStore>,
    pub client: Mutex<Option<Client>>,
    /// Send Clipboard で読み取り、プレビューに表示した内容。送信はこれを使い、再読み取りしない
    /// （プレビューと実際に送る内容が食い違わないようにするため）
    pub snapshot: Mutex<Option<ClipSnapshot>>,
    pub endpoints: Mutex<Vec<EndpointInfo>>,
    /// 同一プロファイルの二重起動防止ロック（保持し続ける）
    pub _lock: std::fs::File,
}

impl AppState {
    pub fn profile(&self) -> Profile {
        Profile::new(&self.app_dir, &self.args.profile).expect("profile dir")
    }
    pub fn client(&self) -> Option<Client> {
        self.client.lock().expect("lock").clone()
    }
    pub fn require_client(&self) -> Result<Client, String> {
        self.client().ok_or_else(|| "not enrolled".to_string())
    }
    pub fn endpoint_name(&self, id: &str) -> String {
        self.endpoints
            .lock()
            .expect("lock")
            .iter()
            .find(|e| e.endpoint_id == id)
            .map(|e| e.name.clone())
            .unwrap_or_else(|| id.chars().take(12).collect())
    }
}
