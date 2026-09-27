//! server-core の入力検証・権限・状態遷移のテスト（メモリ実装で HTTP ハンドラを直接呼ぶ）

use std::collections::HashMap;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signer, SigningKey};
use tsute_proto::*;
use tsute_server_core::memory::{ChannelNotifier, MemoryStore};
use tsute_server_core::traits::{BlobStore, PresignedPut, Result as CoreResult};
use tsute_server_core::{Config, Core, Request};

/// head() が常に「未アップロード」を返す Blob（完了報告の照合を検証するため）
struct NoBlob;
impl BlobStore for NoBlob {
    async fn presign_put(&self, key: &str, _s: u64, _h: &str, _t: u64) -> CoreResult<PresignedPut> {
        Ok(PresignedPut {
            url: format!("https://blob/{key}"),
            headers: vec![],
        })
    }
    async fn presign_get(&self, key: &str, _t: u64) -> CoreResult<String> {
        Ok(format!("https://blob/{key}"))
    }
    async fn head(&self, _k: &str) -> CoreResult<Option<(u64, Option<String>)>> {
        Ok(None)
    }
    async fn delete_prefix(&self, _p: &str) -> CoreResult<()> {
        Ok(())
    }
}

type C = Core<MemoryStore, NoBlob, ChannelNotifier>;

fn core() -> C {
    Core::new(
        MemoryStore::default(),
        NoBlob,
        ChannelNotifier::default(),
        Config::default(),
    )
}

async fn req(
    c: &C,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    let mut headers = HashMap::new();
    if let Some(t) = token {
        headers.insert("authorization".into(), format!("Bearer {t}"));
    }
    let r = c
        .handle_http(Request {
            method: method.into(),
            path: path.into(),
            headers,
            body: serde_json::to_vec(&body).unwrap(),
        })
        .await;
    (
        r.status,
        serde_json::from_slice(&r.body).unwrap_or(serde_json::Value::Null),
    )
}

/// 登録してトークンを得る
async fn endpoint(c: &C, name: &str) -> (String, String) {
    let key = SigningKey::from_bytes(&rand_bytes());
    let (ek, _) = c.issue_enrollment_key().await.unwrap();
    let (s, v) = req(
        c,
        "POST",
        "/api/enroll",
        None,
        serde_json::json!({"enrollment_key": ek, "name": name, "platform": "macos",
            "public_key": URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes())}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let id = v["endpoint_id"].as_str().unwrap().to_string();
    let (_, ch) = req(
        c,
        "POST",
        "/api/auth/challenge",
        None,
        serde_json::json!({"endpoint_id": id}),
    )
    .await;
    let nonce = ch["nonce"].as_str().unwrap();
    let sig = key.sign(&auth_signing_message(&id, nonce));
    let (s, t) = req(
        c,
        "POST",
        "/api/auth/token",
        None,
        serde_json::json!({"endpoint_id": id, "nonce": nonce, "signature": URL_SAFE_NO_PAD.encode(sig.to_bytes())}),
    )
    .await;
    assert_eq!(s, 200);
    // 同じ nonce の再利用（リプレイ）は拒否される
    let (s2, _) = req(
        c,
        "POST",
        "/api/auth/token",
        None,
        serde_json::json!({"endpoint_id": id, "nonce": nonce, "signature": URL_SAFE_NO_PAD.encode(sig.to_bytes())}),
    )
    .await;
    assert_eq!(s2, 401, "nonce replay must fail");
    (id, t["access_token"].as_str().unwrap().to_string())
}

fn rand_bytes() -> [u8; 32] {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b
}

#[tokio::test]
async fn auth_required_and_wrong_signature_rejected() {
    let c = core();
    let (s, _) = req(&c, "GET", "/api/endpoints", None, serde_json::Value::Null).await;
    assert_eq!(s, 401);
    let (s, _) = req(
        &c,
        "GET",
        "/api/endpoints",
        Some("tsute-at-bogus"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(s, 401);
    let (id, _) = endpoint(&c, "A").await;
    let (_, ch) = req(
        &c,
        "POST",
        "/api/auth/challenge",
        None,
        serde_json::json!({"endpoint_id": id}),
    )
    .await;
    let other = SigningKey::from_bytes(&[7u8; 32]);
    let sig = other.sign(&auth_signing_message(&id, ch["nonce"].as_str().unwrap()));
    let (s, _) = req(
        &c,
        "POST",
        "/api/auth/token",
        None,
        serde_json::json!({"endpoint_id": id, "nonce": ch["nonce"], "signature": URL_SAFE_NO_PAD.encode(sig.to_bytes())}),
    )
    .await;
    assert_eq!(s, 401, "signature by another key must fail");
}

#[tokio::test]
async fn transfer_validation() {
    let c = core();
    let (a, ta) = endpoint(&c, "A").await;
    let (b, _tb) = endpoint(&c, "B").await;
    let mk = |files: serde_json::Value| serde_json::json!({"receiver": b, "kind": "files", "files": files});
    for bad in ["../x", "a/b", "", ".", "a\\b", "x\u{0}y"] {
        let (s, _) = req(
            &c,
            "POST",
            "/api/transfers",
            Some(&ta),
            mk(serde_json::json!([{"name": bad, "size": 1, "mime": "a/b"}])),
        )
        .await;
        assert_eq!(s, 400, "file name {bad:?} must be rejected");
    }
    let (s, _) = req(
        &c,
        "POST",
        "/api/transfers",
        Some(&ta),
        serde_json::json!({"receiver": a, "kind": "files", "files": [{"name": "x", "size": 1, "mime": "a"}]}),
    )
    .await;
    assert_eq!(s, 400, "send to self rejected");
    let (s, _) = req(
        &c,
        "POST",
        "/api/transfers",
        Some(&ta),
        serde_json::json!({"receiver": b, "kind": "clipboard_text", "text": "x".repeat(INLINE_TEXT_MAX_BYTES + 1)}),
    )
    .await;
    assert_eq!(s, 400, "inline text over limit rejected");
    let (s, _) = req(
        &c,
        "POST",
        "/api/transfers",
        Some(&ta),
        mk(serde_json::json!([{"name": "x", "size": MAX_TRANSFER_BYTES + 1, "mime": "a"}])),
    )
    .await;
    assert_eq!(s, 400, "oversized transfer rejected");
    let (s, t) = req(
        &c,
        "POST",
        "/api/transfers",
        Some(&ta),
        mk(serde_json::json!([{"name": "ok.bin", "size": 20_000_000, "mime": "a"}])),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(t["files"][0]["chunk_count"], 3);
    let id = t["transfer_id"].as_str().unwrap();
    // チャンクサイズの詐称は拒否
    let (s, _) = req(
        &c,
        "POST",
        &format!("/api/transfers/{id}/upload-urls"),
        Some(&ta),
        serde_json::json!({"chunks": [{"file": 0, "index": 2, "size": 8388608, "sha256": "x"}]}),
    )
    .await;
    assert_eq!(s, 400);
    // 実体がないチャンクの完了報告は拒否（Object Storage と照合）
    let (s, _) = req(
        &c,
        "POST",
        &format!("/api/transfers/{id}/chunks"),
        Some(&ta),
        serde_json::json!({"chunks": [{"file": 0, "index": 0, "size": 8388608, "sha256": "x"}]}),
    )
    .await;
    assert_eq!(s, 409);
    // 全チャンク未完了での finalize / received は拒否
    let (s, _) = req(
        &c,
        "POST",
        &format!("/api/transfers/{id}/files/0/finalize"),
        Some(&ta),
        serde_json::json!({"sha256": "AAAA"}),
    )
    .await;
    assert_eq!(s, 409);
}

#[tokio::test]
async fn third_party_cannot_see_or_touch_transfer() {
    let c = core();
    let (_a, ta) = endpoint(&c, "A").await;
    let (b, tb) = endpoint(&c, "B").await;
    let (_x, tx) = endpoint(&c, "X").await;
    let (_, t) = req(
        &c,
        "POST",
        "/api/transfers",
        Some(&ta),
        serde_json::json!({"receiver": b, "kind": "clipboard_text", "text": "secret"}),
    )
    .await;
    let id = t["transfer_id"].as_str().unwrap();
    for (m, p) in [
        ("GET", format!("/api/transfers/{id}")),
        ("DELETE", format!("/api/transfers/{id}")),
        ("POST", format!("/api/transfers/{id}/received")),
    ] {
        let (s, _) = req(&c, m, &p, Some(&tx), serde_json::Value::Null).await;
        assert_eq!(s, 404, "{m} {p} by third party");
    }
    let (s, l) = req(&c, "GET", "/api/transfers", Some(&tx), serde_json::Value::Null).await;
    assert_eq!(s, 200);
    assert_eq!(l["transfers"].as_array().unwrap().len(), 0);
    // 送信者は received を呼べない（受信者のみ）
    let (s, _) = req(
        &c,
        "POST",
        &format!("/api/transfers/{id}/received"),
        Some(&ta),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(s, 404);
    let (s, _) = req(
        &c,
        "POST",
        &format!("/api/transfers/{id}/received"),
        Some(&tb),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(s, 200);
}

#[tokio::test]
async fn revoked_endpoint_token_is_invalid() {
    let c = core();
    let (a, ta) = endpoint(&c, "A").await;
    let (s, _) = req(&c, "GET", "/api/me", Some(&ta), serde_json::Value::Null).await;
    assert_eq!(s, 200);
    let (b, tb) = endpoint(&c, "B").await;
    let (_, t) = req(
        &c,
        "POST",
        "/api/transfers",
        Some(&ta),
        serde_json::json!({"receiver": b, "kind": "files", "files": [{"name": "x", "size": 10, "mime": "a"}]}),
    )
    .await;
    let id = t["transfer_id"].as_str().unwrap().to_string();
    c.revoke_endpoint(&a).await.unwrap();
    let (s, _) = req(&c, "GET", "/api/me", Some(&ta), serde_json::Value::Null).await;
    assert_eq!(s, 401);
    // 失効した送信者の未完了転送は取り消される
    let (_, d) = req(
        &c,
        "GET",
        &format!("/api/transfers/{id}"),
        Some(&tb),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(d["transfer"]["state"], "cancelled");
}
