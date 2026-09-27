//! 転送エンジンと `Client` ファサード。
//!
//! 設計（ADR-0004/0005）:
//! - 大きなデータはアプリ側で固定長チャンクに分け、各チャンクを独立 Object として presigned URL で PUT する。
//!   S3 Multipart と違い、アップロード済みチャンクを受信側がすぐに GET できるため Upload/Download が重なる。
//! - チャンクごとの SHA-256 を PUT 署名に含め Object Storage 側で検証、受信側でも再検証する。
//!   さらにファイル全体の SHA-256 を finalize 時に送り、受信側は組み立て後に最終検証する。
//! - どのチャンクがアップロード済みかはサーバーが正。受信済みチャンクはローカル DB が正。
//!   これにより送受信どちらもアプリ再起動後に不足分だけ再開できる。
//! - 受信側は最終サイズで確保した part ファイルに offset 指定で直接書き、再結合用の二重ディスク消費をしない。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, Semaphore, broadcast, mpsc, watch};
use tsute_proto::*;

use crate::api::Api;
use crate::db::{Db, Direction, HistoryItem, LocalStatus};
use crate::profile::{Profile, ProfileConfig};
use crate::secrets::SecretStore;
use crate::ws::{self, ConnState, WsSignal};
use crate::Error;

/// 同時転送数。根拠は ADR-0004（家庭/オフィス回線で帯域を使い切りつつメモリを 8MiB×4 程度に抑える）
pub const UPLOAD_CONCURRENCY: usize = 4;
pub const DOWNLOAD_CONCURRENCY: usize = 4;
const MAX_ATTEMPTS: u32 = 6;
/// WebSocket 通知を取りこぼした場合でも進むようにする保険のポーリング間隔
const FALLBACK_POLL: Duration = Duration::from_secs(20);
const RETRY_FAILED_TRANSFER_AFTER: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientEvent {
    Connection { state: ConnState },
    EndpointsChanged,
    Progress { transfer_id: String, direction: Direction, done_bytes: u64, total_bytes: u64 },
    /// 履歴（状態）が変わった
    TransferUpdated { transfer_id: String },
    /// 受信が完了し、ユーザーが Clipboard 反映/保存できる状態になった
    IncomingReady { transfer_id: String },
    /// 送信した転送を相手が受け取った
    Delivered { transfer_id: String },
    TransferFailed { transfer_id: String, message: String },
}

#[derive(Debug, Clone)]
pub struct OutgoingFile {
    pub path: PathBuf,
    /// 受信側で使うファイル名（None なら path のファイル名）
    pub name: Option<String>,
    pub mime: Option<String>,
    pub media: MediaInfo,
}

struct Inner {
    profile: Profile,
    config: Mutex<ProfileConfig>,
    api: Api,
    db: Db,
    events: broadcast::Sender<ClientEvent>,
    up_sem: Semaphore,
    down_sem: Semaphore,
    running: Mutex<HashSet<String>>,
    wakers: Mutex<HashMap<String, Arc<Notify>>>,
    conn_state: watch::Sender<ConnState>,
    stop: watch::Sender<bool>,
    progress: Mutex<HashMap<String, (u64, u64)>>,
}

#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

fn io_err(m: impl Into<String>) -> Error {
    Error::Other(m.into())
}

fn mtime_ns(meta: &std::fs::Metadata) -> Option<i64> {
    meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_nanos() as i64)
}

fn read_at(path: &Path, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
    let f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; len as usize];
    let mut done = 0usize;
    while done < buf.len() {
        #[cfg(unix)]
        let n = std::os::unix::fs::FileExt::read_at(&f, &mut buf[done..], offset + done as u64)?;
        #[cfg(windows)]
        let n = std::os::windows::fs::FileExt::seek_read(&f, &mut buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "file shrank during transfer"));
        }
        done += n;
    }
    Ok(buf)
}

fn write_at(path: &Path, offset: u64, data: &[u8]) -> std::io::Result<()> {
    let f = std::fs::OpenOptions::new().write(true).open(path)?;
    let mut done = 0usize;
    while done < data.len() {
        #[cfg(unix)]
        let n = std::os::unix::fs::FileExt::write_at(&f, &data[done..], offset + done as u64)?;
        #[cfg(windows)]
        let n = std::os::windows::fs::FileExt::seek_write(&f, &data[done..], offset + done as u64)?;
        done += n;
    }
    // チャンクを「受信済み」と DB に記録する前にディスクへ確実に書く（クラッシュ後の再開で欠損させない）
    f.sync_data()
}

pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(STANDARD.encode(h.finalize()))
}

fn sha256_b64(data: &[u8]) -> String {
    STANDARD.encode(Sha256::digest(data))
}

/// 既存ファイルを上書きしないよう "name (1).ext" 形式で空き名を探す
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if !p.exists() {
        return p;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).expect("unique")
}

pub fn guess_mime(name: &str) -> String {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "txt" => "text/plain",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "heic" => "image/heic",
        "tif" | "tiff" => "image/tiff",
        "webp" => "image/webp",
        "mov" => "video/quicktime",
        "mp4" | "m4v" => "video/mp4",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
    .into()
}

async fn with_retry<T, F, Fut>(what: &str, mut f: F) -> Result<T, Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, Error>>,
{
    let mut delay = Duration::from_millis(500);
    for attempt in 1..=MAX_ATTEMPTS {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if e.is_transient() && attempt < MAX_ATTEMPTS => {
                tracing::debug!(what, attempt, error = %e, "retrying");
                tokio::time::sleep(delay + Duration::from_millis(rand::random::<u64>() % 250)).await;
                delay = (delay * 2).min(Duration::from_secs(15));
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

impl Client {
    /// 登録済みプロファイルを開く。ネットワーク接続は `start` まで行わない。
    pub fn open(profile: Profile, secrets: &dyn SecretStore) -> Result<Self, Error> {
        let config = profile.load_config()?.ok_or(Error::NotEnrolled)?;
        let key = profile.load_key(secrets, &config)?;
        let api = Api::new(config.base_url.clone(), config.endpoint_id.clone(), key);
        let db = Db::open(&profile.db_path())?;
        let (events, _) = broadcast::channel(1024);
        let (conn_state, _) = watch::channel(ConnState::Offline);
        let (stop, _) = watch::channel(false);
        Ok(Self {
            inner: Arc::new(Inner {
                profile,
                config: Mutex::new(config),
                api,
                db,
                events,
                up_sem: Semaphore::new(UPLOAD_CONCURRENCY),
                down_sem: Semaphore::new(DOWNLOAD_CONCURRENCY),
                running: Mutex::new(HashSet::new()),
                wakers: Mutex::new(HashMap::new()),
                conn_state,
                stop,
                progress: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ClientEvent> {
        self.inner.events.subscribe()
    }
    pub fn api(&self) -> &Api {
        &self.inner.api
    }
    pub fn config(&self) -> ProfileConfig {
        self.inner.config.lock().expect("lock").clone()
    }
    pub fn profile(&self) -> &Profile {
        &self.inner.profile
    }
    pub fn endpoint_id(&self) -> &str {
        &self.inner.api.endpoint_id
    }
    pub fn connection_state(&self) -> ConnState {
        *self.inner.conn_state.borrow()
    }
    pub fn history(&self, limit: u32) -> Result<Vec<HistoryItem>, Error> {
        self.inner.db.history(limit)
    }
    pub fn item(&self, id: &str) -> Result<Option<HistoryItem>, Error> {
        self.inner.db.get(id)
    }
    pub fn progress(&self, id: &str) -> Option<(u64, u64)> {
        self.inner.progress.lock().expect("lock").get(id).copied()
    }

    pub fn download_dir(&self) -> PathBuf {
        self.config().download_dir.unwrap_or_else(|| self.inner.profile.default_download_dir())
    }

    pub fn set_download_dir(&self, dir: Option<PathBuf>) -> Result<(), Error> {
        let mut c = self.inner.config.lock().expect("lock");
        c.download_dir = dir;
        self.inner.profile.save_config(&c)
    }

    pub async fn rename(&self, name: &str) -> Result<(), Error> {
        let me = self.inner.api.rename(name).await?;
        let mut c = self.inner.config.lock().expect("lock").clone();
        c.name = me.endpoint.name;
        self.inner.profile.save_config(&c)?;
        *self.inner.config.lock().expect("lock") = c;
        Ok(())
    }

    fn emit(&self, e: ClientEvent) {
        let _ = self.inner.events.send(e);
    }

    /// バックグラウンド処理（WebSocket 接続、再同期、中断転送の再開）を開始する
    pub fn start(&self) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let api = self.inner.api.clone();
        let st = self.inner.conn_state.clone();
        let stop = self.inner.stop.subscribe();
        tokio::spawn(ws::run(api, tx, st, stop));

        let mut conn_rx = self.inner.conn_state.subscribe();
        let me = self.clone();
        tokio::spawn(async move {
            while conn_rx.changed().await.is_ok() {
                let state = *conn_rx.borrow();
                me.emit(ClientEvent::Connection { state });
            }
        });

        let me = self.clone();
        tokio::spawn(async move {
            // 通信状態に関わらず、ローカルに残った未完了の送受信を再開する
            me.resume_local();
            while let Some(sig) = rx.recv().await {
                match sig {
                    WsSignal::Connected => {
                        let me2 = me.clone();
                        tokio::spawn(async move { me2.sync().await });
                    }
                    WsSignal::Event(ev) => me.on_server_event(ev),
                    WsSignal::Disconnected => {}
                }
            }
        });

        // 定期再同期: 通知の取りこぼしや、失敗した転送の再試行を拾う保険
        let me = self.clone();
        let mut stop = self.inner.stop.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(120)) => me.sync().await,
                    _ = stop.changed() => return,
                }
            }
        });
    }

    pub fn shutdown(&self) {
        let _ = self.inner.stop.send(true);
    }

    fn on_server_event(&self, ev: ServerEvent) {
        match ev {
            ServerEvent::Presence { .. } | ServerEvent::EndpointsChanged => self.emit(ClientEvent::EndpointsChanged),
            ServerEvent::TransferCreated { transfer } => {
                if transfer.receiver == self.endpoint_id() {
                    self.spawn_download(*transfer);
                }
            }
            ServerEvent::ChunksReady { transfer_id, .. } => self.wake(&transfer_id),
            ServerEvent::TransferState { transfer_id, state } => {
                self.wake(&transfer_id);
                self.on_remote_state(&transfer_id, state);
            }
            ServerEvent::Hello { .. } | ServerEvent::Pong => {}
        }
    }

    fn on_remote_state(&self, id: &str, state: TransferState) {
        let db = &self.inner.db;
        let Ok(Some(item)) = db.get(id) else { return };
        if item.direction != Direction::Outgoing {
            return;
        }
        match state {
            TransferState::Received => {
                let _ = db.set_status(id, LocalStatus::Done, None);
                self.cleanup_outbox(&item);
                self.emit(ClientEvent::Delivered { transfer_id: id.into() });
            }
            TransferState::Cancelled => {
                let _ = db.set_status(id, LocalStatus::Cancelled, None);
                self.cleanup_outbox(&item);
            }
            _ => return,
        }
        self.emit(ClientEvent::TransferUpdated { transfer_id: id.into() });
    }

    /// 送信のためにアプリが書き出した一時ファイルだけを消す（ユーザーのファイルには触れない）
    fn cleanup_outbox(&self, item: &HistoryItem) {
        let outbox = self.inner.profile.outbox_dir();
        for p in &item.paths {
            if p.starts_with(&outbox) {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    fn wake(&self, id: &str) {
        if let Some(n) = self.inner.wakers.lock().expect("lock").get(id) {
            n.notify_one();
        }
    }

    fn waker(&self, id: &str) -> Arc<Notify> {
        self.inner.wakers.lock().expect("lock").entry(id.into()).or_default().clone()
    }

    /// サーバーと状態を突き合わせる（接続確立時・定期）
    pub async fn sync(&self) {
        self.emit(ClientEvent::EndpointsChanged);
        let remote = match self.inner.api.transfers().await {
            Ok(v) => v,
            Err(e) => {
                tracing::info!(error = %e, "sync failed");
                return;
            }
        };
        let me = self.endpoint_id().to_string();
        for t in &remote {
            if t.receiver == me && !t.state.is_terminal() {
                self.spawn_download(t.clone());
            }
        }
        // 送信側: オフライン中に受領/取消されたものを反映し、未完了アップロードを再開する
        if let Ok(outs) = self.inner.db.active(Direction::Outgoing) {
            for t in outs {
                match remote.iter().find(|r| r.transfer_id == t.transfer_id) {
                    Some(r) if r.state == TransferState::Uploading => self.spawn_upload(t),
                    Some(r) => self.on_remote_state(&r.transfer_id, r.state),
                    // 一覧に無い = 期限切れ・削除済み
                    None => {
                        let _ = self.inner.db.set_status(&t.transfer_id, LocalStatus::Failed, Some("expired on server"));
                        self.emit(ClientEvent::TransferUpdated { transfer_id: t.transfer_id });
                    }
                }
            }
        }
    }

    fn resume_local(&self) {
        if let Ok(ins) = self.inner.db.active(Direction::Incoming) {
            for t in ins {
                self.spawn_download(t);
            }
        }
        if let Ok(outs) = self.inner.db.active(Direction::Outgoing) {
            for t in outs {
                if self.inner.db.status(&t.transfer_id).ok().flatten() == Some(LocalStatus::Active) {
                    self.spawn_upload(t);
                }
            }
        }
    }

    fn try_claim(&self, id: &str) -> bool {
        self.inner.running.lock().expect("lock").insert(id.to_string())
    }

    fn release(&self, id: &str) {
        self.inner.running.lock().expect("lock").remove(id);
        self.inner.wakers.lock().expect("lock").remove(id);
    }

    fn set_progress(&self, id: &str, dir: Direction, done: u64, total: u64) {
        self.inner.progress.lock().expect("lock").insert(id.into(), (done, total));
        self.emit(ClientEvent::Progress { transfer_id: id.into(), direction: dir, done_bytes: done, total_bytes: total });
    }

    // ---------------- 送信 ----------------

    /// 小さいテキストは HTTP API に inline、大きいテキストはファイルとして転送する
    pub async fn send_text(&self, receiver: &str, text: &str) -> Result<Transfer, Error> {
        if text.len() <= INLINE_TEXT_MAX_BYTES {
            let t = self
                .inner
                .api
                .create_transfer(&CreateTransferRequest {
                    receiver: receiver.into(),
                    kind: TransferKind::ClipboardText,
                    text: Some(text.into()),
                    files: vec![],
                    chunk_size: None,
                })
                .await?;
            self.inner.db.upsert_transfer(&t, Direction::Outgoing, LocalStatus::Uploaded)?;
            self.inner.db.set_status(&t.transfer_id, LocalStatus::Uploaded, None)?;
            self.emit(ClientEvent::TransferUpdated { transfer_id: t.transfer_id.clone() });
            return Ok(t);
        }
        let dir = self.inner.profile.outbox_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("text-{}.txt", crate::api::now()));
        std::fs::write(&path, text)?;
        let f = OutgoingFile {
            path,
            name: Some("clipboard.txt".into()),
            mime: Some("text/plain; charset=utf-8".into()),
            media: MediaInfo::default(),
        };
        self.send_files(receiver, TransferKind::ClipboardText, vec![f], None).await
    }

    /// ファイル群の転送を作成し、バックグラウンドでアップロードを開始する。
    /// 戻り値の時点では転送は作成済み（受信側は完了チャンクから取得を始められる）。
    pub async fn send_files(
        &self,
        receiver: &str,
        kind: TransferKind,
        files: Vec<OutgoingFile>,
        chunk_size: Option<u64>,
    ) -> Result<Transfer, Error> {
        let mut new_files = Vec::new();
        let mut metas = Vec::new();
        for f in &files {
            let meta = std::fs::metadata(&f.path)?;
            if !meta.is_file() {
                return Err(io_err(format!("not a regular file: {}", f.path.display())));
            }
            let name = f
                .name
                .clone()
                .or_else(|| f.path.file_name().map(|n| n.to_string_lossy().to_string()))
                .ok_or_else(|| io_err("file has no name"))?;
            new_files.push(NewFile {
                mime: f.mime.clone().unwrap_or_else(|| guess_mime(&name)),
                name,
                size: meta.len(),
                media: f.media.clone(),
            });
            metas.push(meta);
        }
        let t = self
            .inner
            .api
            .create_transfer(&CreateTransferRequest {
                receiver: receiver.into(),
                kind,
                text: None,
                files: new_files,
                chunk_size,
            })
            .await?;
        self.inner.db.upsert_transfer(&t, Direction::Outgoing, LocalStatus::Active)?;
        for (i, (f, meta)) in files.iter().zip(&metas).enumerate() {
            self.inner.db.set_file(&t.transfer_id, i as u32, &f.path, mtime_ns(meta), None)?;
        }
        self.emit(ClientEvent::TransferUpdated { transfer_id: t.transfer_id.clone() });
        self.spawn_upload(t.clone());
        Ok(t)
    }

    pub async fn cancel(&self, id: &str) -> Result<(), Error> {
        self.inner.api.cancel(id).await?;
        self.wake(id);
        if let Some(item) = self.inner.db.get(id)? {
            self.inner.db.set_status(id, LocalStatus::Cancelled, None)?;
            if item.direction == Direction::Outgoing {
                self.cleanup_outbox(&item);
            }
        }
        self.emit(ClientEvent::TransferUpdated { transfer_id: id.into() });
        Ok(())
    }

    fn spawn_upload(&self, t: Transfer) {
        if !self.try_claim(&t.transfer_id) {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let id = t.transfer_id.clone();
            let r = me.upload(&t).await;
            me.release(&id);
            match r {
                Ok(()) => {}
                Err(e) if e.is_transient() => {
                    // 通信断などは状態を Active のまま残し、少し後（または再接続時の sync）で再開する
                    tracing::info!(transfer_id = %id, error = %e, "upload interrupted; will retry");
                    let _ = me.inner.db.set_status(&id, LocalStatus::Active, Some(&e.to_string()));
                    me.emit(ClientEvent::TransferUpdated { transfer_id: id.clone() });
                    let me2 = me.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(RETRY_FAILED_TRANSFER_AFTER).await;
                        if me2.inner.db.status(&id).ok().flatten() == Some(LocalStatus::Active) {
                            me2.spawn_upload(t);
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(transfer_id = %id, error = %e, "upload failed");
                    let _ = me.inner.db.set_status(&id, LocalStatus::Failed, Some(&e.to_string()));
                    me.emit(ClientEvent::TransferFailed { transfer_id: id.clone(), message: e.to_string() });
                    me.emit(ClientEvent::TransferUpdated { transfer_id: id });
                }
            }
        });
    }

    async fn upload(&self, t: &Transfer) -> Result<(), Error> {
        let id = t.transfer_id.as_str();
        let api = &self.inner.api;
        let files = self.inner.db.files(id)?;
        if files.len() != t.files.len() {
            return Err(io_err("local transfer record is incomplete"));
        }
        // 再開時に元ファイルが変わっていたら、途中まで送ったデータと混ざるので中止する
        for ((path, mtime, _, _), f) in files.iter().zip(&t.files) {
            let meta = std::fs::metadata(path).map_err(|e| io_err(format!("source file unavailable {}: {e}", path.display())))?;
            if meta.len() != f.size || mtime_ns(&meta) != *mtime {
                return Err(io_err(format!("source file changed since send started: {}", path.display())));
            }
        }
        let detail = api.transfer(id).await?;
        match detail.transfer.state {
            TransferState::Uploading => {}
            TransferState::Uploaded => {
                self.inner.db.set_status(id, LocalStatus::Uploaded, None)?;
                return Ok(());
            }
            s => {
                self.on_remote_state(id, s);
                return Ok(());
            }
        }
        let done: HashSet<(u32, u32)> = detail.chunks.iter().map(|c| (c.file, c.index)).collect();
        let total = t.total_bytes();
        let done_bytes = Arc::new(std::sync::atomic::AtomicU64::new(
            detail.chunks.iter().map(|c| c.size).sum(),
        ));
        self.set_progress(id, Direction::Outgoing, done_bytes.load(std::sync::atomic::Ordering::Relaxed), total);

        // ファイル全体ハッシュはチャンク送信と並行して計算する（開始を遅らせないため）
        let mut hash_tasks = Vec::new();
        for (i, (path, ..)) in files.iter().enumerate() {
            if detail.transfer.files[i].sha256.is_none() {
                let p = path.clone();
                hash_tasks.push((i as u32, tokio::task::spawn_blocking(move || sha256_file(&p))));
            }
        }

        let jobs: Vec<(u32, u32)> = t
            .files
            .iter()
            .flat_map(|f| (0..f.chunk_count).map(move |c| (f.index, c)))
            .filter(|k| !done.contains(k))
            .collect();
        let results: Vec<Result<(), Error>> = futures_util::stream::iter(jobs)
            .map(|(file, index)| {
                let path = files[file as usize].0.clone();
                let done_bytes = done_bytes.clone();
                async move {
                    let _permit = self.inner.up_sem.acquire().await.expect("sem");
                    let offset = index as u64 * t.chunk_size;
                    let len = t.chunk_len(file, index);
                    let r = with_retry("upload chunk", || {
                        let path = path.clone();
                        async move {
                            // リトライ毎に読み直す: 送信中のメモリ破損や途中切断の影響を持ち越さない
                            let data = tokio::task::spawn_blocking(move || read_at(&path, offset, len))
                                .await
                                .map_err(|e| io_err(e.to_string()))??;
                            let info = ChunkInfo { file, index, size: len, sha256: sha256_b64(&data) };
                            let url = api.upload_urls(id, vec![info.clone()]).await?.pop().ok_or_else(|| io_err("no url"))?;
                            api.put_blob(&url, data).await?;
                            api.chunks_complete(id, vec![info]).await
                        }
                    })
                    .await;
                    if r.is_ok() {
                        let d = done_bytes.fetch_add(len, std::sync::atomic::Ordering::Relaxed) + len;
                        self.set_progress(id, Direction::Outgoing, d, total);
                    }
                    r
                }
            })
            .buffer_unordered(UPLOAD_CONCURRENCY)
            .collect()
            .await;
        if let Some(e) = results.into_iter().find_map(Result::err) {
            // 取消済みなら失敗ではなく終了扱い
            if let Ok(d) = api.transfer(id).await
                && d.transfer.state == TransferState::Cancelled
            {
                self.on_remote_state(id, TransferState::Cancelled);
                return Ok(());
            }
            return Err(e);
        }
        for (file, task) in hash_tasks {
            let sha = task.await.map_err(|e| io_err(e.to_string()))??;
            with_retry("finalize", || api.finalize(id, file, &sha)).await?;
        }
        if self.inner.db.status(id)? == Some(LocalStatus::Active) {
            self.inner.db.set_status(id, LocalStatus::Uploaded, None)?;
        }
        self.emit(ClientEvent::TransferUpdated { transfer_id: id.into() });
        Ok(())
    }

    // ---------------- 受信 ----------------

    fn spawn_download(&self, t: Transfer) {
        if !self.try_claim(&t.transfer_id) {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let id = t.transfer_id.clone();
            let r = me.download(t.clone()).await;
            me.release(&id);
            match r {
                Ok(()) => {}
                Err(e) if e.is_transient() => {
                    tracing::info!(transfer_id = %id, error = %e, "download interrupted; will retry");
                    let _ = me.inner.db.set_status(&id, LocalStatus::Active, Some(&e.to_string()));
                    me.emit(ClientEvent::TransferUpdated { transfer_id: id.clone() });
                    let me2 = me.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(RETRY_FAILED_TRANSFER_AFTER).await;
                        if me2.inner.db.status(&id).ok().flatten() == Some(LocalStatus::Active) {
                            me2.spawn_download(t);
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(transfer_id = %id, error = %e, "download failed");
                    let _ = me.inner.db.set_status(&id, LocalStatus::Failed, Some(&e.to_string()));
                    me.emit(ClientEvent::TransferFailed { transfer_id: id.clone(), message: e.to_string() });
                    me.emit(ClientEvent::TransferUpdated { transfer_id: id });
                }
            }
        });
    }

    fn dest_dir(&self, t: &Transfer) -> PathBuf {
        match t.kind {
            TransferKind::Files => self.download_dir(),
            _ => self.inner.profile.received_dir().join(&t.transfer_id),
        }
    }

    /// 初回だけ保存先を決めて part ファイルを確保する
    fn prepare_incoming(&self, t: &Transfer) -> Result<(), Error> {
        let db = &self.inner.db;
        if db.status(&t.transfer_id)?.is_some() {
            return Ok(());
        }
        let dir = self.dest_dir(t);
        std::fs::create_dir_all(&dir)?;
        let short = &t.transfer_id[t.transfer_id.len().saturating_sub(8)..];
        for f in &t.files {
            let final_path = unique_path(&dir, &f.name);
            let part = dir.join(format!(".{}.{short}.tsute-part", f.name));
            let file = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(&part)?;
            // 最終サイズで確保し、各チャンクを offset に直接書く
            file.set_len(f.size)?;
            db.set_file(&t.transfer_id, f.index, &final_path, None, Some(&part))?;
        }
        db.upsert_transfer(t, Direction::Incoming, LocalStatus::Active)?;
        self.emit(ClientEvent::TransferUpdated { transfer_id: t.transfer_id.clone() });
        Ok(())
    }

    async fn download(&self, t: Transfer) -> Result<(), Error> {
        let id = t.transfer_id.clone();
        let api = &self.inner.api;
        let db = &self.inner.db;
        let waker = self.waker(&id);

        if t.text.is_some() {
            // inline テキストはデータ転送不要。記録して受領通知するだけ
            if db.status(&id)?.is_none() {
                db.upsert_transfer(&t, Direction::Incoming, LocalStatus::Active)?;
            }
            if db.status(&id)? != Some(LocalStatus::Done) {
                api.received(&id).await?;
                db.set_status(&id, LocalStatus::Done, None)?;
                self.emit(ClientEvent::TransferUpdated { transfer_id: id.clone() });
                self.emit(ClientEvent::IncomingReady { transfer_id: id });
            }
            return Ok(());
        }

        self.prepare_incoming(&t)?;
        if matches!(db.status(&id)?, Some(LocalStatus::Done | LocalStatus::Cancelled | LocalStatus::Failed)) {
            return Ok(());
        }
        let total = t.total_bytes();
        let mut verify_failures: HashMap<u32, u32> = HashMap::new();
        loop {
            let detail = api.transfer(&id).await?;
            let t = detail.transfer.clone();
            if t.state == TransferState::Cancelled {
                self.discard_parts(&id);
                db.set_status(&id, LocalStatus::Cancelled, None)?;
                self.emit(ClientEvent::TransferUpdated { transfer_id: id.clone() });
                return Ok(());
            }
            let files = db.files(&id)?;
            // part ファイルが消されていたら（ユーザー操作等）そのファイルは最初から取り直す
            for (i, (_, _, part, done)) in files.iter().enumerate() {
                if !*done && part.as_ref().is_some_and(|p| !p.exists()) {
                    let p = part.clone().expect("part");
                    std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(&p)?.set_len(t.files[i].size)?;
                    db.clear_chunks(&id, i as u32)?;
                }
            }
            let local: HashSet<(u32, u32)> = db.chunks(&id)?.into_iter().collect();
            let local_bytes: u64 = local.iter().map(|&(f, c)| t.chunk_len(f, c)).sum();
            self.set_progress(&id, Direction::Incoming, local_bytes, total);
            tracing::debug!(transfer_id = %id, state = ?t.state, remote = detail.chunks.len(), local = local.len(), "download loop");
            let todo: Vec<ChunkInfo> = detail
                .chunks
                .iter()
                .filter(|c| !local.contains(&(c.file, c.index)) && !files[c.file as usize].3)
                .cloned()
                .collect();
            if !todo.is_empty() {
                self.download_chunks(&t, &files, todo, local_bytes).await?;
                continue;
            }
            // 全チャンクが揃ったファイルを検証して確定する
            let mut all_done = true;
            for f in &t.files {
                let (final_path, _, part, done) = &files[f.index as usize];
                if *done {
                    continue;
                }
                let have = local.iter().filter(|(fi, _)| *fi == f.index).count() as u32;
                let (Some(sha), true) = (&f.sha256, have == f.chunk_count) else {
                    all_done = false;
                    continue;
                };
                let part = part.clone().ok_or_else(|| io_err("missing part path"))?;
                let p2 = part.clone();
                let actual = tokio::task::spawn_blocking(move || sha256_file(&p2)).await.map_err(|e| io_err(e.to_string()))??;
                if &actual != sha {
                    let n = verify_failures.entry(f.index).or_default();
                    *n += 1;
                    tracing::warn!(transfer_id = %id, file = f.index, "final checksum mismatch; re-downloading file");
                    if *n >= 2 {
                        return Err(io_err(format!("checksum mismatch for {}", f.name)));
                    }
                    db.clear_chunks(&id, f.index)?;
                    all_done = false;
                    continue;
                }
                // 待機中に同名ファイルができている可能性があるので確定直前に再確認する
                let dest = if final_path.exists() {
                    unique_path(final_path.parent().expect("parent"), &f.name)
                } else {
                    final_path.clone()
                };
                std::fs::rename(&part, &dest)?;
                if dest != *final_path {
                    db.set_file(&id, f.index, &dest, None, None)?;
                }
                db.mark_file_done(&id, f.index)?;
            }
            if all_done {
                if t.state == TransferState::Uploading {
                    // 最後の finalize と状態遷移の間に取得した可能性があるので取り直す
                    continue;
                }
                api.received(&id).await?;
                db.set_status(&id, LocalStatus::Done, None)?;
                self.set_progress(&id, Direction::Incoming, total, total);
                self.emit(ClientEvent::TransferUpdated { transfer_id: id.clone() });
                self.emit(ClientEvent::IncomingReady { transfer_id: id.clone() });
                return Ok(());
            }
            // 送信側の次のチャンクを待つ（通知 or 保険のポーリング）
            let _ = tokio::time::timeout(FALLBACK_POLL, waker.notified()).await;
        }
    }

    async fn download_chunks(
        &self,
        t: &Transfer,
        files: &[(PathBuf, Option<i64>, Option<PathBuf>, bool)],
        todo: Vec<ChunkInfo>,
        base_bytes: u64,
    ) -> Result<(), Error> {
        let id = t.transfer_id.as_str();
        let api = &self.inner.api;
        let total = t.total_bytes();
        let done_bytes = Arc::new(std::sync::atomic::AtomicU64::new(base_bytes));
        for batch in todo.chunks(32) {
            let refs: Vec<ChunkRef> = batch.iter().map(|c| ChunkRef { file: c.file, index: c.index }).collect();
            let urls = with_retry("download urls", || api.download_urls(id, refs.clone())).await?;
            let results: Vec<Result<(), Error>> = futures_util::stream::iter(batch.iter().cloned())
                .map(|c| {
                    let url = urls.iter().find(|u| u.file == c.file && u.index == c.index).map(|u| u.url.clone());
                    let part = files[c.file as usize].2.clone();
                    let done_bytes = done_bytes.clone();
                    async move {
                        let _permit = self.inner.down_sem.acquire().await.expect("sem");
                        let url = url.ok_or_else(|| io_err("missing url"))?;
                        let part = part.ok_or_else(|| io_err("missing part path"))?;
                        let mut attempts = 0;
                        loop {
                            attempts += 1;
                            let data = with_retry("get chunk", || api.get_blob(&url)).await?;
                            // 壊れたチャンクは書き込まずに取り直す（chunk 単位の再取得）
                            if data.len() as u64 != c.size || sha256_b64(&data) != c.sha256 {
                                tracing::warn!(transfer_id = %id, file = c.file, index = c.index, "chunk checksum mismatch");
                                if attempts >= 3 {
                                    return Err(io_err("chunk checksum mismatch"));
                                }
                                continue;
                            }
                            let offset = c.index as u64 * t.chunk_size;
                            let p = part.clone();
                            tokio::task::spawn_blocking(move || write_at(&p, offset, &data))
                                .await
                                .map_err(|e| io_err(e.to_string()))??;
                            self.inner.db.mark_chunk(id, c.file, c.index)?;
                            let d = done_bytes.fetch_add(c.size, std::sync::atomic::Ordering::Relaxed) + c.size;
                            self.set_progress(id, Direction::Incoming, d, total);
                            return Ok(());
                        }
                    }
                })
                .buffer_unordered(DOWNLOAD_CONCURRENCY)
                .collect()
                .await;
            if let Some(e) = results.into_iter().find_map(Result::err) {
                return Err(e);
            }
        }
        Ok(())
    }

    fn discard_parts(&self, id: &str) {
        if let Ok(files) = self.inner.db.files(id) {
            for (_, _, part, done) in files {
                if let (Some(p), false) = (part, done) {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
    }
}
