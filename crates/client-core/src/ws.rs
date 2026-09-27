//! WebSocket 通知チャネル。
//!
//! 方針（ADR-0007）:
//! - 通知は「ヒント」。接続のたびに呼び出し側が HTTP で全状態を再同期するので、切断中の取りこぼしは許容する。
//! - API Gateway の idle timeout(10分) より短い間隔でアプリレベル ping を送る。
//! - 最大接続時間(2時間)で切られる前に自分から張り直す。
//! - スリープ復帰はタイマーの経過時間の飛びで検出し、死んだソケットの ping 待ちを省く。

use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest};
use tsute_proto::{ClientMessage, ServerEvent};

use crate::api::Api;

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
    let (ws, _) = tokio::time::timeout(Duration::from_secs(20), tokio_tungstenite::connect_async(req))
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
