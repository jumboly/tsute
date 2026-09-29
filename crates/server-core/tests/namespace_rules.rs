//! Namespace 境界のテスト（issue #15）。Namespace は表示上のグループではなく認可境界なので、
//! UI を迂回して ID を直接指定した呼び出しでも Backend が拒否することを確かめる。

use std::collections::HashMap;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signer, SigningKey};
use tokio::sync::mpsc::UnboundedReceiver;
use tsute_proto::*;
use tsute_server_core::memory::{ChannelNotifier, MemoryStore};
use tsute_server_core::traits::{BlobStore, PresignedPut, Result as CoreResult};
use tsute_server_core::{Config, Core, Request, validate_namespace};

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

/// `namespace` 用の Key で登録し (endpoint_id, token) を返す。`extra` は Enroll の本文に足す
async fn endpoint(c: &C, namespace: &str, name: &str, extra: serde_json::Value) -> (String, String) {
    let mut seed = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut seed);
    let key = SigningKey::from_bytes(&seed);
    let (ek, _) = c.issue_enrollment_key(namespace).await.unwrap();
    let mut body = serde_json::json!({"enrollment_key": ek, "name": name, "platform": "macos",
        "public_key": URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes())});
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        body[k] = v;
    }
    let (s, v) = req(c, "POST", "/api/enroll", None, body).await;
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
    (id, t["access_token"].as_str().unwrap().to_string())
}

async fn visible_ids(c: &C, token: &str) -> Vec<String> {
    let (s, v) = req(c, "GET", "/api/endpoints", Some(token), serde_json::Value::Null).await;
    assert_eq!(s, 200);
    let mut ids: Vec<String> = v["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["endpoint_id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

async fn send_text(c: &C, token: &str, receiver: &str) -> (u16, serde_json::Value) {
    req(
        c,
        "POST",
        "/api/transfers",
        Some(token),
        serde_json::json!({"receiver": receiver, "kind": "clipboard_text", "text": "hello"}),
    )
    .await
}

async fn connect(c: &C, conn: &str, token: &str) -> UnboundedReceiver<String> {
    let rx = c.notifier.register(conn);
    let h = HashMap::from([("authorization".to_string(), format!("Bearer {token}"))]);
    c.ws_connect(conn, &h).await.unwrap();
    rx
}

fn drain(rx: &mut UnboundedReceiver<String>) -> Vec<ServerEvent> {
    let mut out = Vec::new();
    while let Ok(m) = rx.try_recv() {
        out.push(serde_json::from_str(&m).unwrap());
    }
    out
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

#[test]
fn namespace_names_are_validated() {
    for ok in ["default", "home", "team-a", "e2e", "a_b", "0", &"a".repeat(64)] {
        assert!(validate_namespace(ok).is_ok(), "{ok}");
    }
    for bad in ["", "Home", "-a", "_a", "a b", "a/b", "a#b", "日本", &"a".repeat(65)] {
        assert!(validate_namespace(bad).is_err(), "{bad}");
    }
}

#[tokio::test]
async fn invalid_namespace_cannot_be_issued() {
    let c = core();
    assert!(c.issue_enrollment_key("").await.is_err());
    assert!(c.issue_enrollment_key("A#B").await.is_err());
}

#[tokio::test]
async fn enrollment_key_decides_namespace() {
    let c = core();
    let (a, _) = endpoint(&c, "team-a", "A", serde_json::json!({})).await;
    // クライアントが本文で別の Namespace を申告しても、Key に紐付いた Namespace になる
    let (b, _) = endpoint(&c, "team-b", "B", serde_json::json!({"namespace": "team-a"})).await;
    let all = c.list_endpoints_admin(None).await.unwrap();
    let ns_of = |id: &str| {
        all.iter()
            .find(|e| e.endpoint.endpoint_id == id)
            .unwrap()
            .namespace
            .clone()
    };
    assert_eq!(ns_of(&a), "team-a");
    assert_eq!(ns_of(&b), "team-b");
    // 管理一覧の出力には所属が併記される
    let j = serde_json::to_value(&all[0]).unwrap();
    assert!(j["namespace"].is_string() && j["endpoint_id"].is_string(), "{j}");

    // Namespace で絞り込める
    let only_a = c.list_endpoints_admin(Some("team-a")).await.unwrap();
    assert_eq!(only_a.len(), 1);
    assert_eq!(only_a[0].endpoint.endpoint_id, a);
    assert!(c.list_endpoints_admin(Some("none")).await.unwrap().is_empty());
}

#[tokio::test]
async fn endpoints_are_visible_only_within_namespace() {
    let c = core();
    let (a1, ta1) = endpoint(&c, "team-a", "A1", serde_json::json!({})).await;
    let (a2, ta2) = endpoint(&c, "team-a", "A2", serde_json::json!({})).await;
    let (b1, tb1) = endpoint(&c, "team-b", "B1", serde_json::json!({})).await;

    assert_eq!(visible_ids(&c, &ta1).await, sorted(vec![a1.clone(), a2.clone()]));
    assert_eq!(visible_ids(&c, &ta2).await, sorted(vec![a1.clone(), a2.clone()]));
    assert_eq!(visible_ids(&c, &tb1).await, vec![b1.clone()]);

    let (s, v) = req(&c, "GET", "/api/me", Some(&tb1), serde_json::Value::Null).await;
    assert_eq!(s, 200);
    assert_eq!(v["endpoint"]["endpoint_id"], b1.as_str());
}

#[tokio::test]
async fn cross_namespace_transfer_is_rejected_like_unknown_receiver() {
    let c = core();
    let (_a1, ta1) = endpoint(&c, "team-a", "A1", serde_json::json!({})).await;
    let (a2, ta2) = endpoint(&c, "team-a", "A2", serde_json::json!({})).await;
    let (b1, tb1) = endpoint(&c, "team-b", "B1", serde_json::json!({})).await;

    // 同じ Namespace には従来どおり送れる
    let (s, t) = send_text(&c, &ta1, &a2).await;
    assert_eq!(s, 200, "{t}");

    // 別 Namespace の ID を直接指定しても拒否。存在しない ID と同じ応答で、存在も判別できない
    let (s, cross) = send_text(&c, &ta1, &b1).await;
    let (s2, unknown) = send_text(&c, &ta1, "ep_does_not_exist").await;
    assert_eq!((s, s2), (400, 400));
    assert_eq!(cross, unknown);
    let (s, _) = send_text(&c, &tb1, &a2).await;
    assert_eq!(s, 400);

    // Files 転送も同じ
    let (s, _) = req(
        &c,
        "POST",
        "/api/transfers",
        Some(&ta1),
        serde_json::json!({"receiver": b1, "kind": "files",
            "files": [{"name": "a.txt", "size": 3, "mime": "text/plain"}]}),
    )
    .await;
    assert_eq!(s, 400);

    // 別 Namespace の Endpoint は同 Namespace の Transfer に一切触れられない（存在も見せない）
    let id = t["transfer_id"].as_str().unwrap();
    for (m, p, body) in [
        ("GET", format!("/api/transfers/{id}"), serde_json::Value::Null),
        (
            "POST",
            format!("/api/transfers/{id}/download-urls"),
            serde_json::json!({"chunks": []}),
        ),
        ("POST", format!("/api/transfers/{id}/received"), serde_json::Value::Null),
        ("DELETE", format!("/api/transfers/{id}"), serde_json::Value::Null),
    ] {
        let (s, v) = req(&c, m, &p, Some(&tb1), body).await;
        assert_eq!(s, 404, "{m} {p}: {v}");
    }
    let (_, v) = req(&c, "GET", "/api/transfers", Some(&tb1), serde_json::Value::Null).await;
    assert!(v["transfers"].as_array().unwrap().is_empty());

    // 受信者本人は受け取れる（拒否した操作で状態が壊れていない）
    let (s, _) = req(
        &c,
        "POST",
        &format!("/api/transfers/{id}/received"),
        Some(&ta2),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(s, 200);
}

#[tokio::test]
async fn notifications_stay_within_namespace() {
    let c = core();
    let (a1, ta1) = endpoint(&c, "team-a", "A1", serde_json::json!({})).await;
    let (a2, ta2) = endpoint(&c, "team-a", "A2", serde_json::json!({})).await;
    let (b1, tb1) = endpoint(&c, "team-b", "B1", serde_json::json!({})).await;
    let mut rx_a2 = connect(&c, "conn-a2", &ta2).await;
    let mut rx_b1 = connect(&c, "conn-b1", &tb1).await;
    drain(&mut rx_a2);
    drain(&mut rx_b1);

    // Presence（接続）: 別 Namespace へ Endpoint ID を漏らさない
    let mut rx_a1 = connect(&c, "conn-a1", &ta1).await;
    let ev = drain(&mut rx_a2);
    assert!(ev.contains(&ServerEvent::Presence {
        endpoint_id: a1.clone(),
        online: true
    }));
    assert!(drain(&mut rx_b1).is_empty());

    // 登録・名前変更の EndpointsChanged
    let (a3, _) = endpoint(&c, "team-a", "A3", serde_json::json!({})).await;
    let (s, _) = req(
        &c,
        "PUT",
        "/api/me/name",
        Some(&ta1),
        serde_json::json!({"name": "A1'"}),
    )
    .await;
    assert_eq!(s, 200);
    assert!(drain(&mut rx_a2).contains(&ServerEvent::EndpointsChanged));
    assert!(drain(&mut rx_b1).is_empty());

    // 転送の通知は受信者だけ
    let (s, _) = send_text(&c, &ta1, &a2).await;
    assert_eq!(s, 200);
    assert!(
        drain(&mut rx_a2)
            .iter()
            .any(|e| matches!(e, ServerEvent::TransferCreated { .. }))
    );
    assert!(drain(&mut rx_b1).is_empty());

    // 失効の EndpointsChanged も失効した Endpoint の Namespace だけ
    drain(&mut rx_a1);
    c.revoke_endpoint(&a3).await.unwrap();
    assert!(drain(&mut rx_a1).contains(&ServerEvent::EndpointsChanged));
    assert!(drain(&mut rx_b1).is_empty());

    // Presence（切断）
    c.ws_disconnect("conn-a1").await.unwrap();
    assert!(drain(&mut rx_a2).contains(&ServerEvent::Presence {
        endpoint_id: a1.clone(),
        online: false
    }));
    assert!(drain(&mut rx_b1).is_empty());

    // 逆方向（team-b の出来事は team-a に届かない）
    let (_b2, _) = endpoint(&c, "team-b", "B2", serde_json::json!({})).await;
    assert!(drain(&mut rx_b1).contains(&ServerEvent::EndpointsChanged));
    assert!(drain(&mut rx_a2).is_empty());
    let _ = b1;
}
