//! control-plane HTTP API クライアント。アクセストークンの取得・更新を内包する。

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signer, SigningKey};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;
use tsute_proto::*;

use crate::Error;

#[derive(Clone)]
pub struct Api {
    pub base_url: String,
    pub http: reqwest::Client,
    pub endpoint_id: String,
    key: Arc<SigningKey>,
    token: Arc<Mutex<Option<(String, i64)>>>,
}

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        // 8MiB チャンクを低速回線で送る場合も考え全体タイムアウトは長めにし、無通信だけを検出する
        .read_timeout(Duration::from_secs(60))
        .user_agent(concat!("tsute/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("http client")
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
}

async fn read_json<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T, Error> {
    let status = resp.status();
    let body = resp.bytes().await?;
    if status.is_success() {
        return serde_json::from_slice(&body).map_err(|e| Error::Protocol(format!("decode: {e}")));
    }
    let err: Option<ApiError> = serde_json::from_slice(&body).ok();
    Err(Error::Api {
        status: status.as_u16(),
        code: err.as_ref().map(|e| e.error.clone()).unwrap_or_default(),
        message: err
            .map(|e| e.message)
            .unwrap_or_else(|| String::from_utf8_lossy(&body).chars().take(200).collect()),
    })
}

/// 登録（Enrollment）。成功したら Endpoint ID を返す。秘密鍵は呼び出し側で保存する。
pub async fn enroll(base_url: &str, key: &SigningKey, enrollment_key: &str, name: &str) -> Result<String, Error> {
    let req = EnrollRequest {
        enrollment_key: enrollment_key.trim().to_string(),
        name: name.to_string(),
        platform: if cfg!(target_os = "macos") {
            Platform::Macos
        } else if cfg!(windows) {
            Platform::Windows
        } else {
            Platform::Other
        },
        public_key: URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes()),
        client_kind: ClientKind::Native,
        // Native は全種類を受信できる。明示しておくと、将来 Native 側で種類を増減したときにも申告で表せる
        accepts: Some(NATIVE_ACCEPTS.to_vec()),
    };
    let resp = http_client()
        .post(format!("{base_url}/api/enroll"))
        .json(&req)
        .send()
        .await?;
    Ok(read_json::<EnrollResponse>(resp).await?.endpoint_id)
}

impl Api {
    pub fn new(base_url: String, endpoint_id: String, key: SigningKey) -> Self {
        Self {
            base_url,
            http: http_client(),
            endpoint_id,
            key: Arc::new(key),
            token: Arc::new(Mutex::new(None)),
        }
    }

    /// 有効なアクセストークン（期限の 2 分前には更新する）
    pub async fn token(&self) -> Result<String, Error> {
        let mut guard = self.token.lock().await;
        if let Some((t, exp)) = guard.as_ref()
            && *exp - 120 > now()
        {
            return Ok(t.clone());
        }
        let ch: ChallengeResponse = read_json(
            self.http
                .post(format!("{}/api/auth/challenge", self.base_url))
                .json(&ChallengeRequest {
                    endpoint_id: self.endpoint_id.clone(),
                })
                .send()
                .await?,
        )
        .await?;
        let sig = self.key.sign(&auth_signing_message(&self.endpoint_id, &ch.nonce));
        let tr: TokenResponse = read_json(
            self.http
                .post(format!("{}/api/auth/token", self.base_url))
                .json(&TokenRequest {
                    endpoint_id: self.endpoint_id.clone(),
                    nonce: ch.nonce,
                    signature: URL_SAFE_NO_PAD.encode(sig.to_bytes()),
                })
                .send()
                .await?,
        )
        .await?;
        *guard = Some((tr.access_token.clone(), tr.expires_at));
        Ok(tr.access_token)
    }

    async fn invalidate(&self) {
        *self.token.lock().await = None;
    }

    async fn call<B: Serialize, T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<T, Error> {
        // 401 はトークン失効（サーバー側の期限・revoke）の可能性があるので 1 回だけ取り直す
        for attempt in 0..2 {
            let tok = self.token().await?;
            match read_json(self.request(method.clone(), path, &tok, body).send().await?).await {
                Err(Error::Api { status: 401, .. }) if attempt == 0 => self.invalidate().await,
                r => return r,
            }
        }
        unreachable!()
    }

    fn request<B: Serialize>(
        &self,
        method: reqwest::Method,
        path: &str,
        token: &str,
        body: Option<&B>,
    ) -> reqwest::RequestBuilder {
        let may_have_body = method != reqwest::Method::GET && method != reqwest::Method::HEAD;
        let rb = self
            .http
            .request(method, format!("{}{path}", self.base_url))
            .bearer_auth(token);
        match body {
            Some(b) => rb.json(b),
            // 本文なしの POST 等は Content-Length が付かず、411 で拒否するプロキシがあるため明示する
            None if may_have_body => rb.header(reqwest::header::CONTENT_LENGTH, 0),
            None => rb,
        }
    }

    pub async fn me(&self) -> Result<MeResponse, Error> {
        self.call::<(), _>(reqwest::Method::GET, "/api/me", None).await
    }
    pub async fn rename(&self, name: &str) -> Result<MeResponse, Error> {
        self.call(
            reqwest::Method::PUT,
            "/api/me/name",
            Some(&RenameRequest { name: name.into() }),
        )
        .await
    }
    pub async fn endpoints(&self) -> Result<Vec<EndpointInfo>, Error> {
        Ok(self
            .call::<(), EndpointList>(reqwest::Method::GET, "/api/endpoints", None)
            .await?
            .endpoints)
    }
    pub async fn create_transfer(&self, r: &CreateTransferRequest) -> Result<Transfer, Error> {
        self.call(reqwest::Method::POST, "/api/transfers", Some(r)).await
    }
    pub async fn transfers(&self) -> Result<Vec<Transfer>, Error> {
        Ok(self
            .call::<(), TransferList>(reqwest::Method::GET, "/api/transfers", None)
            .await?
            .transfers)
    }
    pub async fn transfer(&self, id: &str) -> Result<TransferDetail, Error> {
        self.call::<(), _>(reqwest::Method::GET, &format!("/api/transfers/{id}"), None)
            .await
    }
    pub async fn cancel(&self, id: &str) -> Result<serde_json::Value, Error> {
        self.call::<(), _>(reqwest::Method::DELETE, &format!("/api/transfers/{id}"), None)
            .await
    }
    pub async fn upload_urls(&self, id: &str, chunks: Vec<ChunkInfo>) -> Result<Vec<PresignedUrl>, Error> {
        let r: PresignedUrls = self
            .call(
                reqwest::Method::POST,
                &format!("/api/transfers/{id}/upload-urls"),
                Some(&UploadUrlRequest { chunks }),
            )
            .await?;
        Ok(r.urls)
    }
    pub async fn chunks_complete(&self, id: &str, chunks: Vec<ChunkInfo>) -> Result<(), Error> {
        let _: serde_json::Value = self
            .call(
                reqwest::Method::POST,
                &format!("/api/transfers/{id}/chunks"),
                Some(&ChunkCompleteRequest { chunks }),
            )
            .await?;
        Ok(())
    }
    pub async fn finalize(&self, id: &str, file: u32, sha256: &str) -> Result<Transfer, Error> {
        self.call(
            reqwest::Method::POST,
            &format!("/api/transfers/{id}/files/{file}/finalize"),
            Some(&FinalizeFileRequest { sha256: sha256.into() }),
        )
        .await
    }
    pub async fn download_urls(&self, id: &str, chunks: Vec<ChunkRef>) -> Result<Vec<PresignedUrl>, Error> {
        let r: PresignedUrls = self
            .call(
                reqwest::Method::POST,
                &format!("/api/transfers/{id}/download-urls"),
                Some(&DownloadUrlRequest { chunks }),
            )
            .await?;
        Ok(r.urls)
    }
    pub async fn received(&self, id: &str) -> Result<(), Error> {
        let _: serde_json::Value = self
            .call::<(), _>(reqwest::Method::POST, &format!("/api/transfers/{id}/received"), None)
            .await?;
        Ok(())
    }

    /// presigned URL への PUT（Object Storage 直、API を経由しない）
    pub async fn put_blob(&self, u: &PresignedUrl, body: Vec<u8>) -> Result<(), Error> {
        // 空の本文だと Content-Length が付かず、S3 の署名（content-length: 0 を含む）と一致しなくなるため明示する
        let len = body.len();
        let mut rb = self
            .http
            .put(&u.url)
            .header(reqwest::header::CONTENT_LENGTH, len)
            .body(body);
        for (k, v) in &u.headers {
            rb = rb.header(k, v);
        }
        let resp = rb.send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            // 本文には署名付き URL が含まれ得ないが、長さを制限してログ汚染を避ける
            let text: String = resp.text().await.unwrap_or_default().chars().take(200).collect();
            return Err(Error::Blob { status, message: text });
        }
        Ok(())
    }

    pub async fn get_blob(&self, url: &str) -> Result<Vec<u8>, Error> {
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Blob {
                status: resp.status().as_u16(),
                message: String::new(),
            });
        }
        Ok(resp.bytes().await?.to_vec())
    }

    pub fn ws_url(&self) -> String {
        // http(s) → ws(s)。APP_BASE_URL 1 つから全 URL を導出し、設定項目を増やさない
        let b = &self.base_url;
        if let Some(rest) = b.strip_prefix("https://") {
            format!("wss://{rest}/ws")
        } else if let Some(rest) = b.strip_prefix("http://") {
            format!("ws://{rest}/ws")
        } else {
            format!("{b}/ws")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 1 回だけ要求を受け、ヘッダー部分（小文字化）を返して 200 `{}` を答えるサーバー
    async fn capture(method: reqwest::Method, body: Option<&serde_json::Value>) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                let mut b = [0u8; 1];
                s.read_exact(&mut b).await.unwrap();
                head.push(b[0]);
            }
            let head = String::from_utf8(head).unwrap().to_ascii_lowercase();
            // 本文を読み残して閉じると RST になるので読み切ってから答える
            let len = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length: "))
                .map_or(0, |v| v.trim().parse::<usize>().unwrap());
            s.read_exact(&mut vec![0; len]).await.unwrap();
            s.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}")
                .await
                .unwrap();
            head
        });
        let api = Api::new(base, "ep_test".into(), SigningKey::from_bytes(&[7; 32]));
        api.request(method, "/api/x", "tok", body).send().await.unwrap();
        server.await.unwrap()
    }

    #[tokio::test]
    async fn bodyless_requests_send_zero_content_length() {
        for m in [reqwest::Method::POST, reqwest::Method::DELETE] {
            let head = capture(m, None).await;
            assert!(head.contains("\r\ncontent-length: 0\r\n"), "{head}");
        }
        // 本文ありは reqwest が実際の長さを付ける（二重に付かない）
        let head = capture(reqwest::Method::POST, Some(&serde_json::json!({"a": 1}))).await;
        assert_eq!(head.matches("content-length:").count(), 1, "{head}");
        assert!(head.contains("\r\ncontent-length: 7\r\n"), "{head}");
    }
}
