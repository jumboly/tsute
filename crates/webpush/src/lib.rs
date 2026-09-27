//! Web Push の送信。
//!
//! 通知は「新しい受信がある」というヒントだけで、本文はユーザーが App を開いてから API で取得する（ADR-0015）。
//! そのため Payload を載せず、RFC 8291 の暗号化も行わない（push service・端末の通知履歴に内容が残らない）。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use tsute_server_core::traits::{PushOutcome, Pusher, Result};

/// push service がメッセージを保持する秒数。受信の回収は API（起動時）で行うため、古い通知を後から出す意味は薄い
const PUSH_TTL_SECS: u32 = 24 * 3600;
/// JWT の有効期限。Apple は 1 日より先を拒否し、再生成は 1 時間に 1 回までを求めるため、12 時間で作り 1 時間使い回す
const JWT_TTL_SECS: i64 = 12 * 3600;
const JWT_REUSE_SECS: i64 = 3600;

pub struct VapidPusher {
    key: SigningKey,
    public_key_b64: String,
    /// JWT の sub。Apple は https URL か mailto: 以外を 403 BadJwtToken で拒否する
    subject: String,
    http: reqwest::Client,
    jwt_cache: Mutex<HashMap<String, (String, i64)>>,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
}

/// 新しい VAPID 秘密鍵（P-256 スカラー 32 バイト, base64url no pad）
pub fn generate_private_key() -> String {
    let key = SigningKey::random(&mut rand::rngs::OsRng);
    URL_SAFE_NO_PAD.encode(key.to_bytes())
}

impl VapidPusher {
    pub fn new(private_key_b64: &str, subject: &str) -> Result<Self> {
        let raw = URL_SAFE_NO_PAD.decode(private_key_b64.trim())?;
        let key = SigningKey::from_slice(&raw)?;
        let public_key_b64 = URL_SAFE_NO_PAD.encode(key.verifying_key().to_encoded_point(false).as_bytes());
        if !(subject.starts_with("https://") || subject.starts_with("mailto:")) {
            return Err("VAPID subject must be an https: URL or mailto:".into());
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            // リダイレクト先へ送ると許可リスト（SSRF 対策）を迂回できてしまう
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            key,
            public_key_b64,
            subject: subject.to_string(),
            http,
            jwt_cache: Mutex::new(HashMap::new()),
        })
    }

    fn jwt(&self, aud: &str) -> String {
        let t = now();
        let mut cache = self.jwt_cache.lock().expect("lock");
        if let Some((jwt, created)) = cache.get(aud)
            && t - created < JWT_REUSE_SECS
        {
            return jwt.clone();
        }
        let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = URL_SAFE_NO_PAD
            .encode(serde_json::json!({"aud": aud, "exp": t + JWT_TTL_SECS, "sub": self.subject}).to_string());
        let input = format!("{header}.{claims}");
        // JWS の ES256 は DER ではなく r||s の 64 バイト
        let sig: Signature = self.key.sign(input.as_bytes());
        let jwt = format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()));
        cache.insert(aud.to_string(), (jwt.clone(), t));
        jwt
    }
}

impl Pusher for VapidPusher {
    fn vapid_public_key(&self) -> Option<String> {
        Some(self.public_key_b64.clone())
    }

    async fn push(&self, subscription_url: &str) -> Result<PushOutcome> {
        let u = url::Url::parse(subscription_url)?;
        let aud = u.origin().ascii_serialization();
        let resp = self
            .http
            .post(u)
            .header("TTL", PUSH_TTL_SECS.to_string())
            .header("Urgency", "high")
            .header(
                "Authorization",
                format!("vapid t={}, k={}", self.jwt(&aud), self.public_key_b64),
            )
            .header("Content-Length", "0")
            .send()
            .await?;
        let status = resp.status().as_u16();
        match status {
            200..=299 => Ok(PushOutcome::Sent),
            404 | 410 => Ok(PushOutcome::Gone),
            // 購読 URL 自体が宛先を特定する秘密に近いので、エラーにはステータスだけ含める
            _ => Err(format!("push service returned {status}").into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::VerifyingKey;
    use p256::ecdsa::signature::Verifier;

    #[test]
    fn jwt_is_verifiable_es256() {
        let k = generate_private_key();
        let p = VapidPusher::new(&k, "https://example.test").unwrap();
        let jwt = p.jwt("https://web.push.apple.com");
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let claims: serde_json::Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://web.push.apple.com");
        assert_eq!(claims["sub"], "https://example.test");
        assert!(claims["exp"].as_i64().unwrap() - now() <= 24 * 3600);
        let pk = URL_SAFE_NO_PAD.decode(p.vapid_public_key().unwrap()).unwrap();
        assert_eq!(pk.len(), 65, "uncompressed P-256 point");
        let vk = VerifyingKey::from_sec1_bytes(&pk).unwrap();
        let sig = Signature::from_slice(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).unwrap();
        vk.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
            .unwrap();
        // 1 時間以内は同じ JWT を使い回す（Apple の要件）
        assert_eq!(p.jwt("https://web.push.apple.com"), jwt);
    }

    #[test]
    fn rejects_bad_subject() {
        assert!(VapidPusher::new(&generate_private_key(), "localhost").is_err());
    }
}
