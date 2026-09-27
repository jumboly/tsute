//! Web / PWA 向けに追加したルール（ADR-0015）のテスト:
//! Capability（accepts / reach）、WebSocket ticket、Web Push の送信条件と購読の所有権。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signer, SigningKey};
use tsute_proto::*;
use tsute_server_core::memory::{ChannelNotifier, MemoryStore};
use tsute_server_core::traits::{BlobStore, PresignedPut, PushOutcome, Pusher, Result as CoreResult};
use tsute_server_core::{Config, Core, Request, is_allowed_push_url};

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

/// 送った購読 URL を記録する。`gone` に入れた URL には 410 相当を返す
#[derive(Default, Clone)]
struct RecPusher {
    sent: Arc<Mutex<Vec<String>>>,
    gone: Arc<Mutex<Vec<String>>>,
}
impl Pusher for RecPusher {
    fn vapid_public_key(&self) -> Option<String> {
        Some("BPUBKEY".into())
    }
    async fn push(&self, url: &str) -> CoreResult<PushOutcome> {
        self.sent.lock().unwrap().push(url.into());
        Ok(if self.gone.lock().unwrap().iter().any(|g| g == url) {
            PushOutcome::Gone
        } else {
            PushOutcome::Sent
        })
    }
}

type C = Core<MemoryStore, NoBlob, ChannelNotifier, RecPusher>;

fn core() -> (C, RecPusher) {
    let p = RecPusher::default();
    let c = Core::new(
        MemoryStore::default(),
        NoBlob,
        ChannelNotifier::default(),
        Config::default(),
    )
    .with_pusher(p.clone());
    (c, p)
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

/// `extra` を Enroll の本文に足して登録し、(endpoint_id, token) を返す
async fn endpoint(c: &C, name: &str, extra: serde_json::Value) -> (String, String) {
    let mut seed = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut seed);
    let key = SigningKey::from_bytes(&seed);
    let (ek, _) = c.issue_enrollment_key().await.unwrap();
    let mut body = serde_json::json!({"enrollment_key": ek, "name": name, "platform": "other",
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
    let (_, t) = req(
        c,
        "POST",
        "/api/auth/token",
        None,
        serde_json::json!({"endpoint_id": id, "nonce": nonce, "signature": URL_SAFE_NO_PAD.encode(sig.to_bytes())}),
    )
    .await;
    (id, t["access_token"].as_str().unwrap().to_string())
}

fn web() -> serde_json::Value {
    serde_json::json!({"client_kind": "web", "accepts": ["clipboard_text", "clipboard_image"]})
}

fn ws_headers(protocols: &str) -> HashMap<String, String> {
    HashMap::from([("sec-websocket-protocol".to_string(), protocols.to_string())])
}

async fn ticket(c: &C, token: &str) -> String {
    let (s, v) = req(c, "POST", "/api/ws-ticket", Some(token), serde_json::Value::Null).await;
    assert_eq!(s, 200);
    v["ticket"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn accepts_is_reported_and_enforced() {
    let (c, _) = core();
    let (_n, tn) = endpoint(&c, "Mac", serde_json::json!({})).await;
    let (w, tw) = endpoint(&c, "iPhone", web()).await;
    let (s, l) = req(&c, "GET", "/api/endpoints", Some(&tn), serde_json::Value::Null).await;
    assert_eq!(s, 200);
    let eps = l["endpoints"].as_array().unwrap();
    let native = eps.iter().find(|e| e["name"] == "Mac").unwrap();
    // 申告なし（Phase 1 のクライアント）は Native の全種類
    assert_eq!(native["client_kind"], "native");
    assert_eq!(native["accepts"].as_array().unwrap().len(), 4);
    let webep = eps.iter().find(|e| e["name"] == "iPhone").unwrap();
    assert_eq!(webep["client_kind"], "web");
    assert_eq!(
        webep["accepts"],
        serde_json::json!(["clipboard_text", "clipboard_image"])
    );

    // Web が受けられない種類はサーバーが拒否する（UI を迂回した呼び出しへの多層防御）
    for kind in ["clipboard_video", "files"] {
        let (s, v) = req(
            &c,
            "POST",
            "/api/transfers",
            Some(&tn),
            serde_json::json!({"receiver": w, "kind": kind, "files": [{"name": "v.mov", "size": 10, "mime": "video/quicktime"}]}),
        )
        .await;
        assert_eq!(s, 422, "{kind}");
        assert_eq!(v["error"], "receiver_cannot_accept");
    }
    let (s, _) = req(
        &c,
        "POST",
        "/api/transfers",
        Some(&tn),
        serde_json::json!({"receiver": w, "kind": "clipboard_text", "text": "hi"}),
    )
    .await;
    assert_eq!(s, 200);

    // 申告の更新（重複は集合として扱う）
    let (s, me) = req(
        &c,
        "PUT",
        "/api/me/capabilities",
        Some(&tw),
        serde_json::json!({"accepts": ["clipboard_text", "clipboard_text"]}),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(me["endpoint"]["accepts"], serde_json::json!(["clipboard_text"]));
    let (s, _) = req(
        &c,
        "POST",
        "/api/transfers",
        Some(&tn),
        serde_json::json!({"receiver": w, "kind": "clipboard_image", "files": [{"name": "a.png", "size": 10, "mime": "image/png"}]}),
    )
    .await;
    assert_eq!(s, 422);
}

#[tokio::test]
async fn ws_ticket_is_single_use_and_needs_subprotocol() {
    let (c, _) = core();
    let (w, tw) = endpoint(&c, "Web", web()).await;
    // ticket なし・偽 ticket は拒否
    assert!(c.ws_connect("c0", &HashMap::new()).await.is_err());
    assert!(
        c.ws_connect("c0", &ws_headers("tsute.v1, ticket.tsute-wt-bogus"))
            .await
            .is_err()
    );
    let t = ticket(&c, &tw).await;
    // tsute.v1 を要求していない（echo すべきプロトコルがない）接続は拒否し、ticket も消費しない
    assert!(c.ws_connect("c1", &ws_headers(&format!("ticket.{t}"))).await.is_err());
    let acc = c
        .ws_connect("c1", &ws_headers(&format!("tsute.v1, ticket.{t}")))
        .await
        .unwrap();
    assert_eq!(acc.endpoint_id, w);
    assert_eq!(acc.subprotocol, Some(WS_SUBPROTOCOL));
    // 同じ ticket の再利用は拒否（一回限り）
    assert!(
        c.ws_connect("c2", &ws_headers(&format!("tsute.v1, ticket.{t}")))
            .await
            .is_err()
    );
    // ticket の発行には認証が必要
    let (s, _) = req(&c, "POST", "/api/ws-ticket", None, serde_json::Value::Null).await;
    assert_eq!(s, 401);
    // Native の Authorization ヘッダ経路は従来どおりで、サブプロトコルは返さない
    let (_n, tn) = endpoint(&c, "Mac", serde_json::json!({})).await;
    let acc = c
        .ws_connect(
            "c3",
            &HashMap::from([("authorization".to_string(), format!("Bearer {tn}"))]),
        )
        .await
        .unwrap();
    assert_eq!(acc.subprotocol, None);
}

#[tokio::test]
async fn expired_ws_ticket_rejected() {
    let p = RecPusher::default();
    let c = Core::new(
        MemoryStore::default(),
        NoBlob,
        ChannelNotifier::default(),
        Config {
            ws_ticket_ttl_secs: -1,
            ..Default::default()
        },
    )
    .with_pusher(p);
    let (_w, tw) = endpoint(&c, "Web", web()).await;
    let t = ticket(&c, &tw).await;
    assert!(
        c.ws_connect("c1", &ws_headers(&format!("tsute.v1, ticket.{t}")))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn push_only_when_no_live_websocket() {
    let (c, p) = core();
    let (_n, tn) = endpoint(&c, "Mac", serde_json::json!({})).await;
    let (w, tw) = endpoint(&c, "Web", web()).await;
    let sub = "https://fcm.googleapis.com/fcm/send/abc";
    let (s, _) = req(
        &c,
        "PUT",
        "/api/push/subscription",
        Some(&tw),
        serde_json::json!({"endpoint": sub}),
    )
    .await;
    assert_eq!(s, 200);
    let (_, l) = req(&c, "GET", "/api/endpoints", Some(&tn), serde_json::Value::Null).await;
    let webep = l["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["endpoint_id"] == w)
        .cloned()
        .unwrap();
    assert_eq!(webep["reach"], serde_json::json!(["web_push"]));

    let send = |text: &'static str| {
        let c = &c;
        let tn = tn.clone();
        let w = w.clone();
        async move {
            let (s, _) = req(
                c,
                "POST",
                "/api/transfers",
                Some(&tn),
                serde_json::json!({"receiver": w, "kind": "clipboard_text", "text": text}),
            )
            .await;
            assert_eq!(s, 200);
        }
    };
    // WS がない（Background / Closed）→ Push
    send("1").await;
    assert_eq!(p.sent.lock().unwrap().as_slice(), [sub.to_string()]);

    // Foreground で WS に届いたときは Push しない（iOS で通知が二重にならないように）
    let _rx = c.notifier.register("conn-web");
    let t = ticket(&c, &tw).await;
    c.ws_connect("conn-web", &ws_headers(&format!("tsute.v1, ticket.{t}")))
        .await
        .unwrap();
    send("2").await;
    assert_eq!(p.sent.lock().unwrap().len(), 1);

    // WS レコードは残っているが実際には送れない（切断済み）→ Push にフォールバック
    c.notifier.unregister("conn-web");
    send("3").await;
    assert_eq!(p.sent.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn push_subscription_ownership_and_gone_cleanup() {
    let (c, p) = core();
    let (_n, tn) = endpoint(&c, "Mac", serde_json::json!({})).await;
    let (w, tw) = endpoint(&c, "Web", web()).await;
    let (_x, tx) = endpoint(&c, "Other", web()).await;
    let sub = "https://web.push.apple.com/QGx1";
    req(
        &c,
        "PUT",
        "/api/push/subscription",
        Some(&tw),
        serde_json::json!({"endpoint": sub}),
    )
    .await;
    // 他の Endpoint は同じ URL を指定しても自分の購読しか消せない
    let (s, _) = req(
        &c,
        "DELETE",
        "/api/push/subscription",
        Some(&tx),
        serde_json::json!({"endpoint": sub}),
    )
    .await;
    assert_eq!(s, 200);
    let (_, l) = req(&c, "GET", "/api/endpoints", Some(&tn), serde_json::Value::Null).await;
    let reach = |l: &serde_json::Value| {
        l["endpoints"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["endpoint_id"] == w)
            .unwrap()["reach"]
            .clone()
    };
    assert_eq!(reach(&l), serde_json::json!(["web_push"]));
    // 未認証では登録できない
    let (s, _) = req(
        &c,
        "PUT",
        "/api/push/subscription",
        None,
        serde_json::json!({"endpoint": sub}),
    )
    .await;
    assert_eq!(s, 401);

    // push service が 410 を返したら購読を消す
    p.gone.lock().unwrap().push(sub.into());
    req(
        &c,
        "POST",
        "/api/transfers",
        Some(&tn),
        serde_json::json!({"receiver": w, "kind": "clipboard_text", "text": "x"}),
    )
    .await;
    let (_, l) = req(&c, "GET", "/api/endpoints", Some(&tn), serde_json::Value::Null).await;
    assert_eq!(reach(&l), serde_json::json!([]));

    // revoke で購読も消える
    req(
        &c,
        "PUT",
        "/api/push/subscription",
        Some(&tx),
        serde_json::json!({"endpoint": "https://fcm.googleapis.com/x"}),
    )
    .await;
    let (xid, _) = {
        let (_, me) = req(&c, "GET", "/api/me", Some(&tx), serde_json::Value::Null).await;
        (me["endpoint"]["endpoint_id"].as_str().unwrap().to_string(), ())
    };
    c.revoke_endpoint(&xid).await.unwrap();
    let (_, l) = req(&c, "GET", "/api/endpoints", Some(&tn), serde_json::Value::Null).await;
    assert!(
        l["endpoints"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["endpoint_id"] != xid.as_str())
    );
}

#[tokio::test]
async fn push_subscription_count_is_capped() {
    let (c, p) = core();
    let (_n, tn) = endpoint(&c, "Mac", serde_json::json!({})).await;
    let (w, tw) = endpoint(&c, "Web", web()).await;
    for i in 0..8 {
        let (s, _) = req(
            &c,
            "PUT",
            "/api/push/subscription",
            Some(&tw),
            serde_json::json!({"endpoint": format!("https://fcm.googleapis.com/fcm/send/{i}")}),
        )
        .await;
        assert_eq!(s, 200);
    }
    req(
        &c,
        "POST",
        "/api/transfers",
        Some(&tn),
        serde_json::json!({"receiver": w, "kind": "clipboard_text", "text": "x"}),
    )
    .await;
    // Browser が購読を作り直すと古い URL は届かなくなるので、上限を超えた古いものから消える
    assert_eq!(p.sent.lock().unwrap().len(), 5);
    assert!(!p.sent.lock().unwrap().iter().any(|u| u.ends_with("/0")));
}

#[test]
fn push_url_allowlist() {
    for ok in [
        "https://fcm.googleapis.com/fcm/send/abc",
        "https://updates.push.services.mozilla.com/wpush/v2/x",
        "https://web.push.apple.com/QGx1",
        "https://wns2-par02p.notify.windows.com/w/?token=x",
    ] {
        assert!(is_allowed_push_url(ok), "{ok}");
    }
    for bad in [
        "http://fcm.googleapis.com/fcm/send/abc",
        "https://fcm.googleapis.com.evil.test/x",
        "https://evilfcm.googleapis.com.attacker/x",
        "https://fcm.googleapis.com@169.254.169.254/latest",
        "https://fcm.googleapis.com:8443/x",
        "https://169.254.169.254/latest/meta-data",
        "https://localhost/x",
        "https://notapple-push.apple.com.evil/x",
        "file:///etc/passwd",
    ] {
        assert!(!is_allowed_push_url(bad), "{bad}");
    }
}

#[tokio::test]
async fn push_rejects_disallowed_url_and_disabled_config() {
    let (c, _) = core();
    let (_w, tw) = endpoint(&c, "Web", web()).await;
    let (s, _) = req(
        &c,
        "PUT",
        "/api/push/subscription",
        Some(&tw),
        serde_json::json!({"endpoint": "https://169.254.169.254/latest"}),
    )
    .await;
    assert_eq!(s, 400);
    let (s, v) = req(&c, "GET", "/api/push/config", Some(&tw), serde_json::Value::Null).await;
    assert_eq!(s, 200);
    assert_eq!(v["vapid_public_key"], "BPUBKEY");
    let (s, _) = req(&c, "GET", "/api/push/config", None, serde_json::Value::Null).await;
    assert_eq!(s, 401, "config requires auth");
}
