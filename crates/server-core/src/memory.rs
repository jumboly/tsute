//! プロセス内メモリ実装。ユニットテストとローカル開発サーバーで使う。

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use tsute_proto::{ChunkInfo, Transfer, TransferState};

use crate::traits::*;

#[derive(Default)]
struct Inner {
    ekeys: HashMap<String, (String, i64)>,
    endpoints: BTreeMap<String, EndpointRecord>,
    challenges: HashMap<String, (String, i64)>,
    tokens: HashMap<String, (String, i64)>,
    conns: BTreeMap<String, ConnectionRecord>,
    ws_tickets: HashMap<String, (String, i64)>,
    push_subs: BTreeMap<(String, String), PushSubscriptionRecord>,
    transfers: BTreeMap<String, Transfer>,
    file_sha: HashMap<(String, u32), String>,
    chunks: BTreeMap<(String, u32, u32), ChunkInfo>,
}

#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<Inner>,
}

impl MemoryStore {
    fn with<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        f(&mut self.inner.lock().expect("lock"))
    }
}

impl Store for MemoryStore {
    async fn put_enrollment_key(&self, h: &str, ns: &str, exp: i64) -> Result<()> {
        self.with(|i| i.ekeys.insert(h.into(), (ns.into(), exp)));
        Ok(())
    }
    async fn consume_enrollment_key(&self, h: &str, now: i64) -> Result<Option<String>> {
        Ok(self.with(|i| match i.ekeys.remove(h) {
            Some((ns, exp)) if exp > now => Some(ns),
            _ => None,
        }))
    }
    async fn put_endpoint(&self, ep: &EndpointRecord) -> Result<()> {
        self.with(|i| i.endpoints.insert(ep.endpoint_id.clone(), ep.clone()));
        Ok(())
    }
    async fn get_endpoint(&self, id: &str) -> Result<Option<EndpointRecord>> {
        Ok(self.with(|i| i.endpoints.get(id).cloned()))
    }
    async fn list_endpoints(&self) -> Result<Vec<EndpointRecord>> {
        Ok(self.with(|i| i.endpoints.values().cloned().collect()))
    }
    async fn delete_endpoint(&self, id: &str) -> Result<()> {
        self.with(|i| i.endpoints.remove(id));
        Ok(())
    }
    async fn put_challenge(&self, n: &str, ep: &str, exp: i64) -> Result<()> {
        self.with(|i| i.challenges.insert(n.into(), (ep.into(), exp)));
        Ok(())
    }
    async fn consume_challenge(&self, n: &str, ep: &str, now: i64) -> Result<bool> {
        Ok(self.with(|i| match i.challenges.get(n) {
            Some((e, exp)) if e == ep && *exp > now => {
                i.challenges.remove(n);
                true
            }
            _ => false,
        }))
    }
    async fn put_token(&self, h: &str, ep: &str, exp: i64) -> Result<()> {
        self.with(|i| i.tokens.insert(h.into(), (ep.into(), exp)));
        Ok(())
    }
    async fn get_token(&self, h: &str, now: i64) -> Result<Option<String>> {
        Ok(self.with(|i| i.tokens.get(h).filter(|(_, exp)| *exp > now).map(|(e, _)| e.clone())))
    }
    async fn delete_tokens_of(&self, ep: &str) -> Result<()> {
        self.with(|i| i.tokens.retain(|_, (e, _)| e != ep));
        Ok(())
    }
    async fn put_connection(&self, c: &ConnectionRecord) -> Result<()> {
        self.with(|i| i.conns.insert(c.connection_id.clone(), c.clone()));
        Ok(())
    }
    async fn delete_connection(&self, id: &str) -> Result<Option<String>> {
        Ok(self.with(|i| i.conns.remove(id).map(|c| c.endpoint_id)))
    }
    async fn list_connections(&self, now: i64) -> Result<Vec<ConnectionRecord>> {
        Ok(self.with(|i| i.conns.values().filter(|c| c.expires_at > now).cloned().collect()))
    }
    async fn put_ws_ticket(&self, h: &str, ep: &str, exp: i64) -> Result<()> {
        self.with(|i| i.ws_tickets.insert(h.into(), (ep.into(), exp)));
        Ok(())
    }
    async fn consume_ws_ticket(&self, h: &str, now: i64) -> Result<Option<String>> {
        Ok(self.with(|i| match i.ws_tickets.remove(h) {
            Some((ep, exp)) if exp > now => Some(ep),
            _ => None,
        }))
    }
    async fn put_push_subscription(&self, s: &PushSubscriptionRecord) -> Result<()> {
        self.with(|i| {
            i.push_subs
                .insert((s.endpoint_id.clone(), s.url_hash.clone()), s.clone())
        });
        Ok(())
    }
    async fn delete_push_subscription(&self, ep: &str, h: &str) -> Result<()> {
        self.with(|i| i.push_subs.remove(&(ep.to_string(), h.to_string())));
        Ok(())
    }
    async fn list_push_subscriptions(&self, now: i64) -> Result<Vec<PushSubscriptionRecord>> {
        Ok(self.with(|i| i.push_subs.values().filter(|s| s.expires_at > now).cloned().collect()))
    }
    async fn put_transfer(&self, t: &Transfer) -> Result<()> {
        self.with(|i| i.transfers.insert(t.transfer_id.clone(), t.clone()));
        Ok(())
    }
    async fn get_transfer(&self, id: &str) -> Result<Option<Transfer>> {
        Ok(self.with(|i| {
            i.transfers.get(id).cloned().map(|mut t| {
                for f in &mut t.files {
                    f.sha256 = i.file_sha.get(&(id.to_string(), f.index)).cloned();
                }
                t
            })
        }))
    }
    async fn list_transfers(&self, now: i64) -> Result<Vec<Transfer>> {
        let ids: Vec<String> = self.with(|i| {
            i.transfers
                .values()
                .filter(|t| t.expires_at > now)
                .map(|t| t.transfer_id.clone())
                .collect()
        });
        let mut out = Vec::new();
        for id in ids {
            if let Some(t) = self.get_transfer(&id).await? {
                out.push(t);
            }
        }
        Ok(out)
    }
    async fn transition(&self, id: &str, from: &[TransferState], to: TransferState) -> Result<bool> {
        Ok(self.with(|i| match i.transfers.get_mut(id) {
            Some(t) if from.contains(&t.state) => {
                t.state = to;
                true
            }
            _ => false,
        }))
    }
    async fn put_file_sha(&self, id: &str, file: u32, sha: &str, _exp: i64) -> Result<()> {
        self.with(|i| i.file_sha.insert((id.into(), file), sha.into()));
        Ok(())
    }
    async fn put_chunk(&self, id: &str, c: &ChunkInfo, _exp: i64) -> Result<()> {
        self.with(|i| i.chunks.insert((id.into(), c.file, c.index), c.clone()));
        Ok(())
    }
    async fn list_chunks(&self, id: &str) -> Result<Vec<ChunkInfo>> {
        Ok(self.with(|i| {
            i.chunks
                .iter()
                .filter(|((t, _, _), _)| t == id)
                .map(|(_, c)| c.clone())
                .collect()
        }))
    }
}

/// WebSocket 接続ごとの送信キュー。ローカルサーバーとテストで使う。
#[derive(Default)]
pub struct ChannelNotifier {
    pub senders: Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<String>>>,
}

impl ChannelNotifier {
    pub fn register(&self, id: &str) -> tokio::sync::mpsc::UnboundedReceiver<String> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.senders.lock().expect("lock").insert(id.into(), tx);
        rx
    }
    pub fn unregister(&self, id: &str) {
        self.senders.lock().expect("lock").remove(id);
    }
}

impl Notifier for ChannelNotifier {
    async fn send(&self, id: &str, ev: &tsute_proto::ServerEvent) -> Result<bool> {
        let msg = serde_json::to_string(ev)?;
        Ok(match self.senders.lock().expect("lock").get(id) {
            Some(tx) => tx.send(msg).is_ok(),
            None => false,
        })
    }
}
