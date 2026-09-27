//! アプリプロファイル = Endpoint。
//!
//! 物理端末ではなく「プロファイル」を識別単位にするため、プロファイルごとに
//! 設定・秘密鍵・ローカル DB・受信ディレクトリを完全に分離する（同一マシンで複数同時起動できるように）。

use std::path::{Path, PathBuf};

use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::Error;
use crate::secrets::SecretStore;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileConfig {
    pub base_url: String,
    pub endpoint_id: String,
    pub name: String,
    /// 受信ファイルの保存先（未設定なら既定のダウンロードフォルダ配下）
    #[serde(default)]
    pub download_dir: Option<PathBuf>,
}

pub struct Profile {
    pub name: String,
    pub root: PathBuf,
}

pub fn valid_profile_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 32 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl Profile {
    pub fn new(app_data_dir: &Path, name: &str) -> Result<Self, Error> {
        if !valid_profile_name(name) {
            return Err(Error::Other(format!("invalid profile name {name:?} (use [A-Za-z0-9_-], max 32)")));
        }
        let root = app_data_dir.join("profiles").join(name);
        std::fs::create_dir_all(&root)?;
        Ok(Self { name: name.into(), root })
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join("profile.json")
    }
    pub fn db_path(&self) -> PathBuf {
        self.root.join("state.sqlite3")
    }
    /// 受信したクリップボード項目（画像・動画・大きいテキスト）の置き場
    pub fn received_dir(&self) -> PathBuf {
        self.root.join("received")
    }
    /// 送信のためにクリップボードから書き出した一時ファイルの置き場
    pub fn outbox_dir(&self) -> PathBuf {
        self.root.join("outbox")
    }

    pub fn load_config(&self) -> Result<Option<ProfileConfig>, Error> {
        match std::fs::read(self.config_path()) {
            Ok(b) => Ok(Some(serde_json::from_slice(&b).map_err(|e| Error::Other(format!("profile.json: {e}")))?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save_config(&self, c: &ProfileConfig) -> Result<(), Error> {
        // 途中で落ちても壊れた JSON が残らないよう一時ファイル経由で置き換える
        let tmp = self.root.join("profile.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(c).expect("json"))?;
        std::fs::rename(tmp, self.config_path())?;
        Ok(())
    }

    /// Keychain 等のアカウント名。プロファイル名と接続先の両方で分け、
    /// 同名プロファイルでも環境（本番/テスト）が違えば衝突しないようにする。
    pub fn secret_account(&self, c: &ProfileConfig) -> String {
        format!("{}|{}|{}", self.name, c.base_url, c.endpoint_id)
    }

    pub fn load_key(&self, secrets: &dyn SecretStore, c: &ProfileConfig) -> Result<SigningKey, Error> {
        let mut raw = secrets.get(&self.secret_account(c))?.ok_or(Error::NotEnrolled)?;
        let arr: [u8; 32] = raw.as_slice().try_into().map_err(|_| Error::Secret("stored key has wrong length".into()))?;
        raw.zeroize();
        Ok(SigningKey::from_bytes(&arr))
    }

    pub fn default_download_dir(&self) -> PathBuf {
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from);
        let base = home.map(|h| h.join("Downloads")).unwrap_or_else(|| self.root.join("downloads"));
        // 既定プロファイル以外はサブフォルダを分け、同一マシンでのテスト時に受信物が混ざらないようにする
        if self.name == "default" { base.join("Tsute") } else { base.join(format!("Tsute-{}", self.name)) }
    }

    /// 新規登録。鍵を生成してサーバーに公開鍵を登録し、秘密鍵を Credential Storage に保存する。
    pub async fn enroll(
        &self,
        secrets: &dyn SecretStore,
        base_url: &str,
        enrollment_key: &str,
        endpoint_name: &str,
    ) -> Result<ProfileConfig, Error> {
        let base_url = base_url.trim().trim_end_matches('/').to_string();
        let key = SigningKey::generate(&mut rand::rngs::OsRng);
        let endpoint_id = crate::api::enroll(&base_url, &key, enrollment_key, endpoint_name).await?;
        let cfg = ProfileConfig { base_url, endpoint_id, name: endpoint_name.into(), download_dir: None };
        secrets.set(&self.secret_account(&cfg), key.as_bytes())?;
        self.save_config(&cfg)?;
        Ok(cfg)
    }

    /// ローカルの登録情報を削除（サーバー側の Endpoint 失効は管理者操作で行う）
    pub fn forget(&self, secrets: &dyn SecretStore) -> Result<(), Error> {
        if let Some(c) = self.load_config()? {
            secrets.delete(&self.secret_account(&c))?;
        }
        let _ = std::fs::remove_file(self.config_path());
        Ok(())
    }
}
