//! WebSocket 通知チャネル。
//!
//! 方針（ADR-0007）:
//! - 通知は「ヒント」。接続のたびに呼び出し側が HTTP で全状態を再同期するので、切断中の取りこぼしは許容する。
//! - API Gateway の idle timeout(10分) より短い間隔でアプリレベル ping を送る。
//! - 最大接続時間(2時間)で切られる前に自分から張り直す。
//! - スリープ復帰はタイマーの経過時間の飛びで検出し、死んだソケットの ping 待ちを省く。

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use hyper_util::client::proxy::matcher::Matcher;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::handshake::client::{Request, Response};
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest, http::Uri};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tsute_proto::{ClientMessage, ServerEvent};

use crate::api::Api;

/// REST（reqwest の既定）と同じ判定: 環境変数（HTTPS_PROXY / NO_PROXY 等）、無ければ OS の設定
static PROXY: LazyLock<Matcher> = LazyLock::new(Matcher::from_system);

pub const PING_INTERVAL: Duration = Duration::from_secs(4 * 60);
const PONG_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONN_AGE: Duration = Duration::from_secs(110 * 60);
const TICK: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnState {
    Connecting,
    Online,
    Offline,
}

pub enum WsSignal {
    Connected,
    Event(ServerEvent),
    Disconnected,
}

/// 停止要求（`stop` が true になる）まで接続を維持し続ける
pub async fn run(
    api: Api,
    out: mpsc::UnboundedSender<WsSignal>,
    state: watch::Sender<ConnState>,
    mut stop: watch::Receiver<bool>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if *stop.borrow() {
            return;
        }
        let _ = state.send(ConnState::Connecting);
        match session(&api, &out, &state, &mut stop).await {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(e) => tracing::info!(error = %e, "ws session ended"),
        }
        let _ = state.send(ConnState::Offline);
        let _ = out.send(WsSignal::Disconnected);
        if *stop.borrow() {
            return;
        }
        // 全クライアントが同時に再接続しないようジッターを加える
        let jitter = Duration::from_millis(rand::random::<u64>() % 1000);
        tokio::select! {
            _ = tokio::time::sleep(backoff + jitter) => {}
            _ = stop.changed() => return,
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

async fn session(
    api: &Api,
    out: &mpsc::UnboundedSender<WsSignal>,
    state: &watch::Sender<ConnState>,
    stop: &mut watch::Receiver<bool>,
) -> Result<(), crate::Error> {
    let token = api.token().await?;
    let mut req = api
        .ws_url()
        .into_client_request()
        .map_err(|e| crate::Error::Protocol(e.to_string()))?;
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {token}")
            .parse()
            .map_err(|_| crate::Error::Protocol("token header".into()))?,
    );
    let (ws, _) = tokio::time::timeout(Duration::from_secs(20), connect(req, &PROXY))
        .await
        .map_err(|_| crate::Error::Protocol("ws connect timeout".into()))?
        .map_err(|e| match e {
            tungstenite::Error::Http(r) => crate::Error::Protocol(format!("ws handshake status {}", r.status())),
            e => crate::Error::Protocol(format!("ws connect: {e}")),
        })?;
    let (mut tx, mut rx) = ws.split();
    let _ = state.send(ConnState::Online);
    let _ = out.send(WsSignal::Connected);
    let started = Instant::now();
    let mut last_ping = Instant::now();
    let mut awaiting_pong: Option<Instant> = None;
    let mut last_tick = Instant::now();
    let mut tick = tokio::time::interval(TICK);
    let ping = serde_json::to_string(&ClientMessage::Ping).expect("json");
    loop {
        tokio::select! {
            _ = stop.changed() => {
                let _ = tx.send(Message::Close(None)).await;
                return Ok(());
            }
            m = rx.next() => match m {
                Some(Ok(Message::Text(t))) => match serde_json::from_str::<ServerEvent>(&t) {
                    Ok(ServerEvent::Pong) => awaiting_pong = None,
                    Ok(ev) => { let _ = out.send(WsSignal::Event(ev)); }
                    Err(e) => tracing::debug!(error = %e, "unknown ws message"),
                },
                Some(Ok(Message::Ping(p))) => { let _ = tx.send(Message::Pong(p)).await; }
                Some(Ok(Message::Close(_))) | None => return Err(crate::Error::Protocol("closed by server".into())),
                Some(Err(e)) => return Err(crate::Error::Protocol(format!("ws: {e}"))),
                _ => {}
            },
            _ = tick.tick() => {
                let gap = last_tick.elapsed();
                last_tick = Instant::now();
                if gap > TICK * 6 {
                    // 大きな時間の飛び = スリープ復帰。ソケットは死んでいる可能性が高いので即張り直す
                    return Err(crate::Error::Protocol(format!("clock gap {gap:?} (sleep/wake?)")));
                }
                if started.elapsed() > MAX_CONN_AGE {
                    let _ = tx.send(Message::Close(None)).await;
                    return Ok(());
                }
                if let Some(t) = awaiting_pong && t.elapsed() > PONG_TIMEOUT {
                    return Err(crate::Error::Protocol("pong timeout".into()));
                }
                if last_ping.elapsed() >= PING_INTERVAL {
                    last_ping = Instant::now();
                    awaiting_pong = Some(Instant::now());
                    tx.send(Message::Text(ping.clone().into())).await.map_err(|e| crate::Error::Protocol(e.to_string()))?;
                }
            }
        }
    }
}

/// プロキシの対象なら `CONNECT` でトンネルを張ってから TLS + WebSocket のハンドシェイクを行う。
/// tungstenite はプロキシを扱わないため、外へ直接つながらない環境（社内 LAN 等）では REST だけ通って
/// WebSocket が常時オフラインになっていた。
async fn connect(
    req: Request,
    proxy: &Matcher,
) -> Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, Response), tungstenite::Error> {
    let uri = req.uri().clone();
    let (scheme, default_port) = if uri.scheme_str() == Some("wss") {
        ("https", 443)
    } else {
        ("http", 80)
    };
    let host = uri.host().unwrap_or_default().to_string();
    let target = format!("{host}:{}", uri.port_u16().unwrap_or(default_port));
    let Some(via) = format!("{scheme}://{target}/")
        .parse::<Uri>()
        .ok()
        .and_then(|u| proxy.intercept(&u))
    else {
        return tokio_tungstenite::connect_async(req).await;
    };
    let p = via.uri();
    if p.scheme_str().is_some_and(|s| s != "http") {
        return Err(proxy_error(format!(
            "unsupported proxy scheme: {}",
            p.scheme_str().unwrap_or_default()
        )));
    }
    let mut stream = TcpStream::connect((p.host().unwrap_or_default(), p.port_u16().unwrap_or(80))).await?;
    let mut head = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if let Some(auth) = via.basic_auth().and_then(|v| v.to_str().ok()) {
        head.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    // 応答ヘッダーの終わりまでだけ読む（その先はトンネルの中身なので読み過ごさない）
    let mut resp = Vec::new();
    while !resp.ends_with(b"\r\n\r\n") {
        if resp.len() > 8192 {
            return Err(proxy_error("proxy response too large".into()));
        }
        let mut b = [0u8; 1];
        if stream.read(&mut b).await? == 0 {
            return Err(proxy_error("proxy closed the connection".into()));
        }
        resp.push(b[0]);
    }
    let status = String::from_utf8_lossy(&resp)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    if status.split_whitespace().nth(1) != Some("200") {
        return Err(proxy_error(format!("proxy CONNECT failed: {status}")));
    }
    tokio_tungstenite::client_async_tls(req, stream).await
}

fn proxy_error(msg: String) -> tungstenite::Error {
    tungstenite::Error::Io(std::io::Error::other(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    /// 受けたテキストをそのまま返す WebSocket サーバー
    async fn echo_server() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(s).await.unwrap();
            while let Some(Ok(m)) = ws.next().await {
                if m.is_text() {
                    ws.send(m).await.unwrap();
                }
            }
        });
        port
    }

    /// CONNECT を受けるプロキシ。受けた要求ヘッダーを返し、`reply` が 200 ならトンネルを中継する
    async fn fake_proxy(reply: &'static str) -> (u16, oneshot::Receiver<String>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                let mut b = [0u8; 1];
                s.read_exact(&mut b).await.unwrap();
                head.push(b[0]);
            }
            let head = String::from_utf8(head).unwrap();
            let target = head.split_whitespace().nth(1).unwrap().to_string();
            let _ = tx.send(head);
            s.write_all(format!("{reply}\r\n\r\n").as_bytes()).await.unwrap();
            if reply.contains(" 200 ") {
                let mut up = TcpStream::connect(target).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut s, &mut up).await;
            }
        });
        (port, rx)
    }

    async fn roundtrip(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) -> String {
        ws.send(Message::Text("hello".into())).await.unwrap();
        ws.next().await.unwrap().unwrap().into_text().unwrap().to_string()
    }

    #[tokio::test]
    async fn connects_through_http_proxy_with_basic_auth() {
        let ws_port = echo_server().await;
        let (proxy_port, head) = fake_proxy("HTTP/1.1 200 Connection established").await;
        let proxy = Matcher::builder()
            .http(format!("http://user:pass@127.0.0.1:{proxy_port}"))
            .build();
        let req = format!("ws://127.0.0.1:{ws_port}/ws").into_client_request().unwrap();
        let (mut ws, _) = connect(req, &proxy).await.unwrap();
        assert_eq!(roundtrip(&mut ws).await, "hello");
        let head = head.await.unwrap();
        assert!(
            head.starts_with(&format!("CONNECT 127.0.0.1:{ws_port} HTTP/1.1\r\n")),
            "{head}"
        );
        // user:pass の Base64
        assert!(head.contains("Proxy-Authorization: Basic dXNlcjpwYXNz\r\n"), "{head}");
    }

    #[tokio::test]
    async fn proxy_refusal_is_reported() {
        let (proxy_port, _head) = fake_proxy("HTTP/1.1 407 Proxy Authentication Required").await;
        let proxy = Matcher::builder()
            .http(format!("http://127.0.0.1:{proxy_port}"))
            .build();
        let req = "ws://127.0.0.1:9/ws".into_client_request().unwrap();
        let e = connect(req, &proxy).await.unwrap_err().to_string();
        assert!(e.contains("407"), "{e}");
    }

    #[tokio::test]
    async fn connects_directly_without_proxy_and_honors_no_proxy() {
        let ws_port = echo_server().await;
        // プロキシは設定されているが NO_PROXY で除外される宛先（プロキシには一度もつながらない）
        let proxy = Matcher::builder().http("http://127.0.0.1:9").no("127.0.0.1").build();
        let req = format!("ws://127.0.0.1:{ws_port}/ws").into_client_request().unwrap();
        let (mut ws, _) = connect(req, &proxy).await.unwrap();
        assert_eq!(roundtrip(&mut ws).await, "hello");
    }
}
