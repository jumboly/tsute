//! Endpoint 秘密鍵の保管。
//!
//! 本番は OS の Credential Storage（macOS: Keychain）。`FileSecretStore` は
//! 自動テストで Keychain のアクセス許可ダイアログが出て止まるのを避けるための開発用で、
//! 明示フラグなしには使わない（ADR-0011）。

use std::path::PathBuf;

use crate::Error;

pub trait SecretStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<Vec<u8>>, Error>;
    fn set(&self, account: &str, secret: &[u8]) -> Result<(), Error>;
    fn delete(&self, account: &str) -> Result<(), Error>;
    fn describe(&self) -> String;
}

pub struct FileSecretStore {
    pub dir: PathBuf,
}

impl FileSecretStore {
    /// アカウント名は URL を含むためファイル名に使えない文字がある。ハッシュでファイル名化する
    fn path(&self, account: &str) -> PathBuf {
        use sha2::Digest;
        let h = sha2::Sha256::digest(account.as_bytes());
        let name: String = h.iter().take(16).map(|b| format!("{b:02x}")).collect();
        self.dir.join(format!("{name}.secret"))
    }
}

impl SecretStore for FileSecretStore {
    fn get(&self, account: &str) -> Result<Option<Vec<u8>>, Error> {
        match std::fs::read(self.path(account)) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn set(&self, account: &str, secret: &[u8]) -> Result<(), Error> {
        use std::io::Write;
        std::fs::create_dir_all(&self.dir)?;
        let p = self.path(account);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // 他ユーザーから読めないように作成時点で 0600 にする
            opts.mode(0o600);
        }
        opts.open(p)?.write_all(secret)?;
        Ok(())
    }
    fn delete(&self, account: &str) -> Result<(), Error> {
        let _ = std::fs::remove_file(self.path(account));
        Ok(())
    }
    fn describe(&self) -> String {
        format!("file ({}) [INSECURE: development only]", self.dir.display())
    }
}

#[cfg(target_os = "macos")]
pub struct KeychainSecretStore {
    pub service: String,
}

#[cfg(target_os = "macos")]
impl SecretStore for KeychainSecretStore {
    fn get(&self, account: &str) -> Result<Option<Vec<u8>>, Error> {
        use security_framework::passwords::get_generic_password;
        match get_generic_password(&self.service, account) {
            Ok(v) => Ok(Some(v)),
            // errSecItemNotFound
            Err(e) if e.code() == -25300 => Ok(None),
            Err(e) => Err(Error::Secret(format!("keychain read: {e}"))),
        }
    }
    fn set(&self, account: &str, secret: &[u8]) -> Result<(), Error> {
        security_framework::passwords::set_generic_password(&self.service, account, secret)
            .map_err(|e| Error::Secret(format!("keychain write: {e}")))
    }
    fn delete(&self, account: &str) -> Result<(), Error> {
        match security_framework::passwords::delete_generic_password(&self.service, account) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == -25300 => Ok(()),
            Err(e) => Err(Error::Secret(format!("keychain delete: {e}"))),
        }
    }
    fn describe(&self) -> String {
        format!("macOS Keychain (service {})", self.service)
    }
}
