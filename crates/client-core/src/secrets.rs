//! Endpoint 秘密鍵の保管。
//!
//! 本番は OS の Credential Storage（macOS: Keychain / Windows: Credential Manager）。`FileSecretStore` は
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

/// Windows の資格情報マネージャー（汎用資格情報）。中身は OS が DPAPI でユーザーごとに暗号化して保存する。
/// `CRED_PERSIST_LOCAL_MACHINE` にして移動プロファイル（ドメインのローミング）で他の PC へ渡らないようにする。
/// Endpoint 鍵がマシン外へ複製されると、同じ Endpoint が 2 台で動いてしまうため（ADR-0011）
#[cfg(windows)]
pub struct WindowsCredentialStore {
    pub service: String,
}

#[cfg(windows)]
impl WindowsCredentialStore {
    fn target(&self, account: &str) -> Vec<u16> {
        format!("{}|{account}", self.service)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    }
}

#[cfg(windows)]
impl SecretStore for WindowsCredentialStore {
    fn get(&self, account: &str) -> Result<Option<Vec<u8>>, Error> {
        use windows::Win32::Foundation::ERROR_NOT_FOUND;
        use windows::Win32::Security::Credentials::{CRED_TYPE_GENERIC, CREDENTIALW, CredFree, CredReadW};
        use windows::core::{HRESULT, PCWSTR};
        let target = self.target(account);
        let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
        match unsafe { CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None, &mut cred) } {
            Ok(()) => {
                let v = unsafe {
                    let c = &*cred;
                    let v = std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec();
                    CredFree(cred as *const _);
                    v
                };
                Ok(Some(v))
            }
            Err(e) if e.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0) => Ok(None),
            Err(e) => Err(Error::Secret(format!("credential read: {e}"))),
        }
    }
    fn set(&self, account: &str, secret: &[u8]) -> Result<(), Error> {
        use windows::Win32::Security::Credentials::{
            CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredWriteW,
        };
        use windows::core::PWSTR;
        let mut target = self.target(account);
        let mut user: Vec<u16> = account.encode_utf16().chain(std::iter::once(0)).collect();
        let mut blob = secret.to_vec();
        let cred = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(target.as_mut_ptr()),
            UserName: PWSTR(user.as_mut_ptr()),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            ..Default::default()
        };
        let r = unsafe { CredWriteW(&cred, 0) };
        zeroize::Zeroize::zeroize(&mut blob);
        r.map_err(|e| Error::Secret(format!("credential write: {e}")))
    }
    fn delete(&self, account: &str) -> Result<(), Error> {
        use windows::Win32::Foundation::ERROR_NOT_FOUND;
        use windows::Win32::Security::Credentials::{CRED_TYPE_GENERIC, CredDeleteW};
        use windows::core::{HRESULT, PCWSTR};
        let target = self.target(account);
        match unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None) } {
            Ok(()) => Ok(()),
            Err(e) if e.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0) => Ok(()),
            Err(e) => Err(Error::Secret(format!("credential delete: {e}"))),
        }
    }
    fn describe(&self) -> String {
        format!("Windows Credential Manager ({})", self.service)
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn credential_manager_roundtrip() {
        // 実際の資格情報マネージャーに書くので、テスト専用のサービス名を使い最後に消す
        let s = WindowsCredentialStore {
            service: format!("dev.tsute.test-{}", std::process::id()),
        };
        let acct = "test|https://example.invalid|ep1";
        assert_eq!(s.get(acct).unwrap(), None);
        s.set(acct, b"secret-bytes").unwrap();
        assert_eq!(s.get(acct).unwrap().as_deref(), Some(&b"secret-bytes"[..]));
        s.set(acct, b"replaced").unwrap();
        assert_eq!(s.get(acct).unwrap().as_deref(), Some(&b"replaced"[..]));
        s.delete(acct).unwrap();
        assert_eq!(s.get(acct).unwrap(), None);
        s.delete(acct).unwrap();
    }
}
